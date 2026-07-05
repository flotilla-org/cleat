#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

use std::{
    collections::{HashMap, VecDeque},
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use http::StatusCode;

use crate::{
    host::actor::{RawOutputTap, SessionActor, SessionMouseEvent, SessionWheelEvent},
    http_uds,
    packet::{
        Ack, CloseChannel, ControlError, ControlHello, DirectoryEntry, DirectorySnapshot, Input, OpenChannel, PacketFrame, RenderPacket,
        Resize, CHANNEL_CONTROL, MSG_CONTROL_CLOSE_CHANNEL, MSG_CONTROL_DIRECTORY_SNAPSHOT, MSG_CONTROL_ERROR, MSG_CONTROL_HELLO,
        MSG_CONTROL_OPEN_CHANNEL, MSG_SESSION_ACK, MSG_SESSION_INPUT, MSG_SESSION_RENDER, MSG_SESSION_RESIZE,
    },
    platform::{
        daemon::{is_session_daemon_alive, spawn_daemon_process},
        ipc::{
            bind_session_listener, connect_session_stream, session_socket_path as platform_session_socket_path, set_listener_nonblocking,
            set_stream_nonblocking, set_stream_read_timeout, set_stream_write_timeout, shutdown_stream, SessionStream,
        },
        terminal::{attach_signal_exit_requested, current_terminal_size, stdout_is_tty, AttachSignalHandlers, ForegroundTerminal},
    },
    protocol::Frame,
    provider::{
        DirtyState, TerminalInputEvent, TerminalKey, TerminalMouseButton, TerminalMouseEventKind, TerminalNamedKey, TerminalRenderUpdate,
    },
    runtime::{RuntimeLayout, SessionMetadata, TerminalSize},
    vt::{self, ScreenGrid, VtEngine, VtEngineKind},
};

const FOREGROUND_NAME: &str = "foreground";
const DETACH_CLEANUP_SEQUENCE: &[u8] =
    b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1004l\x1b[<u\x1b[r\x1b[0m\x1b[?25h\x1b[2J\x1b[H\x1b[?1049l";
const REATTACH_CLEAR_SEQUENCE: &[u8] = b"\x1b[2J\x1b[H";
const MAX_PENDING_CLIENT_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const SESSION_DAEMON_SERVICING_TICK: Duration = Duration::from_millis(10);
const SESSION_HTTP_HANDSHAKE_DEADLINE: Duration = Duration::from_millis(250);
const SESSION_HTTP_RESPONSE_WRITE_DEADLINE: Duration = Duration::from_millis(250);
const TERMINATE_SIGNAL: i32 = 15;
const SCREEN_STABLE_CHANGED_CELL_TOLERANCE: usize = 16;

#[derive(Debug)]
pub struct ForegroundAttach {
    stream: Arc<Mutex<SessionStream>>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SessionStartOptions {
    pub record: bool,
    pub initial_size: TerminalSize,
    pub colors: vt::TerminalColors,
}

impl ForegroundAttach {
    pub fn relay_stdio(self) -> Result<(), String> {
        let mut cleanup = AttachCleanupGuard::stdout();
        let mut terminal = ForegroundTerminal::enter()?;
        let _signal_handlers = AttachSignalHandlers::install()?;
        let read_handle = {
            let stream = self.stream.lock().map_err(|_| "attach stream lock poisoned".to_string())?;
            stream.try_clone().map_err(|err| format!("clone attach stream: {err}"))?
        };
        let mut read_stream = read_handle;
        let alive = Arc::new(AtomicBool::new(true));
        let alive_out = Arc::clone(&alive);
        let relay_out = thread::spawn(move || -> Result<(), String> {
            let mut stdout = std::io::stdout().lock();
            loop {
                match Frame::read(&mut read_stream) {
                    Ok(Frame::Output(bytes)) => {
                        stdout.write_all(&bytes).map_err(|err| format!("write stdout: {err}"))?;
                        stdout.flush().map_err(|err| format!("flush stdout: {err}"))?;
                    }
                    Ok(_) => {}
                    Err(err) => {
                        alive_out.store(false, Ordering::SeqCst);
                        if is_graceful_socket_shutdown(&err) {
                            return Ok(());
                        }
                        return Err(format!("read attach frame: {err}"));
                    }
                }
            }
        });

        let write_stream = Arc::clone(&self.stream);
        let alive_resize = Arc::clone(&alive);
        let resize_loop = thread::spawn(move || -> Result<(), String> {
            let mut last = current_terminal_size();
            while alive_resize.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(100));
                let next = current_terminal_size();
                if next != last {
                    let mut stream = write_stream.lock().map_err(|_| "attach stream lock poisoned".to_string())?;
                    Frame::Resize { cols: next.0, rows: next.1 }.write(&mut *stream).map_err(|err| format!("write resize frame: {err}"))?;
                    last = next;
                }
            }
            Ok(())
        });

        let mut buf = [0u8; 4096];
        let stdin_result = loop {
            if !alive.load(Ordering::SeqCst) || attach_signal_exit_requested() {
                break Ok(());
            }
            match terminal.read_input(Duration::from_millis(100), &mut buf) {
                Ok(None) => continue,
                Ok(Some(0)) => break Ok(()),
                Ok(Some(n)) => {
                    let mut stream = self.stream.lock().map_err(|_| "attach stream lock poisoned".to_string())?;
                    if let Err(err) = Frame::Input(buf[..n].to_vec()).write(&mut *stream) {
                        if is_graceful_socket_shutdown(&err) {
                            break Ok(());
                        }
                        break Err(format!("write input frame: {err}"));
                    }
                }
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::UnexpectedEof
                            | std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::ConnectionAborted
                    ) =>
                {
                    break Ok(())
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) => break Err(format!("read stdin: {err}")),
            }
        };

        let signal_exit = attach_signal_exit_requested();
        alive.store(false, Ordering::SeqCst);
        if let Ok(stream) = self.stream.lock() {
            shutdown_stream(&stream);
        }
        let out_result = relay_out.join().map_err(|_| "stdout relay thread panicked".to_string())?;
        let resize_result = resize_loop.join().map_err(|_| "resize thread panicked".to_string())?;
        cleanup.emit()?;
        if signal_exit {
            return Ok(());
        }
        stdin_result?;
        out_result?;
        resize_result
    }
}

fn is_graceful_socket_shutdown(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
    )
}

enum AttachCleanupTarget {
    Stdout,
    #[cfg(test)]
    Buffer(Arc<Mutex<Vec<u8>>>),
}

struct AttachCleanupGuard {
    target: AttachCleanupTarget,
    enabled: bool,
    emitted: bool,
}

impl AttachCleanupGuard {
    fn stdout() -> Self {
        Self { target: AttachCleanupTarget::Stdout, enabled: stdout_is_tty().unwrap_or(false), emitted: false }
    }

    #[cfg(test)]
    fn test_buffer(buffer: Arc<Mutex<Vec<u8>>>) -> Self {
        Self { target: AttachCleanupTarget::Buffer(buffer), enabled: true, emitted: false }
    }

    #[cfg(test)]
    fn test_buffer_disabled(buffer: Arc<Mutex<Vec<u8>>>) -> Self {
        Self { target: AttachCleanupTarget::Buffer(buffer), enabled: false, emitted: false }
    }

    fn emit(&mut self) -> Result<(), String> {
        if !self.enabled || self.emitted {
            return Ok(());
        }
        let result = match &self.target {
            AttachCleanupTarget::Stdout => {
                let mut stdout = std::io::stdout().lock();
                write_detach_cleanup(&mut stdout)
            }
            #[cfg(test)]
            AttachCleanupTarget::Buffer(buffer) => {
                if let Ok(mut buffer) = buffer.lock() {
                    write_detach_cleanup(&mut *buffer)
                } else {
                    Err("cleanup buffer lock poisoned".to_string())
                }
            }
        };
        if result.is_ok() {
            self.emitted = true;
        }
        result
    }
}

impl Drop for AttachCleanupGuard {
    fn drop(&mut self) {
        let _ = self.emit();
    }
}

fn write_detach_cleanup<W: Write>(writer: &mut W) -> Result<(), String> {
    writer.write_all(DETACH_CLEANUP_SEQUENCE).map_err(|err| format!("write detach cleanup: {err}"))?;
    writer.flush().map_err(|err| format!("flush detach cleanup: {err}"))
}

pub fn ensure_session_started(
    layout: &RuntimeLayout,
    id: Option<String>,
    vt_engine: Option<VtEngineKind>,
    cwd: Option<PathBuf>,
    cmd: Option<String>,
    options: SessionStartOptions,
) -> Result<SessionMetadata, String> {
    // If a named session directory already exists with a live socket, reuse it.
    if let Some(ref id_str) = id {
        let socket_path = session_socket_path(layout.root(), id_str);
        if socket_path.exists() {
            if is_session_daemon_alive(layout.root(), id_str) {
                // Daemon is running — return the id. The caller should use inspect()
                // if it needs the session's actual config.
                let vt_engine = vt_engine.unwrap_or_else(vt::default_vt_engine_kind);
                return Ok(SessionMetadata {
                    id: id_str.clone(),
                    vt_engine,
                    cwd,
                    cmd,
                    record: options.record,
                    initial_size: options.initial_size,
                    colors: options.colors,
                });
            }
            // Stale socket from a crashed daemon. Remove it so the respawned daemon
            // binds cleanly and wait_for_socket waits for the new listener rather
            // than returning on the leftover file. The session directory and its
            // recording survive, so falling through to the create path recreates
            // the session from its prior output (see recreate::seed_engine_from_cast).
            //
            // A removal failure other than "already gone" (e.g. a permissions
            // problem) would otherwise surface downstream as an opaque socket-bind
            // error (EADDRINUSE), so report the real cause here instead.
            match fs::remove_file(&socket_path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(format!("remove stale session socket {}: {err}", socket_path.display())),
            }
        }
    }

    // Create a new session and spawn the daemon.
    let vt_engine = vt_engine.unwrap_or_else(vt::default_vt_engine_kind);
    vt_engine.ensure_available()?;
    let mut session = layout.create_session(id, vt_engine, cwd, cmd)?;
    session.record = options.record;
    session.initial_size = options.initial_size;
    session.colors = options.colors;

    let socket_path = session_socket_path(layout.root(), &session.id);
    spawn_daemon_process(layout.root(), &session)?;
    wait_for_socket(&socket_path)?;

    Ok(session)
}

pub fn attach_foreground(layout: &RuntimeLayout, id: &str) -> Result<ForegroundAttach, String> {
    connect_foreground_upgrade(layout, id, "attach")
}

pub fn watch_foreground(layout: &RuntimeLayout, id: &str) -> Result<ForegroundAttach, String> {
    connect_foreground_upgrade(layout, id, "watch")
}

fn connect_foreground_upgrade(layout: &RuntimeLayout, id: &str, role: &str) -> Result<ForegroundAttach, String> {
    let socket_path = session_socket_path(layout.root(), id);
    let deadline = Instant::now() + Duration::from_millis(250);
    loop {
        let mut stream = connect_session_stream(&socket_path)?;
        let (cols, rows) = current_terminal_size();
        let body = serde_json::to_vec(&http_uds::AttachRequest {
            cols,
            rows,
            capabilities: attach_capabilities_to_http(attach_init_capabilities()),
        })
        .map_err(|err| format!("serialize attach request: {err}"))?;
        http_uds::write_attach_upgrade_request(&mut stream, &format!("/sessions/{id}/{role}"), &body)
            .map_err(|err| format!("write {role} upgrade request: {err}"))?;
        let response = http_uds::read_response_head(&mut stream).map_err(|err| format!("read {role} upgrade response: {err}"))?;
        match response.status {
            StatusCode::SWITCHING_PROTOCOLS => return Ok(ForegroundAttach { stream: Arc::new(Mutex::new(stream)) }),
            StatusCode::CONFLICT => {}
            other => return Err(format!("unexpected {role} response: {other}")),
        }
        if Instant::now() >= deadline {
            return Err(format!("session {id} already has a foreground client"));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

pub fn session_socket_path(root: &Path, id: &str) -> PathBuf {
    platform_session_socket_path(root, id)
}

pub fn daemon_pid_path(root: &Path, id: &str) -> PathBuf {
    crate::platform::daemon::daemon_pid_path(root, id)
}

pub fn foreground_path(root: &Path, id: &str) -> PathBuf {
    root.join(id).join(FOREGROUND_NAME)
}

fn default_vt_engine(session: &SessionMetadata) -> Result<Box<dyn VtEngine>, String> {
    #[cfg(test)]
    if session.vt_engine == VtEngineKind::Ghostty {
        return Ok(Box::new(TestReplayProbeVtEngine::new(session.initial_size.cols, session.initial_size.rows)));
    }

    if std::env::var_os("CARGO_BIN_EXE_cleat").is_some()
        && std::env::var_os("CLEAT_TEST_VT_ENGINE").as_deref() == Some(std::ffi::OsStr::new("replay-probe"))
    {
        return Ok(Box::new(TestReplayProbeVtEngine::new(session.initial_size.cols, session.initial_size.rows)));
    }
    vt::make_vt_engine_with_colors(session.vt_engine, session.initial_size.cols, session.initial_size.rows, session.colors)
}

#[derive(Debug)]
struct TestReplayProbeVtEngine {
    cols: u16,
    rows: u16,
}

impl TestReplayProbeVtEngine {
    fn new(cols: u16, rows: u16) -> Self {
        Self { cols, rows }
    }
}

impl VtEngine for TestReplayProbeVtEngine {
    fn feed(&mut self, _bytes: &[u8]) -> Result<(), String> {
        Ok(())
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<(), String> {
        self.cols = cols;
        self.rows = rows;
        Ok(())
    }

    fn supports_replay(&self) -> bool {
        true
    }

    fn replay_payload(&self, capabilities: &vt::ClientCapabilities) -> Result<Option<Vec<u8>>, String> {
        let payload = format!("{:?}:{}", capabilities.color_level, capabilities.kitty_keyboard);
        Ok(Some(payload.into_bytes()))
    }

    fn screen_text(&self) -> Result<String, String> {
        Ok(format!("probe:{}x{}", self.cols, self.rows))
    }

    fn screen_grid(&mut self) -> Result<ScreenGrid, String> {
        Ok(ScreenGrid::default())
    }

    fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }
}

#[cfg(test)]
fn record_pty_output(engine: &mut dyn VtEngine, bytes: &[u8]) -> Result<(), String> {
    engine.feed(bytes)
}

fn attach_init_capabilities() -> vt::ClientCapabilities {
    vt::ClientCapabilities::conservative_fallback()
}

fn attach_capabilities_to_http(capabilities: vt::ClientCapabilities) -> http_uds::AttachCapabilitiesRequest {
    http_uds::AttachCapabilitiesRequest {
        color_level: match capabilities.color_level {
            vt::ColorLevel::Sixteen => http_uds::AttachColorLevelRequest::Sixteen,
            vt::ColorLevel::Ansi256 => http_uds::AttachColorLevelRequest::Ansi256,
            vt::ColorLevel::TrueColor => http_uds::AttachColorLevelRequest::TrueColor,
        },
        kitty_keyboard: capabilities.kitty_keyboard,
    }
}

fn attach_capabilities_from_http(capabilities: http_uds::AttachCapabilitiesRequest) -> vt::ClientCapabilities {
    let color_level = match capabilities.color_level {
        http_uds::AttachColorLevelRequest::Sixteen => vt::ColorLevel::Sixteen,
        http_uds::AttachColorLevelRequest::Ansi256 => vt::ColorLevel::Ansi256,
        http_uds::AttachColorLevelRequest::TrueColor => vt::ColorLevel::TrueColor,
    };
    vt::ClientCapabilities::new(color_level, capabilities.kitty_keyboard)
}

#[cfg(test)]
fn apply_attach_state(
    engine: &mut dyn VtEngine,
    cols: u16,
    rows: u16,
    capabilities: &vt::ClientCapabilities,
) -> Result<Option<Vec<u8>>, String> {
    engine.resize(cols, rows)?;
    if engine.supports_replay() {
        engine.replay_payload(capabilities)
    } else {
        Ok(None)
    }
}

struct PendingWait {
    stream: SessionStream,
    conditions: Vec<crate::protocol::WaitCondition>,
    screen_stable: Option<ScreenStableState>,
    timeout_ms: u64,
    registered_at: Instant,
}

#[derive(Clone, Debug, PartialEq)]
struct ScreenStableFingerprint {
    cols: u16,
    rows: u16,
    viewport_kind: crate::provider::TerminalViewportKind,
    scrollback_offset_rows: u64,
    cells: Vec<crate::provider::TerminalCell>,
}

impl ScreenStableFingerprint {
    fn from_snapshot(snapshot: crate::provider::TerminalSnapshot) -> Self {
        Self {
            cols: snapshot.cols,
            rows: snapshot.rows,
            viewport_kind: snapshot.viewport_kind,
            scrollback_offset_rows: snapshot.scrollback_offset_rows,
            cells: snapshot.cells,
        }
    }

    fn significant_change_from(&self, other: &Self) -> bool {
        if self.cols != other.cols
            || self.rows != other.rows
            || self.viewport_kind != other.viewport_kind
            || self.scrollback_offset_rows != other.scrollback_offset_rows
            || self.cells.len() != other.cells.len()
        {
            return true;
        }

        self.cells.iter().zip(&other.cells).filter(|(left, right)| left != right).count() > SCREEN_STABLE_CHANGED_CELL_TOLERANCE
    }
}

#[derive(Clone, Debug)]
struct ScreenStableState {
    fingerprint: ScreenStableFingerprint,
    stable_since: Instant,
}

impl ScreenStableState {
    fn new(fingerprint: ScreenStableFingerprint, stable_since: Instant) -> Self {
        Self { fingerprint, stable_since }
    }

    fn observe(&mut self, fingerprint: ScreenStableFingerprint, observed_at: Instant) {
        if self.fingerprint.significant_change_from(&fingerprint) {
            self.fingerprint = fingerprint;
            self.stable_since = observed_at;
        }
    }
}

struct PendingExpect {
    stream: SessionStream,
    text: String,
    since_offset: u64,
    last_checked_file_size: u64,
    timeout_ms: u64,
    registered_at: Instant,
}

fn write_http_wait_result(stream: &mut SessionStream, status: crate::protocol::WaitStatus, elapsed_ms: u64) -> std::io::Result<()> {
    http_uds::write_json(stream, StatusCode::OK, &http_uds::WaitResultResponse { status: wait_status_to_http(status), elapsed_ms })
}

fn enqueue_output_chunk(
    root: &Path,
    id: &str,
    actor: &SessionActor,
    active_client: &mut Option<ActiveClient>,
    watchers: &mut Vec<ActiveClient>,
    chunk: Vec<u8>,
) {
    let frame = Frame::Output(chunk);
    if let Some(client) = active_client.as_mut() {
        if client.enqueue_frame(&frame).is_err() {
            let _ = fs::remove_file(foreground_path(root, id));
            let _ = actor.record_detach();
            let _ = actor.set_client_presence(false);
            *active_client = None;
        }
    }
    watchers.retain_mut(|watcher| watcher.enqueue_frame(&frame).is_ok());
}

fn drain_raw_output_tap(
    root: &Path,
    id: &str,
    actor: &SessionActor,
    raw_output_tap: &mut RawOutputTap,
    active_client: &mut Option<ActiveClient>,
    watchers: &mut Vec<ActiveClient>,
) -> Result<bool, String> {
    let mut drained = false;
    loop {
        match raw_output_tap.try_recv() {
            Ok(chunk) => {
                drained = true;
                enqueue_output_chunk(root, id, actor, active_client, watchers, chunk);
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => return Ok(drained),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                *raw_output_tap = actor.subscribe_raw_output()?;
                return Ok(drained);
            }
        }
    }
}

fn drain_watcher_inputs(watchers: &mut Vec<ActiveClient>) {
    let mut ignored = VecDeque::new();
    watchers.retain_mut(|watcher| {
        ignored.clear();
        watcher.drain_input_frames(&mut ignored, Duration::ZERO).unwrap_or_default()
    });
}

fn flush_watchers(watchers: &mut Vec<ActiveClient>) {
    watchers.retain_mut(|watcher| watcher.flush_pending_output().unwrap_or(false));
}

#[cfg(any(unix, windows))]
pub fn run_session_daemon(root: &Path, session: &SessionMetadata) -> Result<(), String> {
    let id = &session.id;
    let session_dir = root.join(id);
    fs::create_dir_all(&session_dir).map_err(|err| format!("create session dir {}: {err}", session_dir.display()))?;
    let socket_path = session_socket_path(root, id);
    if socket_path.exists() {
        let _ = fs::remove_file(&socket_path);
    }

    let listener = bind_session_listener(&socket_path)?;
    set_listener_nonblocking(&listener, true)?;
    fs::write(daemon_pid_path(root, id), std::process::id().to_string()).map_err(|err| format!("write daemon pid: {err}"))?;

    let actor_session_dir = session_dir.clone();
    let actor_session = session.clone();
    let actor = SessionActor::spawn(session.initial_size.rows, Arc::new(|| {}), move || {
        crate::session_runtime::SessionRuntime::spawn(actor_session_dir, &actor_session, default_vt_engine(&actor_session)?)
    })?;
    let mut raw_output_tap = actor.subscribe_raw_output()?;
    let mut active_client: Option<ActiveClient> = None;
    let mut watchers: Vec<ActiveClient> = Vec::new();
    let mut packet_clients: Vec<PacketClient> = Vec::new();
    let mut packet_render_cache = PacketRenderCache::default();
    let mut had_foreground_client = false;
    let mut pending_waits: Vec<PendingWait> = Vec::new();
    let mut pending_expects: Vec<PendingExpect> = Vec::new();
    let mut should_keep_session_dir = session.record;
    loop {
        let mut did_work = false;

        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    did_work = true;
                    // Accepted sockets inherit nonblocking mode from the listener on macOS/BSD.
                    // Reset to blocking so the initial frame read works correctly.
                    #[cfg(unix)]
                    {
                        set_stream_nonblocking(&stream, false).map_err(|err| format!("set accepted stream blocking: {err}"))?;
                    }
                    #[cfg(windows)]
                    {
                        set_stream_nonblocking(&stream, true).map_err(|err| format!("set accepted stream nonblocking: {err}"))?;
                    }
                    set_stream_write_timeout(&stream, Some(SESSION_HTTP_RESPONSE_WRITE_DEADLINE))?;
                    let request = {
                        let mut reader = HttpHandshakeReader::new(&mut stream, SESSION_HTTP_HANDSHAKE_DEADLINE);
                        let mut prefix = [0; 5];
                        if let Err(err) = reader.read_exact(&mut prefix) {
                            let _ = Frame::Error(format!("failed to read request: {err}")).write(&mut stream);
                            continue;
                        }
                        if !http_uds::looks_like_http_prefix(&prefix) {
                            let _ = Frame::Error("session daemon requires HTTP requests".to_string()).write(&mut stream);
                            continue;
                        }
                        match http_uds::read_request_with_prefix(&mut reader, &prefix) {
                            Ok(request) => request,
                            Err(err) => {
                                let _ = http_uds::write_error(
                                    &mut stream,
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    &format!("read HTTP request: {err}"),
                                );
                                continue;
                            }
                        }
                    };

                    let mut http_state = HttpRequestState {
                        actor: &actor,
                        active_client: &mut active_client,
                        watchers: &mut watchers,
                        packet_clients: &mut packet_clients,
                        had_foreground_client: &mut had_foreground_client,
                        pending_waits: &mut pending_waits,
                        pending_expects: &mut pending_expects,
                    };
                    if let Err(err) = handle_http_request(root, id, &mut stream, request, &mut http_state) {
                        let _ = http_uds::write_error(&mut stream, StatusCode::INTERNAL_SERVER_ERROR, &err);
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) => return Err(format!("accept client: {err}")),
            }
        }

        did_work |= drain_raw_output_tap(root, id, &actor, &mut raw_output_tap, &mut active_client, &mut watchers)?;
        drain_watcher_inputs(&mut watchers);
        did_work |= service_packet_clients(&actor, id, &mut packet_clients, &mut packet_render_cache)?;

        if active_client.is_some() {
            let mut client_disconnected = false;
            let mut pending = VecDeque::new();
            if let Some(client) = active_client.as_mut() {
                match client.drain_input_frames(&mut pending, Duration::ZERO) {
                    Ok(true) => {}
                    Ok(false) => client_disconnected = true,
                    Err(err) => return Err(format!("read client frame: {err}")),
                }
            }

            while let Some(frame) = pending.pop_front() {
                did_work = true;
                match frame {
                    Frame::Input(bytes) => {
                        actor.write_input(bytes)?;
                    }
                    Frame::Resize { cols, rows } => {
                        actor.resize(cols, rows)?;
                    }
                    _ => {}
                }
            }

            if client_disconnected && active_client.is_some() {
                let _ = fs::remove_file(foreground_path(root, id));
                actor.record_detach()?;
                actor.set_client_presence(false)?;
                active_client = None;
                did_work = true;
            }
        }

        let client_writable = match active_client.as_mut() {
            Some(client) => client.flush_pending_output()?,
            None => true,
        };
        if !client_writable {
            let _ = fs::remove_file(foreground_path(root, id));
            actor.record_detach()?;
            actor.set_client_presence(false)?;
            active_client = None;
            did_work = true;
        }
        flush_watchers(&mut watchers);
        push_due_packet_renders(&actor, &mut packet_clients, &mut packet_render_cache)?;
        flush_packet_clients(&mut packet_clients);

        // Evaluate pending waits on each loop tick.
        // Write errors are intentionally discarded — the client may have disconnected.
        let screen_stable_fingerprint = if pending_waits.iter().any(|wait| wait.screen_stable.is_some()) {
            actor.full_snapshot().ok().map(ScreenStableFingerprint::from_snapshot)
        } else {
            None
        };
        let now = Instant::now();
        pending_waits.retain_mut(|wait| {
            let elapsed = wait.registered_at.elapsed();
            let elapsed_ms = elapsed.as_millis() as u64;

            // Check timeout first
            if elapsed_ms >= wait.timeout_ms {
                let _ = write_http_wait_result(&mut wait.stream, crate::protocol::WaitStatus::Timeout, elapsed_ms);
                return false;
            }

            if let (Some(state), Some(fingerprint)) = (wait.screen_stable.as_mut(), screen_stable_fingerprint.as_ref()) {
                state.observe(fingerprint.clone(), now);
            }

            // Check conditions (OR semantics — any match wins)
            for condition in &wait.conditions {
                match condition {
                    crate::protocol::WaitCondition::OutputIdle { quiet_ms } => {
                        // Silence measured from max(registration_time, last_output_time)
                        let silence_since = match actor.last_pty_output_at().ok().flatten() {
                            Some(t) if t > wait.registered_at => t,
                            _ => wait.registered_at,
                        };
                        let quiet_duration = silence_since.elapsed().as_millis() as u64;
                        if quiet_duration >= *quiet_ms {
                            let _ = write_http_wait_result(&mut wait.stream, crate::protocol::WaitStatus::Ready, elapsed_ms);
                            return false;
                        }
                    }
                    crate::protocol::WaitCondition::TextMatch { text } => {
                        if actor.screen_contains(text.clone()).unwrap_or(false) {
                            let _ = write_http_wait_result(&mut wait.stream, crate::protocol::WaitStatus::Ready, elapsed_ms);
                            return false;
                        }
                    }
                    crate::protocol::WaitCondition::ScreenStable { stable_ms } => {
                        if let Some(state) = wait.screen_stable.as_ref() {
                            let stable_duration_ms = state.stable_since.elapsed().as_millis() as u64;
                            if stable_duration_ms >= *stable_ms {
                                let _ = write_http_wait_result(&mut wait.stream, crate::protocol::WaitStatus::Ready, elapsed_ms);
                                return false;
                            }
                        }
                    }
                }
            }

            true // keep waiting
        });

        // Evaluate pending expects by scanning the cast file for text matches.
        if !pending_expects.is_empty() {
            actor.flush_recording()?;
            let cast_path = root.join(id).join(crate::recording::CAST_FILE_NAME);
            pending_expects.retain_mut(|expect| {
                let elapsed = expect.registered_at.elapsed();
                let elapsed_ms = elapsed.as_millis() as u64;

                if elapsed_ms >= expect.timeout_ms {
                    let _ = write_http_wait_result(&mut expect.stream, crate::protocol::WaitStatus::Timeout, elapsed_ms);
                    return false;
                }

                if cast_path.exists() {
                    let file_size = std::fs::metadata(&cast_path).map(|m| m.len()).unwrap_or(0);
                    if file_size > expect.last_checked_file_size {
                        expect.last_checked_file_size = file_size;
                        if let Ok(events) = crate::cast_reader::read_output_since(&cast_path, expect.since_offset) {
                            let output: String = events.iter().map(|e| e.data.as_str()).collect();
                            if output.contains(&expect.text) {
                                let _ = write_http_wait_result(&mut expect.stream, crate::protocol::WaitStatus::Ready, elapsed_ms);
                                return false;
                            }
                        }
                    }
                }

                true
            });
        }
        actor.flush_recording()?;

        if actor.exit_code()?.is_some() {
            drain_raw_output_tap(root, id, &actor, &mut raw_output_tap, &mut active_client, &mut watchers)?;
            for mut wait in pending_waits.drain(..) {
                let elapsed_ms = wait.registered_at.elapsed().as_millis() as u64;
                let _ = write_http_wait_result(&mut wait.stream, crate::protocol::WaitStatus::SessionGone, elapsed_ms);
            }
            for mut expect in pending_expects.drain(..) {
                let elapsed_ms = expect.registered_at.elapsed().as_millis() as u64;
                let _ = write_http_wait_result(&mut expect.stream, crate::protocol::WaitStatus::SessionGone, elapsed_ms);
            }
            if let Some(client) = active_client.as_mut() {
                let _ = client.flush_pending_output();
            }
            flush_watchers(&mut watchers);
            flush_packet_clients(&mut packet_clients);
            should_keep_session_dir = actor.should_keep_session_dir().unwrap_or(should_keep_session_dir);
            break;
        }

        if !did_work {
            thread::sleep(SESSION_DAEMON_SERVICING_TICK);
        }
    }

    let session_dir = root.join(id);
    let _ = fs::remove_file(&socket_path);
    let _ = fs::remove_file(daemon_pid_path(root, id));
    let _ = fs::remove_file(foreground_path(root, id));
    if !should_keep_session_dir {
        let _ = fs::remove_dir_all(&session_dir);
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
pub fn run_session_daemon(_root: &Path, _session: &SessionMetadata) -> Result<(), String> {
    Err("session daemon is only supported on unix".into())
}

struct HttpRequestState<'a> {
    actor: &'a SessionActor,
    active_client: &'a mut Option<ActiveClient>,
    watchers: &'a mut Vec<ActiveClient>,
    packet_clients: &'a mut Vec<PacketClient>,
    had_foreground_client: &'a mut bool,
    pending_waits: &'a mut Vec<PendingWait>,
    pending_expects: &'a mut Vec<PendingExpect>,
}

struct HttpHandshakeReader<'a> {
    stream: &'a mut SessionStream,
    deadline: Instant,
}

impl<'a> HttpHandshakeReader<'a> {
    fn new(stream: &'a mut SessionStream, budget: Duration) -> Self {
        Self { stream, deadline: Instant::now() + budget }
    }

    fn remaining_budget(&self) -> io::Result<Duration> {
        let Some(remaining) = self.deadline.checked_duration_since(Instant::now()) else {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "HTTP request handshake deadline exceeded"));
        };
        if remaining.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "HTTP request handshake deadline exceeded"));
        }
        Ok(remaining)
    }
}

impl Read for HttpHandshakeReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let remaining = self.remaining_budget()?;
        set_stream_read_timeout(self.stream, Some(remaining)).map_err(io::Error::other)?;
        self.stream.read(buf)
    }
}

fn handle_http_request(
    root: &Path,
    daemon_id: &str,
    stream: &mut SessionStream,
    request: http_uds::HttpRequest,
    state: &mut HttpRequestState<'_>,
) -> Result<(), String> {
    match http_uds::route(&request) {
        http_uds::Route::Root | http_uds::Route::Health => http_uds::write_json(
            stream,
            StatusCode::OK,
            &serde_json::json!({
                "service": "cleat-session",
                "session": daemon_id,
                "ok": true,
            }),
        )
        .map_err(|err| format!("write HTTP response: {err}")),
        http_uds::Route::Sessions => {
            let result = state.actor.inspect(state.active_client.is_some(), state.watchers.len())?;
            http_uds::write_json(stream, StatusCode::OK, &http_uds::SessionListResponse { sessions: vec![result] })
                .map_err(|err| format!("write HTTP sessions response: {err}"))
        }
        http_uds::Route::PacketConnect => {
            if !http_uds::request_has_upgrade_token(&request, "cleat-packet/1") {
                http_uds::write_error(stream, StatusCode::BAD_REQUEST, "missing Upgrade: cleat-packet/1")
                    .map_err(|err| format!("write HTTP packet upgrade error: {err}"))?;
                return Ok(());
            }

            let inspect = state.actor.inspect(state.active_client.is_some(), state.watchers.len())?;
            http_uds::write_packet_switching_protocols(stream).map_err(|err| format!("write HTTP packet upgrade response: {err}"))?;
            let packet_stream = stream.try_clone().map_err(|err| format!("clone HTTP packet stream: {err}"))?;
            #[cfg(unix)]
            set_stream_nonblocking(&packet_stream, true).map_err(|err| format!("set HTTP packet stream nonblocking: {err}"))?;
            let mut client = PacketClient::new(packet_stream)?;
            client.enqueue_control(MSG_CONTROL_HELLO, &ControlHello::current())?;
            client.enqueue_control(MSG_CONTROL_DIRECTORY_SNAPSHOT, &DirectorySnapshot {
                sessions: vec![DirectoryEntry { session_id: inspect.session.id, cols: inspect.terminal.cols, rows: inspect.terminal.rows }],
            })?;
            state.packet_clients.push(client);
            Ok(())
        }
        http_uds::Route::SessionInspect { id } if id == daemon_id => {
            let result = state.actor.inspect(state.active_client.is_some(), state.watchers.len())?;
            http_uds::write_json(stream, StatusCode::OK, &result).map_err(|err| format!("write HTTP inspect response: {err}"))
        }
        http_uds::Route::SessionDelete { id } if id == daemon_id => {
            state.actor.dispatch_signal(TERMINATE_SIGNAL, crate::protocol::SignalTarget::Leader)?;
            http_uds::write_no_content(stream).map_err(|err| format!("write HTTP delete response: {err}"))
        }
        http_uds::Route::SessionAttach { id } if id == daemon_id => 'attach: {
            let body: http_uds::AttachRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP attach request: {err}"))?;
            if state.active_client.is_some() {
                http_uds::write_error(stream, StatusCode::CONFLICT, "session already has a foreground client")
                    .map_err(|err| format!("write HTTP attach busy response: {err}"))?;
                break 'attach Ok(());
            }

            let capabilities = attach_capabilities_from_http(body.capabilities);
            let replay = state.actor.apply_attach_state(body.cols, body.rows, capabilities)?;
            http_uds::write_switching_protocols(stream).map_err(|err| format!("write HTTP attach upgrade response: {err}"))?;
            let attach_stream = stream.try_clone().map_err(|err| format!("clone HTTP attach stream: {err}"))?;
            #[cfg(unix)]
            set_stream_nonblocking(&attach_stream, true).map_err(|err| format!("set HTTP attach stream nonblocking: {err}"))?;
            let mut client = ActiveClient::new(attach_stream)?;
            if let Some(payload) = replay {
                if !payload.is_empty() {
                    if *state.had_foreground_client {
                        client.enqueue_frame(&Frame::Output(REATTACH_CLEAR_SEQUENCE.to_vec()))?;
                    }
                    client.enqueue_frame(&Frame::Output(payload))?;
                }
            }
            let _ = fs::write(foreground_path(root, daemon_id), b"1");
            *state.active_client = Some(client);
            *state.had_foreground_client = true;
            state.actor.set_client_presence(true)?;
            state.actor.record_attach()?;
            Ok(())
        }
        http_uds::Route::SessionWatch { id } if id == daemon_id => {
            let body: http_uds::AttachRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP watch request: {err}"))?;
            let capabilities = attach_capabilities_from_http(body.capabilities);
            let replay = state.actor.replay_payload(capabilities)?;
            http_uds::write_switching_protocols(stream).map_err(|err| format!("write HTTP watch upgrade response: {err}"))?;
            let watch_stream = stream.try_clone().map_err(|err| format!("clone HTTP watch stream: {err}"))?;
            #[cfg(unix)]
            set_stream_nonblocking(&watch_stream, true).map_err(|err| format!("set HTTP watch stream nonblocking: {err}"))?;
            let mut watcher = ActiveClient::new(watch_stream)?;
            if let Some(payload) = replay {
                if !payload.is_empty() {
                    watcher.enqueue_frame(&Frame::Output(payload))?;
                }
            }
            state.watchers.push(watcher);
            Ok(())
        }
        http_uds::Route::SessionDetach { id } if id == daemon_id => {
            let _ = fs::remove_file(foreground_path(root, daemon_id));
            if state.active_client.is_some() {
                state.actor.record_detach()?;
            }
            state.actor.set_client_presence(false)?;
            *state.active_client = None;
            http_uds::write_no_content(stream).map_err(|err| format!("write HTTP detach response: {err}"))
        }
        http_uds::Route::SessionExpect { id } if id == daemon_id => 'expect: {
            let body: http_uds::ExpectRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP expect request: {err}"))?;
            if !state.actor.recording_active()? {
                http_uds::write_error(stream, StatusCode::CONFLICT, "recording not active")
                    .map_err(|err| format!("write HTTP expect error: {err}"))?;
                break 'expect Ok(());
            }

            let cast_path = root.join(daemon_id).join(crate::recording::CAST_FILE_NAME);
            state.actor.flush_recording()?;
            if cast_path.exists() {
                if let Ok(events) = crate::cast_reader::read_output_since(&cast_path, body.since_offset) {
                    let output: String = events.iter().map(|event| event.data.as_str()).collect();
                    if output.contains(&body.text) {
                        http_uds::write_json(stream, StatusCode::OK, &http_uds::WaitResultResponse {
                            status: http_uds::WaitStatusResponse::Ready,
                            elapsed_ms: 0,
                        })
                        .map_err(|err| format!("write HTTP expect response: {err}"))?;
                        break 'expect Ok(());
                    }
                }
            }

            let pending_stream = stream.try_clone().map_err(|err| format!("clone HTTP expect stream: {err}"))?;
            if let Err(err) = set_stream_nonblocking(&pending_stream, true) {
                http_uds::write_error(stream, StatusCode::INTERNAL_SERVER_ERROR, &format!("set nonblocking: {err}"))
                    .map_err(|err| format!("write HTTP expect error: {err}"))?;
                break 'expect Ok(());
            }
            let initial_file_size = std::fs::metadata(&cast_path).map(|metadata| metadata.len()).unwrap_or(0);
            state.pending_expects.push(PendingExpect {
                stream: pending_stream,
                text: body.text,
                since_offset: body.since_offset,
                last_checked_file_size: initial_file_size,
                timeout_ms: body.timeout_ms,
                registered_at: Instant::now(),
            });
            Ok(())
        }
        http_uds::Route::SessionInput { id } if id == daemon_id => {
            let body: http_uds::InputRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP input request: {err}"))?;
            match body {
                http_uds::InputRequest::Text { text } => {
                    state.actor.write_input(text.into_bytes())?;
                }
                http_uds::InputRequest::Paste { text } => {
                    state.actor.paste(text.into_bytes())?;
                }
                http_uds::InputRequest::Key { key } => {
                    let bytes = http_input_key_bytes(key);
                    state.actor.write_input(bytes)?;
                }
                http_uds::InputRequest::RawBytes { bytes } => {
                    state.actor.write_input(bytes)?;
                }
                http_uds::InputRequest::Resize { cols, rows } => {
                    state.actor.resize(cols, rows)?;
                }
            }
            http_uds::write_no_content(stream).map_err(|err| format!("write HTTP input response: {err}"))
        }
        http_uds::Route::SessionKeys { id } if id == daemon_id => {
            let body: http_uds::KeysRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP keys request: {err}"))?;
            state.actor.write_input(body.bytes)?;
            http_uds::write_no_content(stream).map_err(|err| format!("write HTTP keys response: {err}"))
        }
        http_uds::Route::SessionKeysWithMark { id } if id == daemon_id => {
            let body: http_uds::KeysWithMarkRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP keys-with-mark request: {err}"))?;
            let offset = state.actor.write_input_with_mark(body.bytes, body.marker_name)?;
            http_uds::write_json(stream, StatusCode::OK, &http_uds::MarkResponse { offset })
                .map_err(|err| format!("write HTTP keys-with-mark response: {err}"))
        }
        http_uds::Route::SessionPasteWithMark { id } if id == daemon_id => {
            let body: http_uds::PasteWithMarkRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP paste-with-mark request: {err}"))?;
            let offset = state.actor.paste_with_mark(body.text.into_bytes(), body.marker_name)?;
            http_uds::write_json(stream, StatusCode::OK, &http_uds::MarkResponse { offset })
                .map_err(|err| format!("write HTTP paste-with-mark response: {err}"))
        }
        http_uds::Route::SessionRecord { id } if id == daemon_id => {
            let body: http_uds::RecordRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP record request: {err}"))?;
            state.actor.set_recording(body.enable)?;
            http_uds::write_no_content(stream).map_err(|err| format!("write HTTP record response: {err}"))
        }
        http_uds::Route::SessionMark { id } if id == daemon_id => {
            let body: http_uds::MarkRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP mark request: {err}"))?;
            let offset = state.actor.mark(body.name)?;
            http_uds::write_json(stream, StatusCode::OK, &http_uds::MarkResponse { offset })
                .map_err(|err| format!("write HTTP mark response: {err}"))
        }
        http_uds::Route::SessionResolveMarker { id } if id == daemon_id => {
            let body: http_uds::ResolveMarkerRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP resolve-marker request: {err}"))?;
            let marker_name = body.name;
            match state.actor.resolve_marker(marker_name.clone())? {
                Some(offset) => http_uds::write_json(stream, StatusCode::OK, &http_uds::MarkResponse { offset })
                    .map_err(|err| format!("write HTTP resolve-marker response: {err}")),
                None => http_uds::write_error(stream, StatusCode::NOT_FOUND, &format!("marker not found: {marker_name}"))
                    .map_err(|err| format!("write HTTP resolve-marker error: {err}")),
            }
        }
        http_uds::Route::SessionResolveNextMarker { id } if id == daemon_id => {
            let body: http_uds::ResolveNextMarkerRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP resolve-next-marker request: {err}"))?;
            let offset = state.actor.resolve_next_marker_after(body.after)?;
            http_uds::write_json(stream, StatusCode::OK, &http_uds::ResolveNextMarkerResponse { offset })
                .map_err(|err| format!("write HTTP resolve-next-marker response: {err}"))
        }
        http_uds::Route::SessionResize { id } if id == daemon_id => {
            let body: http_uds::ResizeRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP resize request: {err}"))?;
            state.actor.resize(body.cols, body.rows)?;
            http_uds::write_no_content(stream).map_err(|err| format!("write HTTP resize response: {err}"))
        }
        http_uds::Route::SessionScreen { id } if id == daemon_id => match state.actor.capture_text() {
            Ok(text) => http_uds::write_json(stream, StatusCode::OK, &http_uds::ScreenResponse { text })
                .map_err(|err| format!("write HTTP screen response: {err}")),
            Err(err) => http_uds::write_error(stream, StatusCode::CONFLICT, &err).map_err(|err| format!("write HTTP screen error: {err}")),
        },
        http_uds::Route::SessionSignal { id } if id == daemon_id => {
            let body: http_uds::SignalRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP signal request: {err}"))?;
            state.actor.dispatch_signal(body.signal, signal_target_from_http(body.target))?;
            http_uds::write_no_content(stream).map_err(|err| format!("write HTTP signal response: {err}"))
        }
        http_uds::Route::SessionSnapshot { id } if id == daemon_id => match state.actor.full_snapshot() {
            Ok(snapshot) => http_uds::write_json(stream, StatusCode::OK, &http_uds::snapshot_response(snapshot))
                .map_err(|err| format!("write HTTP snapshot response: {err}")),
            Err(err) => {
                http_uds::write_error(stream, StatusCode::CONFLICT, &err).map_err(|err| format!("write HTTP snapshot error: {err}"))
            }
        },
        http_uds::Route::SessionWait { id } if id == daemon_id => 'wait: {
            let body: http_uds::WaitRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP wait request: {err}"))?;
            let conditions: Vec<_> = body.conditions.into_iter().map(wait_condition_from_http).collect();
            if conditions.is_empty() {
                http_uds::write_error(stream, StatusCode::BAD_REQUEST, "at least one wait condition is required")
                    .map_err(|err| format!("write HTTP wait error: {err}"))?;
                break 'wait Ok(());
            }

            let has_text_match = conditions.iter().any(|condition| matches!(condition, crate::protocol::WaitCondition::TextMatch { .. }));
            let has_screen_stable =
                conditions.iter().any(|condition| matches!(condition, crate::protocol::WaitCondition::ScreenStable { .. }));
            if has_text_match {
                if let Err(err) = state.actor.validate_text_matching() {
                    http_uds::write_error(stream, StatusCode::CONFLICT, &format!("text matching not supported: {err}"))
                        .map_err(|err| format!("write HTTP wait error: {err}"))?;
                    break 'wait Ok(());
                }
            }
            let registered_at = Instant::now();
            let screen_stable = if has_screen_stable {
                match state.actor.full_snapshot() {
                    Ok(snapshot) => Some(ScreenStableState::new(ScreenStableFingerprint::from_snapshot(snapshot), registered_at)),
                    Err(err) => {
                        http_uds::write_error(stream, StatusCode::CONFLICT, &format!("screen stability not supported: {err}"))
                            .map_err(|err| format!("write HTTP wait error: {err}"))?;
                        break 'wait Ok(());
                    }
                }
            } else {
                None
            };

            if has_text_match {
                for condition in &conditions {
                    if let crate::protocol::WaitCondition::TextMatch { text } = condition {
                        if state.actor.screen_contains(text.clone())? {
                            http_uds::write_json(stream, StatusCode::OK, &http_uds::WaitResultResponse {
                                status: http_uds::WaitStatusResponse::Ready,
                                elapsed_ms: 0,
                            })
                            .map_err(|err| format!("write HTTP wait response: {err}"))?;
                            break 'wait Ok(());
                        }
                    }
                }
            }

            let pending_stream = stream.try_clone().map_err(|err| format!("clone HTTP wait stream: {err}"))?;
            if let Err(err) = set_stream_nonblocking(&pending_stream, true) {
                http_uds::write_error(stream, StatusCode::INTERNAL_SERVER_ERROR, &format!("set nonblocking: {err}"))
                    .map_err(|err| format!("write HTTP wait error: {err}"))?;
                break 'wait Ok(());
            }
            state.pending_waits.push(PendingWait {
                stream: pending_stream,
                conditions,
                screen_stable,
                timeout_ms: body.timeout_ms,
                registered_at,
            });
            Ok(())
        }
        _ => {
            http_uds::write_error(stream, StatusCode::NOT_FOUND, "not found").map_err(|err| format!("write HTTP not found response: {err}"))
        }
    }
}

fn signal_target_from_http(target: http_uds::SignalTargetRequest) -> crate::protocol::SignalTarget {
    match target {
        http_uds::SignalTargetRequest::Foreground => crate::protocol::SignalTarget::Foreground,
        http_uds::SignalTargetRequest::Leader => crate::protocol::SignalTarget::Leader,
        http_uds::SignalTargetRequest::Tree => crate::protocol::SignalTarget::Tree,
    }
}

fn wait_condition_from_http(condition: http_uds::WaitConditionRequest) -> crate::protocol::WaitCondition {
    match condition {
        http_uds::WaitConditionRequest::OutputIdle { quiet_ms } => crate::protocol::WaitCondition::OutputIdle { quiet_ms },
        http_uds::WaitConditionRequest::TextMatch { text } => crate::protocol::WaitCondition::TextMatch { text },
        http_uds::WaitConditionRequest::ScreenStable { stable_ms } => crate::protocol::WaitCondition::ScreenStable { stable_ms },
    }
}

fn wait_status_to_http(status: crate::protocol::WaitStatus) -> http_uds::WaitStatusResponse {
    match status {
        crate::protocol::WaitStatus::Ready => http_uds::WaitStatusResponse::Ready,
        crate::protocol::WaitStatus::Timeout => http_uds::WaitStatusResponse::Timeout,
        crate::protocol::WaitStatus::SessionGone => http_uds::WaitStatusResponse::SessionGone,
    }
}

fn http_input_key_bytes(key: http_uds::KeyRequest) -> Vec<u8> {
    match key {
        http_uds::KeyRequest::UnicodeScalar { codepoint } => {
            let mut bytes = Vec::new();
            if let Some(ch) = char::from_u32(codepoint) {
                let mut buf = [0; 4];
                bytes.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            }
            bytes
        }
        http_uds::KeyRequest::Named { key } => match key {
            http_uds::NamedKey::Enter => b"\r".to_vec(),
            http_uds::NamedKey::Escape => b"\x1b".to_vec(),
            http_uds::NamedKey::Backspace => b"\x7f".to_vec(),
            http_uds::NamedKey::Tab => b"\t".to_vec(),
            http_uds::NamedKey::Delete => b"\x1b[3~".to_vec(),
            http_uds::NamedKey::ArrowUp => b"\x1b[A".to_vec(),
            http_uds::NamedKey::ArrowDown => b"\x1b[B".to_vec(),
            http_uds::NamedKey::ArrowRight => b"\x1b[C".to_vec(),
            http_uds::NamedKey::ArrowLeft => b"\x1b[D".to_vec(),
        },
    }
}

struct PacketClient {
    stream: SessionStream,
    pending_output: Vec<u8>,
    input_reader: ActiveClientReader,
    input_buffer: Vec<u8>,
    channels: HashMap<u32, PacketSessionChannel>,
}

struct PacketSessionChannel {
    in_flight_generation: Option<u64>,
    last_sent_generation: u64,
}

#[derive(Default)]
struct PacketRenderCache {
    latest: Option<TerminalRenderUpdate>,
}

impl PacketRenderCache {
    fn store(&mut self, update: TerminalRenderUpdate) {
        self.latest = Some(update);
    }

    fn latest_generation(&self) -> Option<u64> {
        self.latest.as_ref().map(|update| update.render_generation)
    }

    fn latest(&self) -> Option<&TerminalRenderUpdate> {
        self.latest.as_ref()
    }
}

impl PacketClient {
    fn new(stream: SessionStream) -> Result<Self, String> {
        let input_reader = ActiveClientReader::new(&stream)?;
        Ok(Self { stream, pending_output: Vec::new(), input_reader, input_buffer: Vec::new(), channels: HashMap::new() })
    }

    fn enqueue_control<T: serde::Serialize>(&mut self, msg_type: u8, value: &T) -> Result<(), String> {
        let frame = PacketFrame::new(CHANNEL_CONTROL, msg_type, value).map_err(|err| format!("encode packet control frame: {err}"))?;
        self.enqueue_frame(&frame)
    }

    fn enqueue_frame(&mut self, frame: &PacketFrame) -> Result<(), String> {
        let mut encoded = Vec::new();
        frame.write(&mut encoded).map_err(|err| format!("buffer packet frame: {err}"))?;
        if self.pending_output.len().saturating_add(encoded.len()) > MAX_PENDING_CLIENT_OUTPUT_BYTES {
            return Err(format!("packet client output backlog exceeded {} bytes", MAX_PENDING_CLIENT_OUTPUT_BYTES));
        }
        self.pending_output.extend_from_slice(&encoded);
        Ok(())
    }

    fn drain_input_frames(&mut self, pending: &mut VecDeque<PacketFrame>, timeout: Duration) -> Result<bool, std::io::Error> {
        let mut first_poll = true;
        loop {
            let chunk = if first_poll {
                first_poll = false;
                self.input_reader.poll_timeout(&mut self.stream, timeout)?
            } else {
                self.input_reader.poll(&mut self.stream)?
            };
            match chunk {
                Some(bytes) if bytes.is_empty() => return Ok(false),
                Some(bytes) => self.input_buffer.extend_from_slice(&bytes),
                None => break,
            }
        }

        while let Some(frame) = PacketFrame::read_from_buffer(&mut self.input_buffer)? {
            pending.push_back(frame);
        }
        Ok(true)
    }

    fn flush_pending_output(&mut self) -> Result<bool, String> {
        while !self.pending_output.is_empty() {
            match self.stream.write(&self.pending_output) {
                Ok(0) => return Ok(false),
                Ok(n) => {
                    self.pending_output.drain(..n);
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) if is_graceful_socket_shutdown(&err) => return Ok(false),
                Err(err) => return Err(format!("flush packet client output: {err}")),
            }
        }
        Ok(true)
    }
}

fn service_packet_clients(
    actor: &SessionActor,
    daemon_id: &str,
    packet_clients: &mut Vec<PacketClient>,
    render_cache: &mut PacketRenderCache,
) -> Result<bool, String> {
    let mut did_work = false;
    let mut index = 0;
    while index < packet_clients.len() {
        let mut pending = VecDeque::new();
        let connected = packet_clients[index]
            .drain_input_frames(&mut pending, Duration::ZERO)
            .map_err(|err| format!("read packet client frame: {err}"))?;
        if !connected {
            packet_clients.swap_remove(index);
            did_work = true;
            continue;
        }

        while let Some(frame) = pending.pop_front() {
            did_work = true;
            handle_packet_frame(actor, daemon_id, &mut packet_clients[index], frame, render_cache)?;
        }
        index += 1;
    }
    Ok(did_work)
}

fn handle_packet_frame(
    actor: &SessionActor,
    daemon_id: &str,
    client: &mut PacketClient,
    frame: PacketFrame,
    render_cache: &mut PacketRenderCache,
) -> Result<(), String> {
    match (frame.channel, frame.msg_type) {
        (CHANNEL_CONTROL, MSG_CONTROL_OPEN_CHANNEL) => {
            let open = frame.decode::<OpenChannel>().map_err(|err| format!("decode open-channel packet: {err}"))?;
            open_packet_channel(actor, daemon_id, client, open, render_cache)?;
        }
        (CHANNEL_CONTROL, MSG_CONTROL_CLOSE_CHANNEL) => {
            let close = frame.decode::<CloseChannel>().map_err(|err| format!("decode close-channel packet: {err}"))?;
            client.channels.remove(&close.channel);
        }
        (channel, MSG_SESSION_ACK) if channel != CHANNEL_CONTROL => {
            let ack = frame.decode::<Ack>().map_err(|err| format!("decode ack packet: {err}"))?;
            if let Some(session_channel) = client.channels.get_mut(&channel) {
                if session_channel.in_flight_generation == Some(ack.generation) {
                    session_channel.in_flight_generation = None;
                    actor.mark_observed(ack.generation);
                }
            }
        }
        (channel, MSG_SESSION_INPUT) if channel != CHANNEL_CONTROL => {
            let input = frame.decode::<Input>().map_err(|err| format!("decode input packet: {err}"))?;
            if client.channels.contains_key(&channel) {
                route_packet_input_event(actor, input.event)?;
            }
        }
        (channel, MSG_SESSION_RESIZE) if channel != CHANNEL_CONTROL => {
            let resize = frame.decode::<Resize>().map_err(|err| format!("decode resize packet: {err}"))?;
            if client.channels.contains_key(&channel) {
                actor.resize(resize.cols, resize.rows)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn open_packet_channel(
    actor: &SessionActor,
    daemon_id: &str,
    client: &mut PacketClient,
    open: OpenChannel,
    render_cache: &mut PacketRenderCache,
) -> Result<(), String> {
    if open.channel == CHANNEL_CONTROL {
        client.enqueue_control(MSG_CONTROL_ERROR, &ControlError {
            channel: open.channel,
            message: "session channel must be non-zero".to_string(),
        })?;
        return Ok(());
    }
    if open.session_id != daemon_id {
        client.enqueue_control(MSG_CONTROL_ERROR, &ControlError {
            channel: open.channel,
            message: format!("unknown session {}", open.session_id),
        })?;
        return Ok(());
    }

    let update = actor.full_render_update()?;
    let generation = update.render_generation;
    render_cache.store(update.clone());
    client.enqueue_frame(
        &PacketFrame::new(open.channel, MSG_SESSION_RENDER, &RenderPacket { update })
            .map_err(|err| format!("encode initial render packet: {err}"))?,
    )?;
    client.channels.insert(open.channel, PacketSessionChannel { in_flight_generation: Some(generation), last_sent_generation: generation });
    Ok(())
}

fn push_due_packet_renders(
    actor: &SessionActor,
    packet_clients: &mut Vec<PacketClient>,
    render_cache: &mut PacketRenderCache,
) -> Result<(), String> {
    if !packet_clients.iter().any(|client| !client.channels.is_empty()) {
        return Ok(());
    }

    if actor.observation().dirty() != DirtyState::Clean {
        render_cache.store(actor.render_update()?);
    }

    let Some(latest_generation) = render_cache.latest_generation() else {
        return Ok(());
    };
    let Some(update) = render_cache.latest().cloned() else {
        return Ok(());
    };

    for client in packet_clients {
        let due_channels: Vec<u32> = client
            .channels
            .iter()
            .filter_map(|(channel, session)| {
                (session.in_flight_generation.is_none() && session.last_sent_generation < latest_generation).then_some(*channel)
            })
            .collect();
        for channel in due_channels {
            client.enqueue_frame(
                &PacketFrame::new(channel, MSG_SESSION_RENDER, &RenderPacket { update: update.clone() })
                    .map_err(|err| format!("encode render packet: {err}"))?,
            )?;
            if let Some(session_channel) = client.channels.get_mut(&channel) {
                session_channel.in_flight_generation = Some(latest_generation);
                session_channel.last_sent_generation = latest_generation;
            }
        }
    }
    Ok(())
}

fn route_packet_input_event(actor: &SessionActor, event: TerminalInputEvent) -> Result<(), String> {
    match event {
        TerminalInputEvent::Text(event) => actor.write_input(event.text.into_bytes()),
        TerminalInputEvent::Paste(event) => actor.paste(event.text.into_bytes()).map(|_| ()),
        TerminalInputEvent::RawBytes(bytes) => actor.write_input(bytes),
        TerminalInputEvent::Resize(event) => {
            actor.resize(event.cols, event.rows)?;
            if event.cell_width_px.is_finite()
                && event.cell_height_px.is_finite()
                && event.cell_width_px > 0.0
                && event.cell_height_px > 0.0
            {
                actor.set_cell_size(event.cell_width_px.round() as u32, event.cell_height_px.round() as u32)?;
            }
            Ok(())
        }
        TerminalInputEvent::Mouse(event) => route_packet_mouse_event(actor, event),
        TerminalInputEvent::Key(event) => actor.write_input(packet_key_event_bytes(event)),
        TerminalInputEvent::Focus(_) => Ok(()),
    }
}

fn route_packet_mouse_event(actor: &SessionActor, event: crate::provider::TerminalMouseEvent) -> Result<(), String> {
    let modifiers = vt::MouseModifiers {
        shift: event.modifiers.contains(crate::provider::TerminalModifiers::SHIFT),
        ctrl: event.modifiers.contains(crate::provider::TerminalModifiers::CTRL),
        alt: event.modifiers.contains(crate::provider::TerminalModifiers::ALT),
    };
    if event.kind == TerminalMouseEventKind::Wheel {
        actor.wheel(SessionWheelEvent {
            modifiers,
            cell_col: event.cell_col,
            cell_row: event.cell_row,
            x_px: event.x_px,
            y_px: event.y_px,
            wheel_delta_x: event.wheel_delta_x,
            wheel_delta_y: event.wheel_delta_y,
        })?;
        return Ok(());
    }

    let action = match event.kind {
        TerminalMouseEventKind::Press => vt::MouseAction::Press,
        TerminalMouseEventKind::Release => vt::MouseAction::Release,
        TerminalMouseEventKind::Move => vt::MouseAction::Motion,
        TerminalMouseEventKind::Wheel => unreachable!("wheel handled above"),
    };
    actor.mouse(SessionMouseEvent {
        action,
        button: event.button.and_then(packet_mouse_button),
        any_button_pressed: !event.buttons.is_empty(),
        modifiers,
        x_px: event.x_px,
        y_px: event.y_px,
    })?;
    Ok(())
}

fn packet_mouse_button(button: TerminalMouseButton) -> Option<vt::MouseButton> {
    match button {
        TerminalMouseButton::Left => Some(vt::MouseButton::Left),
        TerminalMouseButton::Middle => Some(vt::MouseButton::Middle),
        TerminalMouseButton::Right => Some(vt::MouseButton::Right),
        TerminalMouseButton::Back | TerminalMouseButton::Forward => None,
    }
}

fn packet_key_event_bytes(event: crate::provider::TerminalKeyEvent) -> Vec<u8> {
    if let Some(text) = event.generated_text {
        return text.into_bytes();
    }
    match event.key {
        TerminalKey::UnicodeScalar(codepoint) => char::from_u32(codepoint).map(|ch| ch.to_string().into_bytes()).unwrap_or_default(),
        TerminalKey::Named(key) => packet_named_key_bytes(key),
    }
}

fn packet_named_key_bytes(key: TerminalNamedKey) -> Vec<u8> {
    match key {
        TerminalNamedKey::Enter => b"\r".to_vec(),
        TerminalNamedKey::Escape => b"\x1b".to_vec(),
        TerminalNamedKey::Backspace => b"\x7f".to_vec(),
        TerminalNamedKey::Tab => b"\t".to_vec(),
        TerminalNamedKey::Delete => b"\x1b[3~".to_vec(),
        TerminalNamedKey::Insert => b"\x1b[2~".to_vec(),
        TerminalNamedKey::Home => b"\x1b[H".to_vec(),
        TerminalNamedKey::End => b"\x1b[F".to_vec(),
        TerminalNamedKey::PageUp => b"\x1b[5~".to_vec(),
        TerminalNamedKey::PageDown => b"\x1b[6~".to_vec(),
        TerminalNamedKey::ArrowUp => b"\x1b[A".to_vec(),
        TerminalNamedKey::ArrowDown => b"\x1b[B".to_vec(),
        TerminalNamedKey::ArrowLeft => b"\x1b[D".to_vec(),
        TerminalNamedKey::ArrowRight => b"\x1b[C".to_vec(),
        TerminalNamedKey::Function(n) => match n {
            1 => b"\x1bOP".to_vec(),
            2 => b"\x1bOQ".to_vec(),
            3 => b"\x1bOR".to_vec(),
            4 => b"\x1bOS".to_vec(),
            5..=12 => format!("\x1b[{}~", u16::from(n) + 10).into_bytes(),
            _ => Vec::new(),
        },
    }
}

fn flush_packet_clients(packet_clients: &mut Vec<PacketClient>) {
    packet_clients.retain_mut(|client| client.flush_pending_output().unwrap_or(false));
}

struct ActiveClient {
    stream: SessionStream,
    pending_output: Vec<u8>,
    input_reader: ActiveClientReader,
    input_buffer: Vec<u8>,
}

impl ActiveClient {
    fn new(stream: SessionStream) -> Result<Self, String> {
        let input_reader = ActiveClientReader::new(&stream)?;
        Ok(Self { stream, pending_output: Vec::new(), input_reader, input_buffer: Vec::new() })
    }

    fn drain_input_frames(&mut self, pending: &mut VecDeque<Frame>, timeout: Duration) -> Result<bool, std::io::Error> {
        let mut first_poll = true;
        loop {
            let chunk = if first_poll {
                first_poll = false;
                self.input_reader.poll_timeout(&mut self.stream, timeout)?
            } else {
                self.input_reader.poll(&mut self.stream)?
            };
            match chunk {
                Some(bytes) if bytes.is_empty() => return Ok(false),
                Some(bytes) => self.input_buffer.extend_from_slice(&bytes),
                None => break,
            }
        }

        while let Some(frame) = Frame::read_from_buffer(&mut self.input_buffer)? {
            pending.push_back(frame);
        }
        Ok(true)
    }

    fn enqueue_frame(&mut self, frame: &Frame) -> Result<(), String> {
        let mut encoded = Vec::new();
        frame.write(&mut encoded).map_err(|err| format!("buffer client frame: {err}"))?;
        if self.pending_output.len().saturating_add(encoded.len()) > MAX_PENDING_CLIENT_OUTPUT_BYTES {
            return Err(format!("client output backlog exceeded {} bytes", MAX_PENDING_CLIENT_OUTPUT_BYTES));
        }
        self.pending_output.extend_from_slice(&encoded);
        Ok(())
    }

    fn flush_pending_output(&mut self) -> Result<bool, String> {
        while !self.pending_output.is_empty() {
            match self.stream.write(&self.pending_output) {
                Ok(0) => return Ok(false),
                Ok(n) => {
                    self.pending_output.drain(..n);
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) if is_graceful_socket_shutdown(&err) => return Ok(false),
                Err(err) => return Err(format!("flush client output: {err}")),
            }
        }
        Ok(true)
    }
}

#[cfg(windows)]
struct ActiveClientReader {
    reader: crate::platform::ipc::OverlappedRead,
}

#[cfg(windows)]
impl ActiveClientReader {
    fn new(stream: &SessionStream) -> Result<Self, String> {
        let reader = stream.overlapped_reader(64 * 1024).map_err(|err| format!("create foreground client reader: {err}"))?;
        Ok(Self { reader })
    }

    fn poll(&mut self, _stream: &mut SessionStream) -> Result<Option<Vec<u8>>, std::io::Error> {
        self.reader.poll()
    }

    fn poll_timeout(&mut self, _stream: &mut SessionStream, timeout: Duration) -> Result<Option<Vec<u8>>, std::io::Error> {
        self.reader.poll_timeout(timeout)
    }
}

#[cfg(not(windows))]
struct ActiveClientReader;

#[cfg(not(windows))]
impl ActiveClientReader {
    fn new(_stream: &SessionStream) -> Result<Self, String> {
        Ok(Self)
    }

    fn poll(&mut self, stream: &mut SessionStream) -> Result<Option<Vec<u8>>, std::io::Error> {
        self.poll_timeout(stream, Duration::ZERO)
    }

    fn poll_timeout(&mut self, stream: &mut SessionStream, _timeout: Duration) -> Result<Option<Vec<u8>>, std::io::Error> {
        let mut buf = vec![0; 64 * 1024];
        match stream.read(&mut buf) {
            Ok(0) => Ok(Some(Vec::new())),
            Ok(n) => {
                buf.truncate(n);
                Ok(Some(buf))
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(err) => Err(err),
        }
    }
}

fn wait_for_socket(path: &Path) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if path.exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
    Err(format!("timed out waiting for socket {}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };

    use super::{
        apply_attach_state, attach_foreground, attach_init_capabilities, default_vt_engine, record_pty_output, session_socket_path,
        AttachCleanupGuard, ScreenStableFingerprint, ScreenStableState, TestReplayProbeVtEngine, SCREEN_STABLE_CHANGED_CELL_TOLERANCE,
    };
    use crate::{
        http_uds::read_http_request_for_test,
        runtime::{RuntimeLayout, SessionMetadata, TerminalSize},
        vt::{self, VtEngine},
    };

    #[cfg(unix)]
    #[test]
    fn attach_foreground_uses_http_upgrade_request() {
        use std::{fs, os::unix::net::UnixListener, sync::mpsc, thread, time::Duration};

        let temp = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(temp.path().join("alpha")).expect("create session dir");
        let socket_path = session_socket_path(temp.path(), "alpha");
        let listener = UnixListener::bind(&socket_path).expect("bind socket");
        let (tx, rx) = mpsc::channel();
        let reader = thread::spawn(move || {
            use std::io::Write;

            let (mut stream, _) = listener.accept().expect("accept connection");
            let request = read_http_request_for_test(&mut stream);
            tx.send(request).expect("send request");
            stream
                .write_all(b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: cleat-attach/1\r\n\r\n")
                .expect("write response");
        });

        let layout = RuntimeLayout::new(temp.path().to_path_buf());
        let attach = attach_foreground(&layout, "alpha").expect("attach");
        drop(attach);
        let request = rx.recv_timeout(Duration::from_secs(1)).expect("receive request");

        reader.join().expect("join reader");
        assert!(request.starts_with("POST /sessions/alpha/attach HTTP/1.1\r\n"), "{request}");
        assert!(request.contains("Connection: Upgrade\r\n"), "{request}");
        assert!(request.contains("Upgrade: cleat-attach/1\r\n"), "{request}");
        assert!(request.ends_with(r#""capabilities":{"color_level":"sixteen","kitty_keyboard":false}}"#), "{request}");
    }

    #[test]
    fn cleanup_guard_writes_on_drop() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let guard = AttachCleanupGuard::test_buffer(Arc::clone(&output));

        drop(guard);

        assert_eq!(
            *output.lock().expect("lock output"),
            b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1004l\x1b[<u\x1b[r\x1b[0m\x1b[?25h\x1b[2J\x1b[H\x1b[?1049l"
        );
    }

    #[test]
    fn cleanup_writes_fixed_reset_sequence_when_emitted() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut guard = AttachCleanupGuard::test_buffer(Arc::clone(&output));

        guard.emit().expect("emit cleanup");
        drop(guard);

        assert_eq!(
            *output.lock().expect("lock output"),
            b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1004l\x1b[<u\x1b[r\x1b[0m\x1b[?25h\x1b[2J\x1b[H\x1b[?1049l"
        );
    }

    #[test]
    fn cleanup_does_not_write_when_target_is_disabled() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut guard = AttachCleanupGuard::test_buffer_disabled(Arc::clone(&output));

        guard.emit().expect("emit cleanup");
        drop(guard);

        assert!(output.lock().expect("lock output").is_empty());
    }

    #[test]
    fn graceful_socket_shutdown_classifies_broken_pipe_disconnects() {
        let err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "broken pipe");
        assert!(super::is_graceful_socket_shutdown(&err));
    }

    #[cfg(unix)]
    #[test]
    fn active_client_rejects_unbounded_output_backlog() {
        let (stream, _peer) = std::os::unix::net::UnixStream::pair().expect("unix stream pair");
        let mut client = super::ActiveClient::new(stream).expect("create active client");
        client.pending_output = vec![0; super::MAX_PENDING_CLIENT_OUTPUT_BYTES - 1];

        let err = client.enqueue_frame(&super::Frame::Output(vec![1])).expect_err("backlog should overflow");

        assert!(err.contains("client output backlog exceeded"));
    }

    #[test]
    fn default_vt_engine_starts_with_default_size() {
        let session = SessionMetadata {
            id: "test".to_string(),
            vt_engine: vt::default_vt_engine_kind(),
            cwd: None,
            cmd: None,
            record: false,
            initial_size: TerminalSize::default(),
            colors: vt::TerminalColors::default(),
        };
        let engine = default_vt_engine(&session).expect("create default vt engine");
        assert_eq!(engine.size(), (crate::runtime::DEFAULT_TERMINAL_COLS, crate::runtime::DEFAULT_TERMINAL_ROWS));
        #[cfg(feature = "ghostty-vt")]
        assert!(engine.supports_replay());
        #[cfg(not(feature = "ghostty-vt"))]
        assert!(!engine.supports_replay());
        #[cfg(feature = "ghostty-vt")]
        assert!(engine.replay_payload(&vt::ClientCapabilities::conservative_fallback()).expect("replay payload").is_some());
        #[cfg(not(feature = "ghostty-vt"))]
        assert_eq!(engine.replay_payload(&vt::ClientCapabilities::conservative_fallback()).expect("replay payload"), None);
    }

    #[test]
    fn vt_engine_helpers_feed_and_resize_default_engine() {
        let session = SessionMetadata {
            id: "test".to_string(),
            vt_engine: vt::default_vt_engine_kind(),
            cwd: None,
            cmd: None,
            record: false,
            initial_size: TerminalSize { cols: 120, rows: 40 },
            colors: vt::TerminalColors::default(),
        };
        let mut engine = default_vt_engine(&session).expect("create default vt engine");
        assert_eq!(engine.size(), (120, 40));
        record_pty_output(engine.as_mut(), b"hello").expect("feed output");
        let replay =
            apply_attach_state(engine.as_mut(), 132, 40, &vt::ClientCapabilities::conservative_fallback()).expect("apply attach state");

        assert_eq!(engine.size(), (132, 40));
        #[cfg(feature = "ghostty-vt")]
        assert!(replay.is_some());
        #[cfg(not(feature = "ghostty-vt"))]
        assert_eq!(replay, None);
    }

    #[test]
    fn lifecycle_attach_init_capabilities_use_conservative_terminal_assumptions() {
        assert_eq!(attach_init_capabilities(), vt::ClientCapabilities::conservative_fallback());
    }

    fn screen_stable_fingerprint_with_cells(cell_count: usize, changed_prefix: usize) -> ScreenStableFingerprint {
        let cells = (0..cell_count)
            .map(|index| crate::provider::TerminalCell {
                graphemes: vec![if index < changed_prefix { 'x' as u32 } else { 'a' as u32 }],
                ..crate::provider::TerminalCell::default()
            })
            .collect();
        ScreenStableFingerprint {
            cols: cell_count as u16,
            rows: 1,
            viewport_kind: crate::provider::TerminalViewportKind::LiveNormal,
            scrollback_offset_rows: 0,
            cells,
        }
    }

    #[test]
    fn screen_stable_tolerates_small_rendered_cell_churn() {
        let baseline = screen_stable_fingerprint_with_cells(80, 0);
        let spinner_tick = screen_stable_fingerprint_with_cells(80, SCREEN_STABLE_CHANGED_CELL_TOLERANCE);

        assert!(!baseline.significant_change_from(&spinner_tick));
    }

    #[test]
    fn screen_stable_resets_on_large_rendered_cell_change() {
        let baseline = screen_stable_fingerprint_with_cells(80, 0);
        let changed = screen_stable_fingerprint_with_cells(80, SCREEN_STABLE_CHANGED_CELL_TOLERANCE + 1);

        assert!(baseline.significant_change_from(&changed));
    }

    #[test]
    fn screen_stable_resets_on_geometry_change() {
        let baseline = screen_stable_fingerprint_with_cells(80, 0);
        let mut resized = baseline.clone();
        resized.cols = 40;

        assert!(baseline.significant_change_from(&resized));
    }

    #[test]
    fn screen_stable_observe_only_resets_timestamp_for_significant_changes() {
        let baseline = screen_stable_fingerprint_with_cells(80, 0);
        let mut state = ScreenStableState::new(baseline, Instant::now());
        let original_stable_since = state.stable_since;

        state.observe(screen_stable_fingerprint_with_cells(80, 1), original_stable_since + Duration::from_millis(100));
        assert_eq!(state.stable_since, original_stable_since);

        let reset_at = original_stable_since + Duration::from_millis(200);
        state.observe(screen_stable_fingerprint_with_cells(80, SCREEN_STABLE_CHANGED_CELL_TOLERANCE + 1), reset_at);
        assert_eq!(state.stable_since, reset_at);
    }

    #[test]
    fn lifecycle_apply_attach_state_uses_attach_capabilities_for_replay() {
        let mut engine = TestReplayProbeVtEngine::new(80, 24);
        let capabilities = vt::ClientCapabilities::new(vt::ColorLevel::Ansi256, true);

        let replay = apply_attach_state(&mut engine, 100, 30, &capabilities).expect("apply attach state");

        assert_eq!(engine.size(), (100, 30));
        assert_eq!(replay, Some(b"Ansi256:true".to_vec()));
    }

    #[cfg(not(feature = "ghostty-vt"))]
    #[test]
    fn vt_engine_helpers_compile_without_ghostty_feature() {
        let mut engine = vt::make_default_vt_engine(80, 24);

        record_pty_output(engine.as_mut(), b"hello").expect("feed output");
        let replay =
            apply_attach_state(engine.as_mut(), 100, 30, &vt::ClientCapabilities::conservative_fallback()).expect("apply attach state");

        assert_eq!(engine.size(), (100, 30));
        assert_eq!(replay, None);
    }
}
