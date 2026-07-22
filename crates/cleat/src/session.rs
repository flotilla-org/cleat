#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    io::{self, Read, Write},
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

use http::StatusCode;

use crate::{
    host::actor::{RawOutputRecovery, RawOutputReplay, RawOutputTap, SessionActor, SessionMouseEvent, SessionWheelEvent},
    http_uds,
    packet::{
        Ack, ActivityEvent, ActivitySession, ActivitySnapshot, ChannelRole, CloseChannel, ControlError, ControlHello, DirectoryDelta,
        DirectoryEntry, DirectorySnapshot, Input, OpenChannel, PacketFrame, RenderPacket, Resize, RoleRequest, RoleState, ScreenActivity,
        CHANNEL_CONTROL, MSG_CONTROL_ACTIVITY_EVENT, MSG_CONTROL_ACTIVITY_SNAPSHOT, MSG_CONTROL_CLOSE_CHANNEL, MSG_CONTROL_DIRECTORY_DELTA,
        MSG_CONTROL_DIRECTORY_SNAPSHOT, MSG_CONTROL_ERROR, MSG_CONTROL_HELLO, MSG_CONTROL_OPEN_CHANNEL, MSG_SESSION_ACK, MSG_SESSION_INPUT,
        MSG_SESSION_RENDER, MSG_SESSION_RESIZE, MSG_SESSION_ROLE, MSG_SESSION_VIEWPORT,
    },
    platform::{
        daemon::{is_session_daemon_alive, spawn_daemon_process},
        ipc::{
            bind_session_listener, connect_session_stream, set_listener_nonblocking, set_stream_nonblocking, set_stream_write_timeout,
            shutdown_stream, try_connect_session_stream, validate_session_socket_path, SessionStream,
        },
        terminal::{attach_signal_exit_requested, current_terminal_size, stdout_is_tty, AttachSignalHandlers, ForegroundTerminal},
    },
    protocol::Frame,
    provider::{
        DirtyState, TerminalInputEvent, TerminalKey, TerminalMouseButton, TerminalMouseEventKind, TerminalNamedKey, TerminalRenderUpdate,
    },
    runtime::{AmbientSessionCoordinates, RuntimeLayout, SessionMetadata, TerminalSize},
    vt::{self, ScreenGrid, VtEngine, VtEngineKind},
};

const DETACH_CLEANUP_SEQUENCE: &[u8] =
    b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1004l\x1b[<u\x1b[r\x1b[0m\x1b[?25h\x1b[2J\x1b[H\x1b[?1049l";
const REATTACH_CLEAR_SEQUENCE: &[u8] = b"\x1b[2J\x1b[H";
const MAX_PENDING_CLIENT_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const SESSION_DAEMON_SERVICING_TICK: Duration = Duration::from_millis(10);
const SESSION_DAEMON_IDLE_LINGER: Duration = Duration::from_secs(120);
const SESSION_HTTP_HANDSHAKE_DEADLINE: Duration = Duration::from_millis(250);
const SESSION_HTTP_RESPONSE_WRITE_DEADLINE: Duration = Duration::from_millis(250);
const TERMINATE_SIGNAL: i32 = 15;
const SESSION_DAEMON_REGISTRATION_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const SCREEN_STABLE_CHANGED_CELL_TOLERANCE: usize = 16;

#[derive(Debug)]
pub struct ForegroundAttach {
    stream: Arc<Mutex<SessionStream>>,
}

#[derive(Debug, Clone, Default)]
pub struct SessionStartOptions {
    pub record: bool,
    pub initial_size: TerminalSize,
    pub colors: vt::TerminalColors,
    pub tags: Vec<String>,
}

impl ForegroundAttach {
    pub fn relay_stdio(self) -> Result<(), String> {
        let signal_handlers = AttachSignalHandlers::install()?;
        self.relay_stdio_with_handlers(signal_handlers)
    }

    /// Relay with handlers the caller installed *before* the attach
    /// handshake. The daemon writes the foreground marker at attach grant;
    /// a signal delivered between that grant and the relay starting must
    /// already be caught, or the process dies with default disposition
    /// (observed as a test race once the daemon got fast enough).
    pub fn relay_stdio_with_handlers(self, signal_handlers: AttachSignalHandlers) -> Result<(), String> {
        let _signal_handlers = signal_handlers;
        let mut cleanup = AttachCleanupGuard::stdout();
        let mut terminal = ForegroundTerminal::enter()?;
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
    start_session(layout, id, vt_engine, cwd, cmd, options, DaemonRequirement::AutoStart)
}

pub(crate) fn start_session_in_running_daemon(
    layout: &RuntimeLayout,
    expected_daemon_pid: u32,
    id: Option<String>,
    vt_engine: Option<VtEngineKind>,
    cwd: Option<PathBuf>,
    cmd: Option<String>,
    options: SessionStartOptions,
) -> Result<SessionMetadata, String> {
    start_session(layout, id, vt_engine, cwd, cmd, options, DaemonRequirement::AlreadyRunning(expected_daemon_pid))
}

#[derive(Clone, Copy)]
enum DaemonRequirement {
    AutoStart,
    AlreadyRunning(u32),
}

fn start_session(
    layout: &RuntimeLayout,
    id: Option<String>,
    vt_engine: Option<VtEngineKind>,
    cwd: Option<PathBuf>,
    cmd: Option<String>,
    options: SessionStartOptions,
    daemon_requirement: DaemonRequirement,
) -> Result<SessionMetadata, String> {
    let vt_engine = vt_engine.unwrap_or_else(vt::default_vt_engine_kind);
    vt_engine.ensure_available()?;
    let id = id.unwrap_or_else(|| format!("session-{}", uuid::Uuid::new_v4()));
    crate::runtime::validate_runtime_name(&id)?;
    let mut session = layout.session_metadata(id, vt_engine, cwd, cmd);
    session.record = options.record;
    session.initial_size = options.initial_size;
    session.colors = options.colors;
    session.tags = options.tags;
    crate::runtime::normalize_tags(&mut session.tags);

    if matches!(daemon_requirement, DaemonRequirement::AutoStart) {
        ensure_daemon_started(layout)?;
    }
    let mut stream = connect_session_stream(&layout.socket_path()).map_err(|err| match daemon_requirement {
        DaemonRequirement::AutoStart => err,
        DaemonRequirement::AlreadyRunning(_) => format!("source daemon {} is no longer running: {err}", layout.daemon_name()),
    })?;
    let body = serde_json::to_vec(&session).map_err(|err| format!("serialize session create request: {err}"))?;
    match daemon_requirement {
        DaemonRequirement::AutoStart => http_uds::write_request(&mut stream, http::Method::POST, "/sessions", &body),
        DaemonRequirement::AlreadyRunning(expected_pid) => http_uds::write_session_create_request(&mut stream, &body, expected_pid),
    }
    .map_err(|err| format!("write session create request: {err}"))?;
    let response = http_uds::read_response(&mut stream).map_err(|err| format!("read session create response: {err}"))?;
    if response.status != StatusCode::OK {
        return Err(http_error_message(http_uds::HttpResponse { status: response.status, body: response.body }));
    }
    let response: http_uds::CreateSessionResponse =
        serde_json::from_slice(&response.body).map_err(|err| format!("parse session create response: {err}"))?;
    Ok(response.session)
}

pub fn attach_foreground(layout: &RuntimeLayout, id: &str) -> Result<ForegroundAttach, String> {
    connect_foreground_upgrade(layout, id, "attach")
}

pub fn watch_foreground(layout: &RuntimeLayout, id: &str) -> Result<ForegroundAttach, String> {
    connect_foreground_upgrade(layout, id, "watch")
}

fn connect_foreground_upgrade(layout: &RuntimeLayout, id: &str, role: &str) -> Result<ForegroundAttach, String> {
    let socket_path = layout.socket_path();
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
    let _ = id;
    RuntimeLayout::new(root.to_path_buf()).socket_path()
}

pub fn daemon_pid_path(root: &Path, id: &str) -> PathBuf {
    let _ = id;
    RuntimeLayout::new(root.to_path_buf()).daemon_pid_path()
}

pub fn foreground_path(root: &Path, id: &str) -> PathBuf {
    RuntimeLayout::new(root.to_path_buf()).foreground_path(id)
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
    vt::ClientCapabilities::detect()
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

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

struct PendingExpect {
    stream: SessionStream,
    text: String,
    since_offset: u64,
    last_checked_file_size: u64,
    timeout_ms: u64,
    registered_at: Instant,
}

/// Identity of a session channel on a packet connection, stable across the
/// packet_clients vec's swap_remove reordering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PacketChannelRef {
    client_id: u64,
    channel: u32,
}

struct HostedSession {
    metadata: SessionMetadata,
    actor: SessionActor,
    raw_output_tap: RawOutputTap,
    active_client: Option<ActiveClient>,
    watchers: Vec<ActiveClient>,
    /// Packet channel currently holding the controller role. Mutually
    /// exclusive with `active_client` (the legacy stream controller).
    packet_controller: Option<PacketChannelRef>,
    packet_render_cache: PacketRenderCache,
    had_foreground_client: bool,
    pending_waits: Vec<PendingWait>,
    pending_expects: Vec<PendingExpect>,
    /// Render generation at the last screen-stable fingerprint snapshot; lets
    /// the servicing loop skip full-grid snapshots while nothing has rendered.
    screen_stable_snapshot_generation: Option<u64>,
    should_keep_session_dir: bool,
}

impl HostedSession {
    fn spawn(session_dir: PathBuf, session: SessionMetadata, coordinates: AmbientSessionCoordinates) -> Result<Self, String> {
        let should_keep_session_dir = session.record;
        let actor_session_dir = session_dir;
        let actor_session = session.clone();
        let actor = SessionActor::spawn(session.initial_size.rows, Arc::new(|| {}), move || {
            crate::session_runtime::SessionRuntime::spawn_in_daemon(
                actor_session_dir,
                &actor_session,
                default_vt_engine(&actor_session)?,
                &coordinates,
            )
        })?;
        let raw_output_tap = actor.subscribe_raw_output()?;
        Ok(Self {
            metadata: session,
            actor,
            raw_output_tap,
            active_client: None,
            watchers: Vec::new(),
            packet_controller: None,
            packet_render_cache: PacketRenderCache::default(),
            had_foreground_client: false,
            pending_waits: Vec::new(),
            pending_expects: Vec::new(),
            screen_stable_snapshot_generation: None,
            should_keep_session_dir,
        })
    }

    fn activity_session(&self, stable_threshold_ms: u64) -> ActivitySession {
        let activity = self.actor.screen_activity().snapshot(Instant::now(), Duration::from_millis(stable_threshold_ms));
        ActivitySession {
            session_id: self.metadata.id.clone(),
            tags: self.metadata.tags.clone(),
            activity: activity.screen_activity,
            stable_since_unix_ms: activity.quiet_since,
            last_output_at_unix_ms: activity.last_output_at,
        }
    }
}

fn write_http_wait_result(stream: &mut SessionStream, status: crate::protocol::WaitStatus, elapsed_ms: u64) -> std::io::Result<()> {
    http_uds::write_json(stream, StatusCode::OK, &http_uds::WaitResultResponse { status: wait_status_to_http(status), elapsed_ms })
}

fn enqueue_output_chunk(
    layout: &RuntimeLayout,
    id: &str,
    actor: &SessionActor,
    active_client: &mut Option<ActiveClient>,
    watchers: &mut Vec<ActiveClient>,
    chunk: &[u8],
) {
    if let Some(client) = active_client.as_mut() {
        if client.enqueue_output(chunk).is_err() {
            let _ = fs::remove_file(layout.foreground_path(id));
            let _ = actor.record_detach();
            let _ = actor.set_client_presence(false);
            *active_client = None;
        }
    }
    watchers.retain_mut(|watcher| watcher.enqueue_output(chunk).is_ok());
}

fn drain_raw_output_tap(
    layout: &RuntimeLayout,
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
                enqueue_output_chunk(layout, id, actor, active_client, watchers, &chunk.bytes);
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => return Ok(drained),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                recover_raw_output_tap(layout, id, actor, raw_output_tap, active_client, watchers, None)?;
                return Ok(drained);
            }
        }
    }
}

fn drain_raw_output_tap_before_client_install(
    layout: &RuntimeLayout,
    id: &str,
    hosted: &mut HostedSession,
    new_client: &mut ActiveClient,
    replay: RawOutputReplay,
    replay_mode: ReplayMode,
) -> Result<(), String> {
    let plan = plan_raw_output_client_install(&hosted.raw_output_tap, replay, replay_mode);
    let existing_chunks = match &plan {
        RawOutputClientInstallPlan::Complete { existing_chunks, .. } | RawOutputClientInstallPlan::Disconnected { existing_chunks } => {
            existing_chunks
        }
    };
    for chunk in existing_chunks {
        enqueue_output_chunk(layout, id, &hosted.actor, &mut hosted.active_client, &mut hosted.watchers, chunk);
    }
    match plan {
        RawOutputClientInstallPlan::Complete { new_client_frames, .. } => {
            enqueue_frames(new_client, &new_client_frames)?;
        }
        RawOutputClientInstallPlan::Disconnected { .. } => {
            recover_raw_output_tap(
                layout,
                id,
                &hosted.actor,
                &mut hosted.raw_output_tap,
                &mut hosted.active_client,
                &mut hosted.watchers,
                Some(NewRawOutputClient { client: new_client, replay_mode }),
            )?;
        }
    }
    Ok(())
}

enum RawOutputClientInstallPlan {
    Complete { existing_chunks: Vec<Arc<[u8]>>, new_client_frames: Vec<Frame> },
    Disconnected { existing_chunks: Vec<Arc<[u8]>> },
}

fn plan_raw_output_client_install(
    raw_output_tap: &RawOutputTap,
    replay: RawOutputReplay,
    replay_mode: ReplayMode,
) -> RawOutputClientInstallPlan {
    let mut existing_chunks = Vec::new();
    let mut live_after_replay = Vec::new();
    loop {
        match raw_output_tap.try_recv() {
            Ok(chunk) => {
                if chunk.sequence > replay.through_sequence {
                    live_after_replay.push(Arc::clone(&chunk.bytes));
                }
                existing_chunks.push(chunk.bytes);
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                let mut new_client_frames = replay_frames(replay.payload, replay_mode);
                // Install-time only (never the per-byte path), so copying the
                // few live chunks into owned `Frame` payloads is fine.
                new_client_frames.extend(live_after_replay.into_iter().map(|bytes| Frame::Output(bytes.to_vec())));
                return RawOutputClientInstallPlan::Complete { existing_chunks, new_client_frames };
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                return RawOutputClientInstallPlan::Disconnected { existing_chunks };
            }
        }
    }
}

trait RawOutputRecoverySource {
    fn recover_raw_output(&self, capabilities: Vec<vt::ClientCapabilities>) -> Result<RawOutputRecovery, String>;
}

impl RawOutputRecoverySource for SessionActor {
    fn recover_raw_output(&self, capabilities: Vec<vt::ClientCapabilities>) -> Result<RawOutputRecovery, String> {
        SessionActor::recover_raw_output(self, capabilities)
    }
}

#[derive(Clone, Copy)]
enum ReplayMode {
    FreshTerminal,
    ResetTerminal,
}

impl ReplayMode {
    fn resets_terminal(self) -> bool {
        matches!(self, Self::ResetTerminal)
    }
}

#[derive(Clone, Copy)]
struct RawOutputRecoveryRequest {
    capabilities: vt::ClientCapabilities,
    replay_mode: ReplayMode,
}

struct RawOutputRecoveryPlan {
    tap: RawOutputTap,
    recipient_frames: Vec<Vec<Frame>>,
}

fn plan_raw_output_recovery(
    source: &impl RawOutputRecoverySource,
    requests: &[RawOutputRecoveryRequest],
) -> Result<RawOutputRecoveryPlan, String> {
    let recovery = source.recover_raw_output(requests.iter().map(|request| request.capabilities).collect())?;
    if recovery.payloads.len() != requests.len() {
        return Err(format!("raw output recovery returned {} payloads for {} clients", recovery.payloads.len(), requests.len()));
    }
    let recipient_frames =
        recovery.payloads.into_iter().zip(requests).map(|(payload, request)| replay_frames(payload, request.replay_mode)).collect();
    Ok(RawOutputRecoveryPlan { tap: recovery.tap, recipient_frames })
}

fn replay_frames(payload: Option<Vec<u8>>, replay_mode: ReplayMode) -> Vec<Frame> {
    let Some(payload) = payload.filter(|payload| !payload.is_empty()) else {
        return Vec::new();
    };
    let mut frames = Vec::with_capacity(usize::from(replay_mode.resets_terminal()) + 1);
    if replay_mode.resets_terminal() {
        frames.push(Frame::Output(REATTACH_CLEAR_SEQUENCE.to_vec()));
    }
    frames.push(Frame::Output(payload));
    frames
}

fn enqueue_frames(client: &mut ActiveClient, frames: &[Frame]) -> Result<(), String> {
    for frame in frames {
        client.enqueue_frame(frame)?;
    }
    Ok(())
}

struct NewRawOutputClient<'a> {
    client: &'a mut ActiveClient,
    replay_mode: ReplayMode,
}

fn recover_raw_output_tap(
    layout: &RuntimeLayout,
    id: &str,
    actor: &SessionActor,
    raw_output_tap: &mut RawOutputTap,
    active_client: &mut Option<ActiveClient>,
    watchers: &mut Vec<ActiveClient>,
    new_client: Option<NewRawOutputClient<'_>>,
) -> Result<(), String> {
    let requests: Vec<_> = active_client
        .iter()
        .map(|client| RawOutputRecoveryRequest { capabilities: client.capabilities, replay_mode: ReplayMode::ResetTerminal })
        .chain(
            watchers
                .iter()
                .map(|watcher| RawOutputRecoveryRequest { capabilities: watcher.capabilities, replay_mode: ReplayMode::ResetTerminal }),
        )
        .chain(new_client.as_ref().map(|new_client| RawOutputRecoveryRequest {
            capabilities: new_client.client.capabilities,
            replay_mode: new_client.replay_mode,
        }))
        .collect();
    let recovery = plan_raw_output_recovery(actor, &requests)?;
    *raw_output_tap = recovery.tap;
    let mut recipient_frames = recovery.recipient_frames.into_iter();
    if let Some(client) = active_client.as_mut() {
        if recipient_frames.next().is_some_and(|frames| enqueue_frames(client, &frames).is_err()) {
            let _ = fs::remove_file(layout.foreground_path(id));
            let _ = actor.record_detach();
            let _ = actor.set_client_presence(false);
            *active_client = None;
        }
    }
    watchers.retain_mut(|watcher| recipient_frames.next().is_none_or(|frames| enqueue_frames(watcher, &frames).is_ok()));
    if let Some(new_client) = new_client {
        if let Some(frames) = recipient_frames.next() {
            enqueue_frames(new_client.client, &frames)?;
        }
    }
    Ok(())
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
pub fn run_session_daemon(root: &Path, daemon_name: &str) -> Result<(), String> {
    let layout = RuntimeLayout::new(root.to_path_buf()).with_daemon(daemon_name.to_string())?;
    let socket_path = layout.socket_path();
    validate_session_socket_path(&socket_path)?;
    layout.ensure_daemon_dirs()?;
    let listener = match bind_session_listener(&socket_path) {
        Ok(listener) => listener,
        Err(first_err) => {
            if try_connect_session_stream(&socket_path).is_ok() {
                return Ok(());
            }
            match fs::remove_file(&socket_path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(format!("remove stale daemon socket {}: {err}", socket_path.display())),
            }
            bind_session_listener(&socket_path).map_err(|second_err| format!("{first_err}; after stale cleanup: {second_err}"))?
        }
    };
    set_listener_nonblocking(&listener, true)?;
    let daemon_pid = std::process::id().to_string();
    fs::write(layout.daemon_pid_path(), &daemon_pid).map_err(|err| format!("write daemon pid: {err}"))?;

    let mut sessions: HashMap<String, HostedSession> = HashMap::new();
    let mut packet_clients: Vec<PacketClient> = Vec::new();
    let mut pending_http_handshakes: Vec<PendingHttpHandshake> = Vec::new();
    let mut next_packet_client_id: u64 = 1;
    let mut exited_session: Option<(String, bool)> = None;
    let mut faulted_session: Option<String> = None;
    let mut idle_since = Some(Instant::now());
    let mut last_registration_check = Instant::now();
    let mut registration_was_lost = false;

    loop {
        // Self-fencing watchdog: the pid file registers which process owns
        // this daemon identity. If it no longer names us — the runtime root
        // was deleted (e.g. a test tempdir was cleaned up), or another daemon
        // reclaimed the socket — no client can ever route to us again, so
        // terminate the hosted sessions and exit instead of servicing them
        // forever. Two consecutive misses guard against transient read
        // failures.
        if last_registration_check.elapsed() >= SESSION_DAEMON_REGISTRATION_CHECK_INTERVAL {
            last_registration_check = Instant::now();
            // Only a definitively missing pid file (or one naming another
            // process) counts as deregistration. A transient read failure
            // (EIO, permissions blip) must not self-terminate a healthy
            // daemon — that would drop every hosted session.
            let registered = match fs::read_to_string(layout.daemon_pid_path()) {
                Ok(contents) => contents.trim() == daemon_pid,
                Err(err) => err.kind() != std::io::ErrorKind::NotFound,
            };
            if !registered && registration_was_lost {
                for hosted in sessions.values() {
                    // Tree, not Leader: this path has the same orphaned-child
                    // problem as SessionDelete — a background child in the
                    // leader's process group must not outlive the fenced daemon.
                    let _ = hosted.actor.dispatch_signal(TERMINATE_SIGNAL, crate::protocol::SignalTarget::Tree);
                }
                // Deliberately leave the socket and pid file alone on this
                // path: they are either already gone or owned by a successor.
                return Ok(());
            }
            registration_was_lost = !registered;
        }

        let mut did_work = false;

        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    did_work = true;
                    if let Ok(pending) = PendingHttpHandshake::new(stream) {
                        pending_http_handshakes.push(pending);
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) => return Err(format!("accept client: {err}")),
            }
        }

        let mut handshake_index = 0;
        while handshake_index < pending_http_handshakes.len() {
            match pending_http_handshakes[handshake_index].poll() {
                Ok(HttpHandshakePoll::Pending(handshake_did_work)) => {
                    did_work |= handshake_did_work;
                    handshake_index += 1;
                }
                Ok(HttpHandshakePoll::Ready(request)) => {
                    did_work = true;
                    let pending = pending_http_handshakes.swap_remove(handshake_index);
                    let mut stream = pending.into_stream();
                    if set_stream_nonblocking(&stream, false).is_err() {
                        continue;
                    }
                    let mut http_state = HttpRequestState {
                        layout: &layout,
                        sessions: &mut sessions,
                        packet_clients: &mut packet_clients,
                        next_packet_client_id: &mut next_packet_client_id,
                    };
                    let mut response_committed = false;
                    if let Err(err) =
                        handle_http_request(root, daemon_name, &mut stream, *request, &mut http_state, &mut response_committed)
                    {
                        if !response_committed {
                            let _ = http_uds::write_error(&mut stream, StatusCode::INTERNAL_SERVER_ERROR, &err);
                        }
                    }
                    if !sessions.is_empty() {
                        idle_since = None;
                    }
                }
                Err(err) => {
                    did_work = true;
                    let pending = pending_http_handshakes.swap_remove(handshake_index);
                    if err.kind() == io::ErrorKind::InvalidData {
                        let mut stream = pending.into_stream();
                        let _ = set_stream_nonblocking(&stream, false);
                        let _ = http_uds::write_error(&mut stream, StatusCode::INTERNAL_SERVER_ERROR, &format!("read HTTP request: {err}"));
                    }
                }
            }
        }

        did_work |= service_packet_clients(&layout, &mut sessions, &mut packet_clients)?;
        let session_ids: Vec<String> = sessions.keys().cloned().collect();
        for session_id in session_ids {
            let Some(hosted) = sessions.get_mut(&session_id) else {
                continue;
            };
            let service_result = contain_session_unwind(&session_id, || -> Result<(bool, Option<bool>), String> {
                maybe_panic_for_containment_test(&session_id);
                let session_did_work = service_hosted_session(&layout, &session_id, hosted, &mut packet_clients)?;
                // Exit state is read from the observation mirror — never a
                // blocking round-trip into a possibly-busy actor.
                let exit_code = hosted.actor.observation().exit_code();
                if exit_code.is_none() && hosted.actor.worker_finished() {
                    return Err("session actor stopped without reporting an exit".to_string());
                }
                let should_keep_session_dir = if exit_code.is_some() {
                    Some(finish_exited_session(&layout, &session_id, hosted, &mut packet_clients)?)
                } else {
                    None
                };
                Ok((session_did_work, should_keep_session_dir))
            });
            match service_result {
                Some((session_did_work, Some(should_keep_session_dir))) => {
                    did_work |= session_did_work;
                    exited_session = Some((session_id, should_keep_session_dir));
                    break;
                }
                Some((session_did_work, None)) => {
                    did_work |= session_did_work;
                }
                None => {
                    did_work = true;
                    faulted_session = Some(session_id);
                    break;
                }
            }
        }

        if let Some(session_id) = faulted_session.take() {
            cleanup_exited_session(&layout, &session_id, true);
            broadcast_directory_remove(&session_id, &mut packet_clients)?;
            sessions.remove(&session_id);
            remove_packet_channels_for_session(&session_id, &mut packet_clients);
        }

        if let Some((session_id, should_keep_session_dir)) = exited_session.take() {
            cleanup_exited_session(&layout, &session_id, should_keep_session_dir);
            broadcast_directory_remove(&session_id, &mut packet_clients)?;
            sessions.remove(&session_id);
            remove_packet_channels_for_session(&session_id, &mut packet_clients);
        }

        service_activity_subscriptions(&sessions, &mut packet_clients)?;
        flush_packet_clients(&mut packet_clients);

        if sessions.is_empty() {
            let idle_started = idle_since.get_or_insert_with(Instant::now);
            if idle_started.elapsed() >= SESSION_DAEMON_IDLE_LINGER {
                break;
            }
        } else {
            idle_since = None;
        }

        if !did_work {
            thread::sleep(SESSION_DAEMON_SERVICING_TICK);
        }
    }

    let _ = fs::remove_file(&socket_path);
    let _ = fs::remove_file(layout.daemon_pid_path());
    Ok(())
}

/// Contains both panics and errors from servicing one session: either way
/// the caller faults that session and the daemon keeps serving the others.
/// `None` means the session must be faulted.
fn contain_session_unwind<T, F>(id: &str, action: F) -> Option<T>
where
    F: FnOnce() -> Result<T, String>,
{
    match panic::catch_unwind(AssertUnwindSafe(action)) {
        Ok(Ok(result)) => Some(result),
        Ok(Err(err)) => {
            eprintln!("contained error while servicing session {id}: {err}");
            None
        }
        Err(payload) => {
            eprintln!("contained panic while servicing session {id}: {}", panic_payload_message(payload.as_ref()));
            None
        }
    }
}

fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

fn remove_packet_channels_for_session(session_id: &str, packet_clients: &mut [PacketClient]) {
    for client in packet_clients {
        // Tell the client each affected channel is gone before dropping it.
        // Directory deltas are selector-filtered, so an attached client may
        // never see the remove delta; the channel-scoped error is the only
        // guaranteed close signal. Enqueue failures mean the client transport
        // is already dead and its teardown path will reap it.
        let closed: Vec<u32> = client.channels.iter().filter(|(_, ch)| ch.session_id == session_id).map(|(id, _)| *id).collect();
        for channel in closed {
            let _ = client.enqueue_control(MSG_CONTROL_ERROR, &ControlError { channel, message: format!("session {session_id} exited") });
            client.channels.remove(&channel);
        }
    }
}

/// O(clients x channels) scan, run on every directory-entry build. Fine at
/// expected client counts; cache per-session counters if that ever grows.
fn packet_role_counts(session_id: &str, packet_clients: &[PacketClient]) -> (u32, u32) {
    let mut controllers = 0u32;
    let mut watchers = 0u32;
    for client in packet_clients {
        for session_channel in client.channels.values() {
            if session_channel.session_id == session_id {
                match session_channel.role {
                    ChannelRole::Controller => controllers = controllers.saturating_add(1),
                    ChannelRole::Watcher => watchers = watchers.saturating_add(1),
                }
            }
        }
    }
    (controllers, watchers)
}

fn directory_entry_for_session(
    layout: &RuntimeLayout,
    hosted: &HostedSession,
    packet_clients: &[PacketClient],
) -> Result<DirectoryEntry, String> {
    let inspect = hosted.actor.inspect(hosted.active_client.is_some(), hosted.watchers.len())?;
    let (packet_controllers, packet_watchers) = packet_role_counts(&inspect.session.id, packet_clients);
    Ok(DirectoryEntry {
        session_id: inspect.session.id.clone(),
        tags: inspect.session.tags,
        state: inspect.session.state,
        controller_count: (if hosted.active_client.is_some() { 1 } else { 0 }) + packet_controllers,
        watcher_count: u32::try_from(hosted.watchers.len()).unwrap_or(u32::MAX).saturating_add(packet_watchers),
        recreatable: crate::recreate::session_is_recreatable(&layout.session_dir(&inspect.session.id)),
        cols: inspect.terminal.cols,
        rows: inspect.terminal.rows,
    })
}

fn directory_snapshot_for_sessions(
    layout: &RuntimeLayout,
    sessions: &HashMap<String, HostedSession>,
    packet_clients: &[PacketClient],
    selectors: &[String],
) -> Result<DirectorySnapshot, String> {
    let mut entries = Vec::new();
    for hosted in sessions.values() {
        let entry = directory_entry_for_session(layout, hosted, packet_clients)?;
        if directory_entry_matches_selectors(&entry, selectors) {
            entries.push(entry);
        }
    }
    entries.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    Ok(DirectorySnapshot { sessions: entries })
}

fn activity_snapshot_for_sessions(
    sessions: &HashMap<String, HostedSession>,
    selectors: &[String],
    stable_threshold_ms: u64,
) -> ActivitySnapshot {
    ActivitySnapshot { stable_threshold_ms, sessions: matching_activity_sessions(sessions, selectors, stable_threshold_ms) }
}

fn matching_activity_sessions(
    sessions: &HashMap<String, HostedSession>,
    selectors: &[String],
    stable_threshold_ms: u64,
) -> Vec<ActivitySession> {
    let mut activity_sessions = sessions
        .values()
        .filter(|hosted| selectors.iter().all(|selector| hosted.metadata.tags.contains(selector)))
        .map(|hosted| hosted.activity_session(stable_threshold_ms))
        .collect::<Vec<_>>();
    activity_sessions.sort_by(|left, right| left.session_id.cmp(&right.session_id));
    activity_sessions
}

fn service_activity_subscriptions(sessions: &HashMap<String, HostedSession>, packet_clients: &mut Vec<PacketClient>) -> Result<(), String> {
    for client in packet_clients {
        if client.dead {
            continue;
        }
        let Some(stable_threshold_ms) = client.screen_activity_stable_ms else {
            continue;
        };
        let current = matching_activity_sessions(sessions, &client.selectors, stable_threshold_ms);

        let current_ids = current.iter().map(|session| session.session_id.clone()).collect::<HashSet<_>>();
        for session in current {
            let prior = client.known_activity_sessions.get(&session.session_id);
            let added = prior.is_none();
            let changed = prior.is_some_and(|known| known.activity != session.activity);
            client.known_activity_sessions.insert(session.session_id.clone(), session.clone());
            if added {
                client.enqueue_control(MSG_CONTROL_ACTIVITY_EVENT, &ActivityEvent::MembershipAdded {
                    session,
                    changed_at_unix_ms: unix_time_ms(),
                })?;
            } else if changed {
                let changed_at_unix_ms = match session.activity {
                    ScreenActivity::Active => session.last_output_at_unix_ms.unwrap_or(session.stable_since_unix_ms),
                    ScreenActivity::Stable => session.stable_since_unix_ms.saturating_add(stable_threshold_ms),
                };
                client.enqueue_control(MSG_CONTROL_ACTIVITY_EVENT, &ActivityEvent::ActivityChanged { session, changed_at_unix_ms })?;
            }
        }

        let mut removed_ids =
            client.known_activity_sessions.keys().filter(|session_id| !current_ids.contains(*session_id)).cloned().collect::<Vec<_>>();
        removed_ids.sort();
        for session_id in removed_ids {
            let Some(removed) = client.known_activity_sessions.remove(&session_id) else {
                continue;
            };
            client.enqueue_control(MSG_CONTROL_ACTIVITY_EVENT, &ActivityEvent::MembershipRemoved {
                session_id,
                tags: removed.tags,
                changed_at_unix_ms: unix_time_ms(),
            })?;
        }
    }
    Ok(())
}

fn directory_entry_matches_selectors(entry: &DirectoryEntry, selectors: &[String]) -> bool {
    selectors.iter().all(|selector| entry.tags.contains(selector))
}

fn broadcast_directory_upsert(entry: DirectoryEntry, packet_clients: &mut Vec<PacketClient>) -> Result<(), String> {
    for client in packet_clients {
        if directory_entry_matches_selectors(&entry, &client.selectors) {
            client.known_directory_sessions.insert(entry.session_id.clone());
            client.enqueue_control(MSG_CONTROL_DIRECTORY_DELTA, &DirectoryDelta {
                upserted: vec![entry.clone()],
                removed_session_ids: Vec::new(),
            })?;
        } else if client.known_directory_sessions.remove(&entry.session_id) {
            client.enqueue_control(MSG_CONTROL_DIRECTORY_DELTA, &DirectoryDelta {
                upserted: Vec::new(),
                removed_session_ids: vec![entry.session_id.clone()],
            })?;
        }
    }
    Ok(())
}

fn broadcast_directory_remove(session_id: &str, packet_clients: &mut Vec<PacketClient>) -> Result<(), String> {
    for client in packet_clients {
        if client.known_directory_sessions.remove(session_id) {
            client.enqueue_control(MSG_CONTROL_DIRECTORY_DELTA, &DirectoryDelta {
                upserted: Vec::new(),
                removed_session_ids: vec![session_id.to_string()],
            })?;
        }
    }
    Ok(())
}

#[cfg(not(debug_assertions))]
fn maybe_panic_for_containment_test(_session_id: &str) {}

#[cfg(debug_assertions)]
fn maybe_panic_for_containment_test(session_id: &str) {
    if std::env::var("CLEAT_TEST_PANIC_SESSION_TICK").as_deref() == Ok(session_id) {
        panic!("test-requested panic for session {session_id}");
    }
}

#[cfg(not(debug_assertions))]
fn maybe_fail_after_http_upgrade(_route: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(debug_assertions)]
fn maybe_fail_after_http_upgrade(route: &str) -> Result<(), String> {
    if std::env::var("CLEAT_TEST_FAIL_AFTER_HTTP_UPGRADE").as_deref() == Ok(route) {
        return Err(format!("test-requested failure after {route} upgrade"));
    }
    Ok(())
}

fn cleanup_exited_session(layout: &RuntimeLayout, id: &str, should_keep_session_dir: bool) {
    let session_dir = layout.session_dir(id);
    let _ = fs::remove_file(layout.foreground_path(id));
    if !should_keep_session_dir {
        let _ = fs::remove_dir_all(&session_dir);
    }
}

fn service_hosted_session(
    layout: &RuntimeLayout,
    id: &str,
    hosted: &mut HostedSession,
    packet_clients: &mut Vec<PacketClient>,
) -> Result<bool, String> {
    let mut did_work = false;
    let previous_controller_count = hosted.active_client.is_some();
    let previous_watcher_count = hosted.watchers.len();
    let mut resized = false;

    let output_drained =
        drain_raw_output_tap(layout, id, &hosted.actor, &mut hosted.raw_output_tap, &mut hosted.active_client, &mut hosted.watchers)?;
    did_work |= output_drained;
    drain_watcher_inputs(&mut hosted.watchers);

    if hosted.active_client.is_some() {
        let mut client_disconnected = false;
        let mut pending = VecDeque::new();
        if let Some(client) = hosted.active_client.as_mut() {
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
                    hosted.actor.write_input(bytes)?;
                }
                Frame::Resize { cols, rows } => {
                    hosted.actor.resize(cols, rows)?;
                    resized = true;
                }
                _ => {}
            }
        }

        if client_disconnected && hosted.active_client.is_some() {
            let _ = fs::remove_file(layout.foreground_path(id));
            hosted.actor.record_detach()?;
            hosted.actor.set_client_presence(false)?;
            hosted.active_client = None;
            did_work = true;
        }
    }

    let client_writable = match hosted.active_client.as_mut() {
        Some(client) => client.flush_pending_output()?,
        None => true,
    };
    if !client_writable {
        let _ = fs::remove_file(layout.foreground_path(id));
        hosted.actor.record_detach()?;
        hosted.actor.set_client_presence(false)?;
        hosted.active_client = None;
        did_work = true;
    }
    flush_watchers(&mut hosted.watchers);
    push_due_packet_renders(id, &hosted.actor, packet_clients, &mut hosted.packet_render_cache)?;
    if output_drained {
        hosted.actor.enqueue_screen_activity_flush()?;
    }
    if resized || previous_controller_count != hosted.active_client.is_some() || previous_watcher_count != hosted.watchers.len() {
        broadcast_directory_upsert(directory_entry_for_session(layout, hosted, packet_clients)?, packet_clients)?;
    }

    service_pending_waits(&hosted.actor, &mut hosted.pending_waits, &mut hosted.screen_stable_snapshot_generation);
    service_pending_expects(layout, id, &hosted.actor, &mut hosted.pending_expects)?;
    // Recording flush happens actor-side after each pump slice; no per-tick
    // round-trip here (ADR 0004: the servicing side is never blocked).

    Ok(did_work)
}

/// Whether a screen-stable wait needs a fresh full-grid fingerprint. An
/// unchanged render generation means the actor has fed nothing to the engine
/// since the last fingerprint, so the screen cannot have changed and the
/// existing stability window simply keeps aging without a snapshot.
fn screen_stable_needs_snapshot(current_generation: u64, last_snapshot_generation: Option<u64>) -> bool {
    last_snapshot_generation != Some(current_generation)
}

fn service_pending_waits(actor: &SessionActor, pending_waits: &mut Vec<PendingWait>, last_snapshot_generation: &mut Option<u64>) {
    let screen_stable_fingerprint = if pending_waits.iter().any(|wait| wait.screen_stable.is_some()) {
        // Read the generation before snapshotting: a pump landing in between
        // costs one redundant snapshot next tick instead of a missed change.
        let generation = actor.observation().render_generation();
        if screen_stable_needs_snapshot(generation, *last_snapshot_generation) {
            let fingerprint = actor.full_snapshot().ok().map(ScreenStableFingerprint::from_snapshot);
            if fingerprint.is_some() {
                *last_snapshot_generation = Some(generation);
            }
            fingerprint
        } else {
            None
        }
    } else {
        *last_snapshot_generation = None;
        None
    };
    let now = Instant::now();
    pending_waits.retain_mut(|wait| {
        let elapsed = wait.registered_at.elapsed();
        let elapsed_ms = elapsed.as_millis() as u64;

        if elapsed_ms >= wait.timeout_ms {
            let _ = write_http_wait_result(&mut wait.stream, crate::protocol::WaitStatus::Timeout, elapsed_ms);
            return false;
        }

        if let (Some(state), Some(fingerprint)) = (wait.screen_stable.as_mut(), screen_stable_fingerprint.as_ref()) {
            state.observe(fingerprint.clone(), now);
        }

        for condition in &wait.conditions {
            match condition {
                crate::protocol::WaitCondition::OutputIdle { quiet_ms } => {
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

        true
    });
}

fn service_pending_expects(
    layout: &RuntimeLayout,
    id: &str,
    actor: &SessionActor,
    pending_expects: &mut Vec<PendingExpect>,
) -> Result<(), String> {
    if pending_expects.is_empty() {
        return Ok(());
    }

    actor.flush_recording()?;
    let cast_path = layout.session_dir(id).join(crate::recording::CAST_FILE_NAME);
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
    Ok(())
}

fn finish_exited_session(
    layout: &RuntimeLayout,
    id: &str,
    hosted: &mut HostedSession,
    packet_clients: &mut [PacketClient],
) -> Result<bool, String> {
    drain_raw_output_tap(layout, id, &hosted.actor, &mut hosted.raw_output_tap, &mut hosted.active_client, &mut hosted.watchers)?;
    for mut wait in hosted.pending_waits.drain(..) {
        let elapsed_ms = wait.registered_at.elapsed().as_millis() as u64;
        let _ = write_http_wait_result(&mut wait.stream, crate::protocol::WaitStatus::SessionGone, elapsed_ms);
    }
    for mut expect in hosted.pending_expects.drain(..) {
        let elapsed_ms = expect.registered_at.elapsed().as_millis() as u64;
        let _ = write_http_wait_result(&mut expect.stream, crate::protocol::WaitStatus::SessionGone, elapsed_ms);
    }
    if let Some(client) = hosted.active_client.as_mut() {
        let _ = client.flush_pending_output();
    }
    flush_watchers(&mut hosted.watchers);
    flush_packet_clients(packet_clients);
    Ok(hosted.actor.should_keep_session_dir().unwrap_or(hosted.should_keep_session_dir))
}

#[cfg(not(any(unix, windows)))]
pub fn run_session_daemon(_root: &Path, _session: &SessionMetadata) -> Result<(), String> {
    Err("session daemon is only supported on unix".into())
}

struct HttpRequestState<'a> {
    layout: &'a RuntimeLayout,
    sessions: &'a mut HashMap<String, HostedSession>,
    packet_clients: &'a mut Vec<PacketClient>,
    next_packet_client_id: &'a mut u64,
}

#[cfg(any(unix, windows))]
struct PendingHttpHandshake {
    stream: SessionStream,
    buffer: Vec<u8>,
    deadline: Instant,
    #[cfg(windows)]
    reader: crate::platform::ipc::OverlappedRead,
}

enum HttpHandshakePoll {
    Pending(bool),
    Ready(Box<http_uds::HttpRequest>),
}

#[cfg(any(unix, windows))]
impl PendingHttpHandshake {
    fn new(stream: SessionStream) -> Result<Self, String> {
        set_stream_nonblocking(&stream, true)?;
        set_stream_write_timeout(&stream, Some(SESSION_HTTP_RESPONSE_WRITE_DEADLINE))?;
        #[cfg(windows)]
        let reader = stream.overlapped_reader(8192).map_err(|err| format!("create HTTP handshake reader: {err}"))?;
        Ok(Self {
            stream,
            buffer: Vec::new(),
            deadline: Instant::now() + SESSION_HTTP_HANDSHAKE_DEADLINE,
            #[cfg(windows)]
            reader,
        })
    }

    fn poll(&mut self) -> io::Result<HttpHandshakePoll> {
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "HTTP request handshake deadline exceeded"));
        }

        let mut did_work = false;
        loop {
            if self.buffer.len() >= 5 && !http_uds::looks_like_http_prefix(&self.buffer[..5]) {
                return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "connection did not start with HTTP"));
            }
            if let Some(request) = http_uds::try_parse_request(&self.buffer)? {
                return Ok(HttpHandshakePoll::Ready(Box::new(request)));
            }

            let Some(chunk) = self.read_available()? else {
                return Ok(HttpHandshakePoll::Pending(did_work));
            };
            if chunk.is_empty() {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed before HTTP request completed"));
            }
            self.buffer.extend_from_slice(&chunk);
            did_work = true;
        }
    }

    #[cfg(unix)]
    fn read_available(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut chunk = vec![0; 8192];
        match self.stream.read(&mut chunk) {
            Ok(read) => {
                chunk.truncate(read);
                Ok(Some(chunk))
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(err) => Err(err),
        }
    }

    #[cfg(windows)]
    fn read_available(&mut self) -> io::Result<Option<Vec<u8>>> {
        self.reader.poll()
    }

    fn into_stream(self) -> SessionStream {
        self.stream
    }
}

fn handle_http_request(
    _root: &Path,
    daemon_id: &str,
    stream: &mut SessionStream,
    request: http_uds::HttpRequest,
    state: &mut HttpRequestState<'_>,
    response_committed: &mut bool,
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
            let mut sessions = Vec::new();
            for hosted in state.sessions.values() {
                sessions.push(hosted.actor.inspect(hosted.active_client.is_some(), hosted.watchers.len())?);
            }
            sessions.sort_by(|a, b| a.session.id.cmp(&b.session.id));
            http_uds::write_json(stream, StatusCode::OK, &http_uds::SessionListResponse { sessions })
                .map_err(|err| format!("write HTTP sessions response: {err}"))
        }
        http_uds::Route::SessionCreate => {
            if let Some(expected_pid) = request.headers().get(http_uds::DAEMON_INSTANCE_HEADER) {
                let Some(expected_pid) = expected_pid.to_str().ok().and_then(|value| value.parse::<u32>().ok()) else {
                    return http_uds::write_error(stream, StatusCode::BAD_REQUEST, "invalid daemon instance header")
                        .map_err(|err| format!("write HTTP error response: {err}"));
                };
                if expected_pid != std::process::id() {
                    return http_uds::write_error(stream, StatusCode::CONFLICT, "source daemon instance changed")
                        .map_err(|err| format!("write HTTP error response: {err}"));
                }
            }
            let session: SessionMetadata =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP session create request: {err}"))?;
            crate::runtime::validate_runtime_name(&session.id)?;
            session.vt_engine.ensure_available()?;
            let mut created = false;
            if !state.sessions.contains_key(&session.id) {
                let session_dir = state.layout.session_dir(&session.id);
                fs::create_dir_all(&session_dir).map_err(|err| format!("create session dir {}: {err}", session_dir.display()))?;
                let coordinates = state.layout.session_coordinates(&session.id)?;
                let hosted = HostedSession::spawn(session_dir, session.clone(), coordinates)?;
                state.sessions.insert(session.id.clone(), hosted);
                created = true;
            }
            if created {
                if let Some(hosted) = state.sessions.get(&session.id) {
                    broadcast_directory_upsert(
                        directory_entry_for_session(state.layout, hosted, state.packet_clients)?,
                        state.packet_clients,
                    )?;
                }
            }
            let session =
                state.sessions.get(&session.id).ok_or_else(|| "created session disappeared before response".to_string())?.metadata.clone();
            http_uds::write_json(stream, StatusCode::OK, &http_uds::CreateSessionResponse { session })
                .map_err(|err| format!("write HTTP session create response: {err}"))
        }
        http_uds::Route::PacketConnect => {
            if !http_uds::request_has_upgrade_token(&request, "cleat-packet/1") {
                http_uds::write_error(stream, StatusCode::BAD_REQUEST, "missing Upgrade: cleat-packet/1")
                    .map_err(|err| format!("write HTTP packet upgrade error: {err}"))?;
                return Ok(());
            }

            let subscribe = if request.body().is_empty() {
                http_uds::PacketSubscribeRequest::default()
            } else {
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP directory subscribe request: {err}"))?
            };
            if subscribe.screen_activity_stable_ms == Some(0) {
                http_uds::write_error(stream, StatusCode::BAD_REQUEST, "screen activity stability threshold must be greater than zero")
                    .map_err(|err| format!("write HTTP activity subscribe error: {err}"))?;
                return Ok(());
            }
            let mut selectors = subscribe.selectors;
            crate::runtime::normalize_tags(&mut selectors);
            let directory = directory_snapshot_for_sessions(state.layout, state.sessions, state.packet_clients, &selectors)?;
            let activity = subscribe
                .screen_activity_stable_ms
                .map(|stable_threshold_ms| activity_snapshot_for_sessions(state.sessions, &selectors, stable_threshold_ms));
            let packet_stream = stream.try_clone().map_err(|err| format!("clone HTTP packet stream: {err}"))?;
            #[cfg(unix)]
            set_stream_nonblocking(&packet_stream, true).map_err(|err| format!("set HTTP packet stream nonblocking: {err}"))?;
            let client_id = *state.next_packet_client_id;
            let mut client =
                PacketClient::new(client_id, packet_stream, selectors, subscribe.screen_activity_stable_ms, &directory, activity.as_ref())?;
            client.enqueue_control(MSG_CONTROL_HELLO, &ControlHello::current())?;
            client.enqueue_control(MSG_CONTROL_DIRECTORY_SNAPSHOT, &directory)?;
            if let Some(activity) = activity {
                client.enqueue_control(MSG_CONTROL_ACTIVITY_SNAPSHOT, &activity)?;
            }
            *response_committed = true;
            http_uds::write_packet_switching_protocols(stream).map_err(|err| format!("write HTTP packet upgrade response: {err}"))?;
            maybe_fail_after_http_upgrade("packet")?;
            *state.next_packet_client_id += 1;
            state.packet_clients.push(client);
            Ok(())
        }
        http_uds::Route::SessionInspect { id } => {
            let Some(hosted) = state.sessions.get(&id) else {
                return write_http_not_found(stream);
            };
            let result = hosted.actor.inspect(hosted.active_client.is_some(), hosted.watchers.len())?;
            http_uds::write_json(stream, StatusCode::OK, &result).map_err(|err| format!("write HTTP inspect response: {err}"))
        }
        http_uds::Route::SessionDelete { id } => {
            let Some(hosted) = state.sessions.get(&id) else {
                return write_http_not_found(stream);
            };
            hosted.actor.dispatch_signal(TERMINATE_SIGNAL, crate::protocol::SignalTarget::Tree)?;
            http_uds::write_no_content(stream).map_err(|err| format!("write HTTP delete response: {err}"))
        }
        http_uds::Route::SessionAttach { id } => 'attach: {
            let Some(hosted) = state.sessions.get_mut(&id) else {
                break 'attach write_http_not_found(stream);
            };
            let body: http_uds::AttachRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP attach request: {err}"))?;
            vacate_dead_packet_controller(hosted, state.packet_clients);
            if hosted.active_client.is_some() || hosted.packet_controller.is_some() {
                http_uds::write_error(stream, StatusCode::CONFLICT, "session already has a foreground client")
                    .map_err(|err| format!("write HTTP attach busy response: {err}"))?;
                break 'attach Ok(());
            }

            let capabilities = attach_capabilities_from_http(body.capabilities);
            let replay = hosted.actor.apply_attach_state(body.cols, body.rows, capabilities)?;
            let attach_stream = stream.try_clone().map_err(|err| format!("clone HTTP attach stream: {err}"))?;
            let mut client = ActiveClient::new(attach_stream, capabilities)?;
            let replay_mode = if hosted.had_foreground_client { ReplayMode::ResetTerminal } else { ReplayMode::FreshTerminal };
            drain_raw_output_tap_before_client_install(state.layout, &id, hosted, &mut client, replay, replay_mode)?;
            *response_committed = true;
            http_uds::write_switching_protocols(stream).map_err(|err| format!("write HTTP attach upgrade response: {err}"))?;
            maybe_fail_after_http_upgrade("attach")?;
            #[cfg(unix)]
            set_stream_nonblocking(&client.stream, true).map_err(|err| format!("set HTTP attach stream nonblocking: {err}"))?;
            let _ = fs::write(state.layout.foreground_path(&id), b"1");
            hosted.active_client = Some(client);
            let activation = (|| {
                hosted.actor.set_client_presence(true)?;
                hosted.actor.record_attach()?;
                broadcast_directory_upsert(directory_entry_for_session(state.layout, hosted, state.packet_clients)?, state.packet_clients)
            })();
            if let Err(err) = activation {
                hosted.active_client = None;
                let _ = hosted.actor.set_client_presence(false);
                let _ = fs::remove_file(state.layout.foreground_path(&id));
                return Err(err);
            }
            hosted.had_foreground_client = true;
            Ok(())
        }
        http_uds::Route::SessionWatch { id } => {
            let Some(hosted) = state.sessions.get_mut(&id) else {
                return write_http_not_found(stream);
            };
            let body: http_uds::AttachRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP watch request: {err}"))?;
            let capabilities = attach_capabilities_from_http(body.capabilities);
            let replay = hosted.actor.replay_payload(capabilities)?;
            let watch_stream = stream.try_clone().map_err(|err| format!("clone HTTP watch stream: {err}"))?;
            let mut watcher = ActiveClient::new(watch_stream, capabilities)?;
            drain_raw_output_tap_before_client_install(state.layout, &id, hosted, &mut watcher, replay, ReplayMode::FreshTerminal)?;
            *response_committed = true;
            http_uds::write_switching_protocols(stream).map_err(|err| format!("write HTTP watch upgrade response: {err}"))?;
            maybe_fail_after_http_upgrade("watch")?;
            #[cfg(unix)]
            set_stream_nonblocking(&watcher.stream, true).map_err(|err| format!("set HTTP watch stream nonblocking: {err}"))?;
            hosted.watchers.push(watcher);
            let activation = (|| {
                broadcast_directory_upsert(directory_entry_for_session(state.layout, hosted, state.packet_clients)?, state.packet_clients)
            })();
            if let Err(err) = activation {
                hosted.watchers.pop();
                return Err(err);
            }
            Ok(())
        }
        http_uds::Route::SessionDetach { id } => {
            let Some(hosted) = state.sessions.get_mut(&id) else {
                return write_http_not_found(stream);
            };
            let _ = fs::remove_file(state.layout.foreground_path(&id));
            if hosted.active_client.is_some() {
                hosted.actor.record_detach()?;
            }
            hosted.actor.set_client_presence(false)?;
            hosted.active_client = None;
            broadcast_directory_upsert(directory_entry_for_session(state.layout, hosted, state.packet_clients)?, state.packet_clients)?;
            http_uds::write_no_content(stream).map_err(|err| format!("write HTTP detach response: {err}"))
        }
        http_uds::Route::SessionExpect { id } => 'expect: {
            let Some(hosted) = state.sessions.get_mut(&id) else {
                break 'expect write_http_not_found(stream);
            };
            let body: http_uds::ExpectRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP expect request: {err}"))?;
            if !hosted.actor.recording_active()? {
                http_uds::write_error(stream, StatusCode::CONFLICT, "recording not active")
                    .map_err(|err| format!("write HTTP expect error: {err}"))?;
                break 'expect Ok(());
            }

            let cast_path = state.layout.session_dir(&id).join(crate::recording::CAST_FILE_NAME);
            hosted.actor.flush_recording()?;
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
            hosted.pending_expects.push(PendingExpect {
                stream: pending_stream,
                text: body.text,
                since_offset: body.since_offset,
                last_checked_file_size: initial_file_size,
                timeout_ms: body.timeout_ms,
                registered_at: Instant::now(),
            });
            Ok(())
        }
        http_uds::Route::SessionInput { id } => {
            let Some(hosted) = state.sessions.get_mut(&id) else {
                return write_http_not_found(stream);
            };
            let body: http_uds::InputRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP input request: {err}"))?;
            match body {
                http_uds::InputRequest::Text { text } => {
                    hosted.actor.write_input(text.into_bytes())?;
                }
                http_uds::InputRequest::Paste { text } => {
                    hosted.actor.paste(text.into_bytes())?;
                }
                http_uds::InputRequest::Key { key } => {
                    let bytes = http_input_key_bytes(key);
                    hosted.actor.write_input(bytes)?;
                }
                http_uds::InputRequest::RawBytes { bytes } => {
                    hosted.actor.write_input(bytes)?;
                }
                http_uds::InputRequest::Resize { cols, rows } => {
                    hosted.actor.resize(cols, rows)?;
                    broadcast_directory_upsert(
                        directory_entry_for_session(state.layout, hosted, state.packet_clients)?,
                        state.packet_clients,
                    )?;
                }
            }
            http_uds::write_no_content(stream).map_err(|err| format!("write HTTP input response: {err}"))
        }
        http_uds::Route::SessionKeys { id } => {
            let Some(hosted) = state.sessions.get(&id) else {
                return write_http_not_found(stream);
            };
            let body: http_uds::KeysRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP keys request: {err}"))?;
            hosted.actor.write_input(body.bytes)?;
            http_uds::write_no_content(stream).map_err(|err| format!("write HTTP keys response: {err}"))
        }
        http_uds::Route::SessionKeysWithMark { id } => {
            let Some(hosted) = state.sessions.get(&id) else {
                return write_http_not_found(stream);
            };
            let body: http_uds::KeysWithMarkRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP keys-with-mark request: {err}"))?;
            let offset = hosted.actor.write_input_with_mark(body.bytes, body.marker_name)?;
            http_uds::write_json(stream, StatusCode::OK, &http_uds::MarkResponse { offset })
                .map_err(|err| format!("write HTTP keys-with-mark response: {err}"))
        }
        http_uds::Route::SessionPasteWithMark { id } => {
            let Some(hosted) = state.sessions.get(&id) else {
                return write_http_not_found(stream);
            };
            let body: http_uds::PasteWithMarkRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP paste-with-mark request: {err}"))?;
            let offset = hosted.actor.paste_with_mark(body.text.into_bytes(), body.marker_name)?;
            http_uds::write_json(stream, StatusCode::OK, &http_uds::MarkResponse { offset })
                .map_err(|err| format!("write HTTP paste-with-mark response: {err}"))
        }
        http_uds::Route::SessionRecord { id } => {
            let Some(hosted) = state.sessions.get_mut(&id) else {
                return write_http_not_found(stream);
            };
            let body: http_uds::RecordRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP record request: {err}"))?;
            hosted.actor.set_recording(body.enable)?;
            hosted.metadata.record = body.enable;
            http_uds::write_no_content(stream).map_err(|err| format!("write HTTP record response: {err}"))
        }
        http_uds::Route::SessionTags { id } => {
            let Some(hosted) = state.sessions.get_mut(&id) else {
                return write_http_not_found(stream);
            };
            let body: http_uds::TagRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP tag request: {err}"))?;
            let tags = hosted.actor.update_tags(body.add, body.remove)?;
            hosted.metadata.tags.clone_from(&tags);
            broadcast_directory_upsert(directory_entry_for_session(state.layout, hosted, state.packet_clients)?, state.packet_clients)?;
            http_uds::write_json(stream, StatusCode::OK, &http_uds::TagResponse { tags })
                .map_err(|err| format!("write HTTP tag response: {err}"))
        }
        http_uds::Route::SessionMark { id } => {
            let Some(hosted) = state.sessions.get(&id) else {
                return write_http_not_found(stream);
            };
            let body: http_uds::MarkRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP mark request: {err}"))?;
            let offset = hosted.actor.mark(body.name)?;
            http_uds::write_json(stream, StatusCode::OK, &http_uds::MarkResponse { offset })
                .map_err(|err| format!("write HTTP mark response: {err}"))
        }
        http_uds::Route::SessionResolveMarker { id } => {
            let Some(hosted) = state.sessions.get(&id) else {
                return write_http_not_found(stream);
            };
            let body: http_uds::ResolveMarkerRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP resolve-marker request: {err}"))?;
            let marker_name = body.name;
            match hosted.actor.resolve_marker(marker_name.clone())? {
                Some(offset) => http_uds::write_json(stream, StatusCode::OK, &http_uds::MarkResponse { offset })
                    .map_err(|err| format!("write HTTP resolve-marker response: {err}")),
                None => http_uds::write_error(stream, StatusCode::NOT_FOUND, &format!("marker not found: {marker_name}"))
                    .map_err(|err| format!("write HTTP resolve-marker error: {err}")),
            }
        }
        http_uds::Route::SessionResolveNextMarker { id } => {
            let Some(hosted) = state.sessions.get(&id) else {
                return write_http_not_found(stream);
            };
            let body: http_uds::ResolveNextMarkerRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP resolve-next-marker request: {err}"))?;
            let offset = hosted.actor.resolve_next_marker_after(body.after)?;
            http_uds::write_json(stream, StatusCode::OK, &http_uds::ResolveNextMarkerResponse { offset })
                .map_err(|err| format!("write HTTP resolve-next-marker response: {err}"))
        }
        http_uds::Route::SessionResize { id } => {
            let Some(hosted) = state.sessions.get(&id) else {
                return write_http_not_found(stream);
            };
            let body: http_uds::ResizeRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP resize request: {err}"))?;
            hosted.actor.resize(body.cols, body.rows)?;
            broadcast_directory_upsert(directory_entry_for_session(state.layout, hosted, state.packet_clients)?, state.packet_clients)?;
            http_uds::write_no_content(stream).map_err(|err| format!("write HTTP resize response: {err}"))
        }
        http_uds::Route::SessionScreen { id } => {
            let Some(hosted) = state.sessions.get(&id) else {
                return write_http_not_found(stream);
            };
            match hosted.actor.capture_text() {
                Ok(text) => http_uds::write_json(stream, StatusCode::OK, &http_uds::ScreenResponse { text })
                    .map_err(|err| format!("write HTTP screen response: {err}")),
                Err(err) => {
                    http_uds::write_error(stream, StatusCode::CONFLICT, &err).map_err(|err| format!("write HTTP screen error: {err}"))
                }
            }
        }
        http_uds::Route::SessionSignal { id } => {
            let Some(hosted) = state.sessions.get(&id) else {
                return write_http_not_found(stream);
            };
            let body: http_uds::SignalRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP signal request: {err}"))?;
            hosted.actor.dispatch_signal(body.signal, signal_target_from_http(body.target))?;
            http_uds::write_no_content(stream).map_err(|err| format!("write HTTP signal response: {err}"))
        }
        http_uds::Route::SessionSnapshot { id } => {
            let Some(hosted) = state.sessions.get(&id) else {
                return write_http_not_found(stream);
            };
            match hosted.actor.full_snapshot() {
                Ok(snapshot) => http_uds::write_json(stream, StatusCode::OK, &http_uds::snapshot_response(snapshot))
                    .map_err(|err| format!("write HTTP snapshot response: {err}")),
                Err(err) => {
                    http_uds::write_error(stream, StatusCode::CONFLICT, &err).map_err(|err| format!("write HTTP snapshot error: {err}"))
                }
            }
        }
        http_uds::Route::SessionWait { id } => 'wait: {
            let Some(hosted) = state.sessions.get_mut(&id) else {
                break 'wait write_http_not_found(stream);
            };
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
                if let Err(err) = hosted.actor.validate_text_matching() {
                    http_uds::write_error(stream, StatusCode::CONFLICT, &format!("text matching not supported: {err}"))
                        .map_err(|err| format!("write HTTP wait error: {err}"))?;
                    break 'wait Ok(());
                }
            }
            let registered_at = Instant::now();
            let screen_stable = if has_screen_stable {
                // Read the generation before snapshotting (same conservative
                // ordering as service_pending_waits) and seed the snapshot
                // memo, so the next servicing tick doesn't immediately take a
                // second full-grid snapshot right after this one.
                let generation = hosted.actor.observation().render_generation();
                match hosted.actor.full_snapshot() {
                    Ok(snapshot) => {
                        hosted.screen_stable_snapshot_generation = Some(generation);
                        Some(ScreenStableState::new(ScreenStableFingerprint::from_snapshot(snapshot), registered_at))
                    }
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
                        if hosted.actor.screen_contains(text.clone())? {
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
            hosted.pending_waits.push(PendingWait {
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

fn write_http_not_found(stream: &mut SessionStream) -> Result<(), String> {
    http_uds::write_error(stream, StatusCode::NOT_FOUND, "not found").map_err(|err| format!("write HTTP not found response: {err}"))
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
    id: u64,
    stream: SessionStream,
    pending_output: PendingOutput,
    input_reader: ActiveClientReader,
    input_buffer: Vec<u8>,
    channels: HashMap<u32, PacketSessionChannel>,
    selectors: Vec<String>,
    screen_activity_stable_ms: Option<u64>,
    known_activity_sessions: HashMap<String, ActivitySession>,
    known_directory_sessions: HashSet<String>,
    /// Marked when this client's transport fails or its output backlog
    /// overflows. Client failures are never daemon-fatal: the client is
    /// reaped (with role release) on the next servicing pass.
    dead: bool,
}

struct PacketSessionChannel {
    session_id: String,
    role: ChannelRole,
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
    fn new(
        id: u64,
        stream: SessionStream,
        selectors: Vec<String>,
        screen_activity_stable_ms: Option<u64>,
        initial_directory: &DirectorySnapshot,
        initial_activity: Option<&ActivitySnapshot>,
    ) -> Result<Self, String> {
        let input_reader = ActiveClientReader::new(&stream)?;
        Ok(Self {
            id,
            stream,
            pending_output: PendingOutput::new(),
            input_reader,
            input_buffer: Vec::new(),
            channels: HashMap::new(),
            selectors,
            screen_activity_stable_ms,
            known_activity_sessions: initial_activity
                .into_iter()
                .flat_map(|snapshot| &snapshot.sessions)
                .map(|session| (session.session_id.clone(), session.clone()))
                .collect(),
            known_directory_sessions: initial_directory.sessions.iter().map(|entry| entry.session_id.clone()).collect(),
            dead: false,
        })
    }

    fn enqueue_control<T: serde::Serialize>(&mut self, msg_type: u8, value: &T) -> Result<(), String> {
        let frame = PacketFrame::new(CHANNEL_CONTROL, msg_type, value).map_err(|err| format!("encode packet control frame: {err}"))?;
        self.enqueue_frame(&frame)
    }

    /// Errors only on encode failures (a programming error in frame
    /// construction). A reader that stalls long enough to overflow its
    /// backlog is a client-scoped failure: the client is marked dead and
    /// reaped later, never surfaced as a daemon-level error.
    fn enqueue_frame(&mut self, frame: &PacketFrame) -> Result<(), String> {
        if self.dead {
            return Ok(());
        }
        if self.pending_output.len().saturating_add(frame.encoded_len()) > MAX_PENDING_CLIENT_OUTPUT_BYTES {
            self.dead = true;
            self.pending_output = PendingOutput::new();
            return Ok(());
        }
        frame.write(&mut self.pending_output).map_err(|err| format!("buffer packet frame: {err}"))
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
            match self.stream.write(self.pending_output.as_slice()) {
                Ok(0) => return Ok(false),
                Ok(n) => {
                    self.pending_output.consume(n);
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
    layout: &RuntimeLayout,
    sessions: &mut HashMap<String, HostedSession>,
    packet_clients: &mut Vec<PacketClient>,
) -> Result<bool, String> {
    let mut did_work = false;
    let mut index = 0;
    while index < packet_clients.len() {
        let mut pending = VecDeque::new();
        // A transport read error is a client-scoped failure, exactly like a
        // clean disconnect: drop the one client, never the daemon.
        let connected =
            !packet_clients[index].dead && packet_clients[index].drain_input_frames(&mut pending, Duration::ZERO).unwrap_or(false);
        if !connected {
            let removed = packet_clients.swap_remove(index);
            release_packet_client_roles(layout, sessions, &removed, packet_clients)?;
            did_work = true;
            continue;
        }

        while let Some(frame) = pending.pop_front() {
            did_work = true;
            // A frame-handling error is scoped to the client that sent the
            // frame (its channel bookkeeping is suspect from here on): drop
            // that client, keep the daemon and its sessions running.
            match handle_packet_frame(layout, sessions, packet_clients, index, frame) {
                Ok(updates) => {
                    for entry in updates {
                        broadcast_directory_upsert(entry, packet_clients)?;
                    }
                }
                Err(err) => {
                    eprintln!("dropping packet client after frame error: {err}");
                    packet_clients[index].dead = true;
                    break;
                }
            }
        }
        index += 1;
    }
    Ok(did_work)
}

/// Free controller slots held by a disconnected packet client and re-announce
/// the affected sessions' attachment counts.
fn release_packet_client_roles(
    layout: &RuntimeLayout,
    sessions: &mut HashMap<String, HostedSession>,
    removed: &PacketClient,
    packet_clients: &mut Vec<PacketClient>,
) -> Result<(), String> {
    let mut affected: Vec<&String> = Vec::new();
    for (channel, session_channel) in &removed.channels {
        if let Some(hosted) = sessions.get_mut(&session_channel.session_id) {
            if hosted.packet_controller == Some(PacketChannelRef { client_id: removed.id, channel: *channel }) {
                hosted.packet_controller = None;
            }
        }
        affected.push(&session_channel.session_id);
    }
    affected.sort();
    affected.dedup();
    for session_id in affected {
        if let Some(hosted) = sessions.get(session_id) {
            // A failing session actor must not turn role release into a
            // daemon-fatal error; the session faults on its own servicing
            // pass and broadcasts its removal there.
            match directory_entry_for_session(layout, hosted, packet_clients) {
                Ok(entry) => broadcast_directory_upsert(entry, packet_clients)?,
                Err(err) => eprintln!("skipping directory update for session {session_id}: {err}"),
            }
        }
    }
    Ok(())
}

/// A dead client awaiting reaping must not hold the controller slot against
/// a live requester (packet role grants and legacy attach alike).
fn vacate_dead_packet_controller(hosted: &mut HostedSession, packet_clients: &[PacketClient]) {
    if let Some(current) = hosted.packet_controller {
        if !packet_controller_holder_is_live(current, packet_clients) {
            hosted.packet_controller = None;
        }
    }
}

fn packet_controller_holder_is_live(current: PacketChannelRef, packet_clients: &[PacketClient]) -> bool {
    packet_clients.iter().any(|client| client.id == current.client_id && !client.dead)
}

/// Resolve a controller/watcher request for `requester`, demoting the current
/// packet controller when `take` is set. A legacy stream controller is never
/// preempted (a raw attach cannot be demoted to read-only), so requests grant
/// Watcher while one is attached.
fn grant_packet_role(
    hosted: &mut HostedSession,
    packet_clients: &mut [PacketClient],
    requester: PacketChannelRef,
    role: ChannelRole,
    take: bool,
) -> Result<ChannelRole, String> {
    match role {
        ChannelRole::Watcher => {
            if hosted.packet_controller == Some(requester) {
                hosted.packet_controller = None;
            }
            Ok(ChannelRole::Watcher)
        }
        ChannelRole::Controller => {
            if hosted.active_client.is_some() {
                return Ok(ChannelRole::Watcher);
            }
            vacate_dead_packet_controller(hosted, packet_clients);
            match hosted.packet_controller {
                None => {
                    hosted.packet_controller = Some(requester);
                    Ok(ChannelRole::Controller)
                }
                Some(current) if current == requester => Ok(ChannelRole::Controller),
                Some(current) => {
                    if !take {
                        return Ok(ChannelRole::Watcher);
                    }
                    for client in packet_clients.iter_mut() {
                        if client.id != current.client_id {
                            continue;
                        }
                        if let Some(session_channel) = client.channels.get_mut(&current.channel) {
                            session_channel.role = ChannelRole::Watcher;
                        }
                        client.enqueue_frame(
                            &PacketFrame::new(current.channel, MSG_SESSION_ROLE, &RoleState { role: ChannelRole::Watcher })
                                .map_err(|err| format!("encode role demotion packet: {err}"))?,
                        )?;
                        break;
                    }
                    hosted.packet_controller = Some(requester);
                    Ok(ChannelRole::Controller)
                }
            }
        }
    }
}

fn handle_packet_frame(
    layout: &RuntimeLayout,
    sessions: &mut HashMap<String, HostedSession>,
    packet_clients: &mut [PacketClient],
    index: usize,
    frame: PacketFrame,
) -> Result<Vec<DirectoryEntry>, String> {
    let mut updates = Vec::new();
    match (frame.channel, frame.msg_type) {
        (CHANNEL_CONTROL, MSG_CONTROL_OPEN_CHANNEL) => {
            let open = frame.decode::<OpenChannel>().map_err(|err| format!("decode open-channel packet: {err}"))?;
            open_packet_channel(layout, sessions, packet_clients, index, open, &mut updates)?;
        }
        (CHANNEL_CONTROL, MSG_CONTROL_CLOSE_CHANNEL) => {
            let close = frame.decode::<CloseChannel>().map_err(|err| format!("decode close-channel packet: {err}"))?;
            let client_id = packet_clients[index].id;
            if let Some(removed) = packet_clients[index].channels.remove(&close.channel) {
                if let Some(hosted) = sessions.get_mut(&removed.session_id) {
                    if hosted.packet_controller == Some(PacketChannelRef { client_id, channel: close.channel }) {
                        hosted.packet_controller = None;
                    }
                }
                if let Some(hosted) = sessions.get(&removed.session_id) {
                    updates.push(directory_entry_for_session(layout, hosted, packet_clients)?);
                }
            }
        }
        (channel, MSG_SESSION_ACK) if channel != CHANNEL_CONTROL => {
            let ack = frame.decode::<Ack>().map_err(|err| format!("decode ack packet: {err}"))?;
            let client = &mut packet_clients[index];
            let session_id = client.channels.get(&channel).map(|session| session.session_id.clone());
            if let Some(session_channel) = client.channels.get_mut(&channel) {
                if session_channel.in_flight_generation == Some(ack.generation) {
                    session_channel.in_flight_generation = None;
                    if let Some(hosted) = session_id.and_then(|id| sessions.get(&id)) {
                        hosted.actor.mark_observed(ack.generation);
                    }
                }
            }
        }
        (channel, MSG_SESSION_INPUT) if channel != CHANNEL_CONTROL => {
            let input = frame.decode::<Input>().map_err(|err| format!("decode input packet: {err}"))?;
            if let Some(hosted) = controller_session(sessions, packet_clients, index, channel) {
                route_packet_input_event(&hosted.actor, input.event)?;
            }
        }
        (channel, MSG_SESSION_RESIZE) if channel != CHANNEL_CONTROL => {
            let resize = frame.decode::<Resize>().map_err(|err| format!("decode resize packet: {err}"))?;
            if let Some(hosted) = controller_session(sessions, packet_clients, index, channel) {
                hosted.actor.resize(resize.cols, resize.rows)?;
                updates.push(directory_entry_for_session(layout, hosted, packet_clients)?);
            }
        }
        (channel, MSG_SESSION_VIEWPORT) if channel != CHANNEL_CONTROL => {
            let viewport = frame.decode::<crate::packet::Viewport>().map_err(|err| format!("decode viewport packet: {err}"))?;
            // the viewport is session-global state; outcome flows back as
            // viewport/scrollbar state in the next render packet; no reply
            if let Some(hosted) = controller_session(sessions, packet_clients, index, channel) {
                let _ = hosted
                    .actor
                    .request_result(|reply| crate::host::actor::SessionCommand::ScrollViewport { command: viewport.command, reply });
            }
        }
        (channel, MSG_SESSION_ROLE) if channel != CHANNEL_CONTROL => {
            let request = frame.decode::<RoleRequest>().map_err(|err| format!("decode role request packet: {err}"))?;
            apply_packet_role_request(layout, sessions, packet_clients, index, channel, request, &mut updates)?;
        }
        _ => {}
    }
    Ok(updates)
}

/// The session a channel controls: `Some` only when the channel exists and
/// holds the controller role. Watcher traffic on session-mutating messages
/// is dropped by role, not errored.
fn controller_session<'a>(
    sessions: &'a HashMap<String, HostedSession>,
    packet_clients: &[PacketClient],
    index: usize,
    channel: u32,
) -> Option<&'a HostedSession> {
    let session_channel = packet_clients[index].channels.get(&channel)?;
    if session_channel.role != ChannelRole::Controller {
        return None;
    }
    sessions.get(&session_channel.session_id)
}

fn apply_packet_role_request(
    layout: &RuntimeLayout,
    sessions: &mut HashMap<String, HostedSession>,
    packet_clients: &mut [PacketClient],
    index: usize,
    channel: u32,
    request: RoleRequest,
    updates: &mut Vec<DirectoryEntry>,
) -> Result<(), String> {
    let Some(session_id) = packet_clients[index].channels.get(&channel).map(|session| session.session_id.clone()) else {
        return Ok(());
    };
    let Some(hosted) = sessions.get_mut(&session_id) else {
        return Ok(());
    };
    let requester = PacketChannelRef { client_id: packet_clients[index].id, channel };
    let granted = grant_packet_role(hosted, packet_clients, requester, request.role, request.take)?;
    let client = &mut packet_clients[index];
    if let Some(session_channel) = client.channels.get_mut(&channel) {
        session_channel.role = granted;
    }
    client.enqueue_frame(
        &PacketFrame::new(channel, MSG_SESSION_ROLE, &RoleState { role: granted })
            .map_err(|err| format!("encode role state packet: {err}"))?,
    )?;
    if let Some(hosted) = sessions.get(&session_id) {
        updates.push(directory_entry_for_session(layout, hosted, packet_clients)?);
    }
    Ok(())
}

fn open_packet_channel(
    layout: &RuntimeLayout,
    sessions: &mut HashMap<String, HostedSession>,
    packet_clients: &mut [PacketClient],
    index: usize,
    open: OpenChannel,
    updates: &mut Vec<DirectoryEntry>,
) -> Result<(), String> {
    if open.channel == CHANNEL_CONTROL {
        packet_clients[index].enqueue_control(MSG_CONTROL_ERROR, &ControlError {
            channel: open.channel,
            message: "session channel must be non-zero".to_string(),
        })?;
        return Ok(());
    }
    let Some(hosted) = sessions.get_mut(&open.session_id) else {
        packet_clients[index].enqueue_control(MSG_CONTROL_ERROR, &ControlError {
            channel: open.channel,
            message: format!("unknown session {}", open.session_id),
        })?;
        return Ok(());
    };

    // Probe render state before granting a role: a session whose VT engine
    // cannot serve it (e.g. the passthrough placeholder) must fail this one
    // channel, not demote the current controller or tear down the daemon.
    let update = match hosted.actor.full_render_update() {
        Ok(update) => update,
        Err(err) => {
            packet_clients[index].enqueue_control(MSG_CONTROL_ERROR, &ControlError {
                channel: open.channel,
                message: format!("open channel for session {}: {err}", open.session_id),
            })?;
            return Ok(());
        }
    };
    let requester = PacketChannelRef { client_id: packet_clients[index].id, channel: open.channel };
    let granted = grant_packet_role(hosted, packet_clients, requester, open.role, open.take)?;
    let session_id = open.session_id;
    let generation = update.render_generation;
    hosted.packet_render_cache.store(update.clone());
    let client = &mut packet_clients[index];
    client.enqueue_frame(
        &PacketFrame::new(open.channel, MSG_SESSION_ROLE, &RoleState { role: granted })
            .map_err(|err| format!("encode role state packet: {err}"))?,
    )?;
    client.enqueue_frame(
        &PacketFrame::new(open.channel, MSG_SESSION_RENDER, &RenderPacket { update })
            .map_err(|err| format!("encode initial render packet: {err}"))?,
    )?;
    client.channels.insert(open.channel, PacketSessionChannel {
        session_id: session_id.clone(),
        role: granted,
        in_flight_generation: Some(generation),
        last_sent_generation: generation,
    });
    if let Some(hosted) = sessions.get(&session_id) {
        updates.push(directory_entry_for_session(layout, hosted, packet_clients)?);
    }
    Ok(())
}

fn push_due_packet_renders(
    session_id: &str,
    actor: &SessionActor,
    packet_clients: &mut Vec<PacketClient>,
    render_cache: &mut PacketRenderCache,
) -> Result<(), String> {
    if !packet_clients.iter().any(|client| client.channels.values().any(|channel| channel.session_id == session_id)) {
        return Ok(());
    }

    if actor.observation().dirty() != DirtyState::Clean {
        let update = if has_packet_channel_lagging_cached_generation(session_id, packet_clients, render_cache) {
            actor.full_render_update()?
        } else {
            actor.render_update()?
        };
        render_cache.store(update);
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
                (session.session_id == session_id
                    && session.in_flight_generation.is_none()
                    && session.last_sent_generation < latest_generation)
                    .then_some(*channel)
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

fn has_packet_channel_lagging_cached_generation(
    session_id: &str,
    packet_clients: &[PacketClient],
    render_cache: &PacketRenderCache,
) -> bool {
    let Some(latest_generation) = render_cache.latest_generation() else {
        return false;
    };
    packet_clients
        .iter()
        .flat_map(|client| client.channels.values())
        .any(|session| session.session_id == session_id && session.last_sent_generation < latest_generation)
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

/// Flush failures mark the client dead rather than dropping it here:
/// removal happens in `service_packet_clients`, which also releases any
/// controller role the client held.
fn flush_packet_clients(packet_clients: &mut [PacketClient]) {
    for client in packet_clients {
        if client.dead {
            continue;
        }
        if !client.flush_pending_output().unwrap_or(false) {
            client.dead = true;
        }
    }
}

/// Bytes queued for a client socket, consumed from the front on partial
/// writes. A start cursor makes consumption O(1) instead of `Vec::drain`'s
/// per-write memmove of the whole backlog (issue #135). The consumed prefix
/// is compacted away once it outgrows the live remainder, so total memmove
/// work is bounded by total bytes queued (amortized O(1) per byte).
struct PendingOutput {
    buf: Vec<u8>,
    start: usize,
}

impl PendingOutput {
    fn new() -> Self {
        Self { buf: Vec::new(), start: 0 }
    }

    /// Bytes not yet written to the socket — the quantity capped by
    /// `MAX_PENDING_CLIENT_OUTPUT_BYTES`, exactly what `Vec::len` measured
    /// before the cursor existed.
    fn len(&self) -> usize {
        self.buf.len() - self.start
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn as_slice(&self) -> &[u8] {
        &self.buf[self.start..]
    }

    fn consume(&mut self, n: usize) {
        self.start += n;
        debug_assert!(self.start <= self.buf.len());
        if self.start >= self.buf.len() {
            self.buf.clear();
            self.start = 0;
        } else if self.start > self.len() {
            // The dead prefix outgrew the live remainder: one memmove that
            // touches fewer bytes than were consumed since the last
            // compaction keeps append targets (and memory) bounded.
            self.buf.copy_within(self.start.., 0);
            let remaining = self.buf.len() - self.start;
            self.buf.truncate(remaining);
            self.start = 0;
        }
    }
}

/// Frames append their wire encoding straight into the backlog through
/// `io::Write`, so enqueueing copies each payload exactly once — no
/// intermediate encode buffer.
impl Write for PendingOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
impl From<Vec<u8>> for PendingOutput {
    fn from(buf: Vec<u8>) -> Self {
        Self { buf, start: 0 }
    }
}

struct ActiveClient {
    stream: SessionStream,
    pending_output: PendingOutput,
    input_reader: ActiveClientReader,
    input_buffer: Vec<u8>,
    capabilities: vt::ClientCapabilities,
}

impl ActiveClient {
    fn new(stream: SessionStream, capabilities: vt::ClientCapabilities) -> Result<Self, String> {
        let input_reader = ActiveClientReader::new(&stream)?;
        Ok(Self { stream, pending_output: PendingOutput::new(), input_reader, input_buffer: Vec::new(), capabilities })
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
        if self.pending_output.len().saturating_add(frame.encoded_len()) > MAX_PENDING_CLIENT_OUTPUT_BYTES {
            return Err(format!("client output backlog exceeded {} bytes", MAX_PENDING_CLIENT_OUTPUT_BYTES));
        }
        frame.write(&mut self.pending_output).map_err(|err| format!("buffer client frame: {err}"))
    }

    /// Frames a raw PTY output chunk straight into the backlog — header plus
    /// one payload copy, with no intermediate `Frame` allocation. This is the
    /// per-byte fan-out hot path (issue #135).
    fn enqueue_output(&mut self, payload: &[u8]) -> Result<(), String> {
        if self.pending_output.len().saturating_add(Frame::output_encoded_len(payload.len())) > MAX_PENDING_CLIENT_OUTPUT_BYTES {
            return Err(format!("client output backlog exceeded {} bytes", MAX_PENDING_CLIENT_OUTPUT_BYTES));
        }
        Frame::write_output(&mut self.pending_output, payload).map_err(|err| format!("buffer client frame: {err}"))
    }

    fn flush_pending_output(&mut self) -> Result<bool, String> {
        while !self.pending_output.is_empty() {
            match self.stream.write(self.pending_output.as_slice()) {
                Ok(0) => return Ok(false),
                Ok(n) => {
                    self.pending_output.consume(n);
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
    // Feature builds can spend several seconds loading the Ghostty VT library
    // and starting the daemon process under CI load before the socket is bound.
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if try_connect_session_stream(path).is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
    Err(format!("timed out waiting for socket {}", path.display()))
}

pub(crate) fn ensure_daemon_started(layout: &RuntimeLayout) -> Result<(), String> {
    validate_session_socket_path(&layout.socket_path())?;
    if try_connect_session_stream(&layout.socket_path()).is_ok() && is_session_daemon_alive(layout.root(), layout.daemon_name()) {
        return Ok(());
    }

    if layout.socket_path().exists() && !is_session_daemon_alive(layout.root(), layout.daemon_name()) {
        match fs::remove_file(layout.socket_path()) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(format!("remove stale daemon socket {}: {err}", layout.socket_path().display())),
        }
    }

    layout.ensure_daemon_dirs()?;
    spawn_daemon_process(layout.root(), layout.daemon_name())?;
    wait_for_socket(&layout.socket_path())
}

fn http_error_message(response: http_uds::HttpResponse) -> String {
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&response.body) {
        if let Some(message) = value.get("error").and_then(|value| value.as_str()) {
            return message.to_string();
        }
    }
    format!("HTTP request returned {}", response.status)
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        io::Write,
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
        let layout = RuntimeLayout::new(temp.path().to_path_buf());
        layout.ensure_daemon_dirs().expect("create daemon dirs");
        fs::create_dir_all(layout.session_dir("alpha")).expect("create session dir");
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

        let attach = attach_foreground(&layout, "alpha").expect("attach");
        drop(attach);
        let request = rx.recv_timeout(Duration::from_secs(1)).expect("receive request");

        reader.join().expect("join reader");
        assert!(request.starts_with("POST /sessions/alpha/attach HTTP/1.1\r\n"), "{request}");
        assert!(request.contains("Connection: Upgrade\r\n"), "{request}");
        assert!(request.contains("Upgrade: cleat-attach/1\r\n"), "{request}");
        // Capabilities are detected from the ambient environment, so compare
        // against the serialization of whatever detection currently reports.
        let expected_capabilities =
            serde_json::to_string(&super::attach_capabilities_to_http(attach_init_capabilities())).expect("serialize capabilities");
        assert!(request.ends_with(&format!(r#""capabilities":{expected_capabilities}}}"#)), "{request}");
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
    fn packet_client_backlog_overflow_marks_client_dead_not_daemon_fatal() {
        let (stream, _peer) = std::os::unix::net::UnixStream::pair().expect("unix stream pair");
        let mut client =
            super::PacketClient::new(1, stream, Vec::new(), None, &crate::packet::DirectorySnapshot { sessions: Vec::new() }, None)
                .expect("create packet client");
        client.pending_output = super::PendingOutput::from(vec![0; super::MAX_PENDING_CLIENT_OUTPUT_BYTES - 1]);

        let frame = crate::packet::PacketFrame { channel: 1, msg_type: 0, payload: vec![0; 64] };
        client.enqueue_frame(&frame).expect("overflow must not surface as a daemon-level error");

        assert!(client.dead, "overflowing client should be marked dead for reaping");
        assert!(client.pending_output.is_empty(), "backlog should be released");

        client.enqueue_frame(&frame).expect("enqueue to a dead client is a no-op");
        assert!(client.pending_output.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn dead_packet_client_does_not_hold_the_controller_slot() {
        let (stream, _peer) = std::os::unix::net::UnixStream::pair().expect("unix stream pair");
        let mut client =
            super::PacketClient::new(7, stream, Vec::new(), None, &crate::packet::DirectorySnapshot { sessions: Vec::new() }, None)
                .expect("create packet client");
        let holder = super::PacketChannelRef { client_id: 7, channel: 1 };

        assert!(super::packet_controller_holder_is_live(holder, std::slice::from_ref(&client)));

        client.dead = true;
        assert!(!super::packet_controller_holder_is_live(holder, std::slice::from_ref(&client)));
        assert!(!super::packet_controller_holder_is_live(holder, &[]), "a reaped holder is not live either");
    }

    #[test]
    fn contain_session_unwind_contains_errors_as_faults() {
        let contained = super::contain_session_unwind("alpha", || -> Result<(), String> { Err("actor died".into()) });
        assert!(contained.is_none(), "a servicing error should fault the session, not the daemon");

        let ok = super::contain_session_unwind("alpha", || Ok(7));
        assert_eq!(ok, Some(7));
    }

    #[test]
    fn pending_output_partial_consumes_preserve_byte_order_without_upfront_memmove() {
        let mut pending = super::PendingOutput::new();
        pending.write_all(b"abcdefgh").expect("queue bytes");

        pending.consume(3);
        assert_eq!(pending.as_slice(), b"defgh");
        assert_eq!(pending.len(), 5);
        // Consumed less than remains: the cursor advances, nothing moves yet.
        assert_eq!(pending.start, 3);

        pending.write_all(b"ij").expect("queue more bytes");
        assert_eq!(pending.as_slice(), b"defghij");

        // Dead prefix (7) now exceeds the remainder (3): compaction fires.
        pending.consume(4);
        assert_eq!(pending.as_slice(), b"hij");
        assert_eq!(pending.start, 0);

        pending.consume(3);
        assert!(pending.is_empty());
        assert_eq!(pending.start, 0);
    }

    /// The zero-copy output path must produce byte-identical backlogs to the
    /// generic `Frame` path it replaces (issue #135).
    #[cfg(unix)]
    #[test]
    fn enqueue_output_frames_bytes_identically_to_enqueue_frame() {
        let (stream_a, _peer_a) = std::os::unix::net::UnixStream::pair().expect("unix stream pair");
        let (stream_b, _peer_b) = std::os::unix::net::UnixStream::pair().expect("unix stream pair");
        let capabilities = crate::vt::ClientCapabilities::conservative_fallback();
        let mut via_output = super::ActiveClient::new(stream_a, capabilities).expect("create active client");
        let mut via_frame = super::ActiveClient::new(stream_b, capabilities).expect("create active client");

        let payload = b"\x1b[1mchunk\x00\xff";
        via_output.enqueue_output(payload).expect("enqueue output payload");
        via_frame.enqueue_frame(&super::Frame::Output(payload.to_vec())).expect("enqueue output frame");

        assert_eq!(via_output.pending_output.as_slice(), via_frame.pending_output.as_slice());
    }

    #[cfg(unix)]
    #[test]
    fn active_client_rejects_unbounded_output_backlog() {
        let (stream, _peer) = std::os::unix::net::UnixStream::pair().expect("unix stream pair");
        let mut client =
            super::ActiveClient::new(stream, crate::vt::ClientCapabilities::conservative_fallback()).expect("create active client");
        client.pending_output = super::PendingOutput::from(vec![0; super::MAX_PENDING_CLIENT_OUTPUT_BYTES - 1]);

        let err = client.enqueue_frame(&super::Frame::Output(vec![1])).expect_err("backlog should overflow");
        assert!(err.contains("client output backlog exceeded"));

        let err = client.enqueue_output(&[1]).expect_err("zero-copy path enforces the same backlog cap");
        assert!(err.contains("client output backlog exceeded"));
    }

    #[test]
    fn default_vt_engine_starts_with_default_size() {
        let session = SessionMetadata {
            id: "test".to_string(),
            vt_engine: vt::default_vt_engine_kind(),
            cwd: None,
            cmd: None,
            tags: Vec::new(),
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
            tags: Vec::new(),
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
    fn lifecycle_attach_init_capabilities_detect_from_environment() {
        // Detection reads the ambient environment, so assert the wiring rather
        // than a specific level; the pure detection logic is unit-tested in
        // `vt::tests`.
        assert_eq!(attach_init_capabilities(), vt::ClientCapabilities::detect());
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
    fn screen_stable_snapshot_gating_tracks_render_generation() {
        // No fingerprint taken yet: always snapshot.
        assert!(super::screen_stable_needs_snapshot(0, None));
        // Nothing rendered since the last fingerprint: the screen cannot have
        // changed, skip the snapshot and let the stability window age.
        assert!(!super::screen_stable_needs_snapshot(7, Some(7)));
        // A pump advanced the generation: re-fingerprint.
        assert!(super::screen_stable_needs_snapshot(8, Some(7)));
    }

    #[test]
    fn lifecycle_apply_attach_state_uses_attach_capabilities_for_replay() {
        let mut engine = TestReplayProbeVtEngine::new(80, 24);
        let capabilities = vt::ClientCapabilities::new(vt::ColorLevel::Ansi256, true);

        let replay = apply_attach_state(&mut engine, 100, 30, &capabilities).expect("apply attach state");

        assert_eq!(engine.size(), (100, 30));
        assert_eq!(replay, Some(b"Ansi256:true".to_vec()));
    }

    #[test]
    fn client_install_plan_sends_snapshot_covered_chunks_only_to_existing_clients() {
        let (tap_tx, tap) = crate::host::actor::RawOutputTap::test_channel(2);
        tap_tx
            .send(crate::host::actor::RawOutputChunk { sequence: 1, bytes: b"line2\n".to_vec().into() })
            .expect("queue snapshot-covered output");
        tap_tx
            .send(crate::host::actor::RawOutputChunk { sequence: 2, bytes: b"line3\n".to_vec().into() })
            .expect("queue post-snapshot output");

        let plan = super::plan_raw_output_client_install(
            &tap,
            crate::host::actor::RawOutputReplay { payload: Some(b"line2\n".to_vec()), through_sequence: 1 },
            super::ReplayMode::FreshTerminal,
        );

        let super::RawOutputClientInstallPlan::Complete { existing_chunks, new_client_frames } = plan else {
            panic!("connected tap should produce a complete install plan");
        };
        assert_eq!(existing_chunks, vec![Arc::from(&b"line2\n"[..]), Arc::from(&b"line3\n"[..])]);
        assert_eq!(new_client_frames, vec![
            crate::protocol::Frame::Output(b"line2\n".to_vec()),
            crate::protocol::Frame::Output(b"line3\n".to_vec())
        ]);
    }

    struct FakeRawOutputRecoverySource {
        seen_capabilities: RefCell<Vec<vt::ClientCapabilities>>,
        payloads: Vec<Option<Vec<u8>>>,
    }

    impl super::RawOutputRecoverySource for FakeRawOutputRecoverySource {
        fn recover_raw_output(&self, capabilities: Vec<vt::ClientCapabilities>) -> Result<crate::host::actor::RawOutputRecovery, String> {
            self.seen_capabilities.replace(capabilities);
            let (_tap_tx, tap) = crate::host::actor::RawOutputTap::test_channel(1);
            Ok(crate::host::actor::RawOutputRecovery { tap, payloads: self.payloads.clone() })
        }
    }

    #[test]
    fn disconnected_tap_recovers_with_terminal_reset_and_fresh_replay() {
        let (tap_tx, tap) = crate::host::actor::RawOutputTap::test_channel(1);
        tap_tx
            .send(crate::host::actor::RawOutputChunk { sequence: 1, bytes: b"queued\n".to_vec().into() })
            .expect("queue output before overflow");
        drop(tap_tx);
        let install = super::plan_raw_output_client_install(
            &tap,
            crate::host::actor::RawOutputReplay { payload: Some(b"stale snapshot".to_vec()), through_sequence: 0 },
            super::ReplayMode::FreshTerminal,
        );
        let super::RawOutputClientInstallPlan::Disconnected { existing_chunks } = install else {
            panic!("dropped tap sender should require recovery");
        };
        assert_eq!(existing_chunks, vec![Arc::from(&b"queued\n"[..])]);

        let capabilities = vt::ClientCapabilities::new(vt::ColorLevel::Ansi256, true);
        let source = FakeRawOutputRecoverySource {
            seen_capabilities: RefCell::new(Vec::new()),
            payloads: vec![Some(b"fresh snapshot with OVERFLOW_END".to_vec())],
        };
        let recovery = super::plan_raw_output_recovery(&source, &[super::RawOutputRecoveryRequest {
            capabilities,
            replay_mode: super::ReplayMode::ResetTerminal,
        }])
        .expect("plan recovery");

        assert_eq!(*source.seen_capabilities.borrow(), vec![capabilities]);
        assert_eq!(recovery.recipient_frames, vec![vec![
            crate::protocol::Frame::Output(super::REATTACH_CLEAR_SEQUENCE.to_vec()),
            crate::protocol::Frame::Output(b"fresh snapshot with OVERFLOW_END".to_vec())
        ]]);
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
