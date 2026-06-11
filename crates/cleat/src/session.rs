#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

use std::{
    collections::VecDeque,
    fs,
    io::{Read, Write},
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
    http_uds,
    platform::{
        daemon::{is_session_daemon_alive, spawn_daemon_process},
        ipc::{
            bind_session_listener, connect_session_stream, session_socket_path as platform_session_socket_path, set_listener_nonblocking,
            set_stream_nonblocking, set_stream_read_timeout, shutdown_stream, SessionStream,
        },
        pty::poll_session_ready,
        terminal::{attach_signal_exit_requested, current_terminal_size, stdout_is_tty, AttachSignalHandlers, ForegroundTerminal},
    },
    protocol::Frame,
    runtime::{RuntimeLayout, SessionMetadata},
    session_runtime::SessionRuntime,
    vt::{self, ScreenGrid, VtEngine, VtEngineKind},
};

const FOREGROUND_NAME: &str = "foreground";
const DEFAULT_TERMINAL_COLS: u16 = 80;
const DEFAULT_TERMINAL_ROWS: u16 = 24;
const DETACH_CLEANUP_SEQUENCE: &[u8] =
    b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1004l\x1b[<u\x1b[r\x1b[0m\x1b[?25h\x1b[2J\x1b[H\x1b[?1049l";
const REATTACH_CLEAR_SEQUENCE: &[u8] = b"\x1b[2J\x1b[H";
const MAX_PENDING_CLIENT_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const TERMINATE_SIGNAL: i32 = 15;

#[derive(Debug)]
pub struct ForegroundAttach {
    stream: Arc<Mutex<SessionStream>>,
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
    record: bool,
    colors: vt::TerminalColors,
) -> Result<SessionMetadata, String> {
    // If a named session directory already exists with a live socket, reuse it.
    if let Some(ref id_str) = id {
        let socket_path = session_socket_path(layout.root(), id_str);
        if socket_path.exists() {
            if is_session_daemon_alive(layout.root(), id_str) {
                // Daemon is running — return the id. The caller should use inspect()
                // if it needs the session's actual config.
                let vt_engine = vt_engine.unwrap_or_else(vt::default_vt_engine_kind);
                return Ok(SessionMetadata { id: id_str.clone(), vt_engine, cwd, cmd, record, colors });
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
    session.record = record;
    session.colors = colors;

    let socket_path = session_socket_path(layout.root(), &session.id);
    spawn_daemon_process(layout.root(), &session)?;
    wait_for_socket(&socket_path)?;

    Ok(session)
}

pub fn attach_foreground(layout: &RuntimeLayout, id: &str) -> Result<ForegroundAttach, String> {
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
        http_uds::write_attach_upgrade_request(&mut stream, &format!("/sessions/{id}/attach"), &body)
            .map_err(|err| format!("write attach upgrade request: {err}"))?;
        let response = http_uds::read_response_head(&mut stream).map_err(|err| format!("read attach upgrade response: {err}"))?;
        match response.status {
            StatusCode::SWITCHING_PROTOCOLS => return Ok(ForegroundAttach { stream: Arc::new(Mutex::new(stream)) }),
            StatusCode::CONFLICT => {}
            other => return Err(format!("unexpected attach response: {other}")),
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
        return Ok(Box::new(TestReplayProbeVtEngine::new(DEFAULT_TERMINAL_COLS, DEFAULT_TERMINAL_ROWS)));
    }

    if std::env::var_os("CARGO_BIN_EXE_cleat").is_some()
        && std::env::var_os("CLEAT_TEST_VT_ENGINE").as_deref() == Some(std::ffi::OsStr::new("replay-probe"))
    {
        return Ok(Box::new(TestReplayProbeVtEngine::new(DEFAULT_TERMINAL_COLS, DEFAULT_TERMINAL_ROWS)));
    }
    vt::make_vt_engine_with_colors(session.vt_engine, DEFAULT_TERMINAL_COLS, DEFAULT_TERMINAL_ROWS, session.colors)
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
    timeout_ms: u64,
    registered_at: Instant,
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

    let mut runtime = SessionRuntime::spawn(session_dir.clone(), session, default_vt_engine(session)?)?;
    let mut active_client: Option<ActiveClient> = None;
    let mut had_foreground_client = false;
    let mut pending_waits: Vec<PendingWait> = Vec::new();
    let mut pending_expects: Vec<PendingExpect> = Vec::new();
    loop {
        let poll_result = poll_session_ready(
            &listener,
            active_client.as_ref().map(|client| &client.stream),
            active_client.as_ref().map(|client| !client.pending_output.is_empty()).unwrap_or(false),
            runtime.pty_child(),
            100,
        )?;

        if poll_result.listener_readable {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    // Accepted sockets inherit nonblocking mode from the listener on macOS/BSD.
                    // Reset to blocking so the initial frame read works correctly.
                    #[cfg(unix)]
                    {
                        set_stream_nonblocking(&stream, false).map_err(|err| format!("set accepted stream blocking: {err}"))?;
                        let _ = set_stream_read_timeout(&stream, Some(Duration::from_millis(100)));
                    }
                    #[cfg(windows)]
                    {
                        set_stream_nonblocking(&stream, true).map_err(|err| format!("set accepted stream nonblocking: {err}"))?;
                    }
                    let mut prefix = [0; 5];
                    if let Err(err) = stream.read_exact(&mut prefix) {
                        let _ = Frame::Error(format!("failed to read request: {err}")).write(&mut stream);
                        continue;
                    }
                    if !http_uds::looks_like_http_prefix(&prefix) {
                        let _ = Frame::Error("session daemon requires HTTP requests".to_string()).write(&mut stream);
                        continue;
                    }

                    let mut http_state = HttpRequestState {
                        runtime: &mut runtime,
                        active_client: &mut active_client,
                        had_foreground_client: &mut had_foreground_client,
                        pending_waits: &mut pending_waits,
                        pending_expects: &mut pending_expects,
                    };
                    if let Err(err) = handle_http_request(root, id, &mut stream, &prefix, &mut http_state) {
                        let _ = http_uds::write_error(&mut stream, StatusCode::INTERNAL_SERVER_ERROR, &err);
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(err) => return Err(format!("accept client: {err}")),
            }
        }

        if active_client.is_some() {
            let mut client_disconnected = false;
            let mut pending = VecDeque::new();
            let client_read_timeout =
                if poll_result.pty_readable || poll_result.client_writable { Duration::ZERO } else { Duration::from_millis(100) };
            if let Some(client) = active_client.as_mut() {
                match client.drain_input_frames(&mut pending, client_read_timeout) {
                    Ok(true) => {}
                    Ok(false) => client_disconnected = true,
                    Err(err) => return Err(format!("read client frame: {err}")),
                }
            }

            while let Some(frame) = pending.pop_front() {
                match frame {
                    Frame::Input(bytes) => {
                        runtime.write_input(&bytes)?;
                    }
                    Frame::Resize { cols, rows } => {
                        runtime.resize(cols, rows)?;
                    }
                    _ => {}
                }
            }

            if client_disconnected && active_client.is_some() {
                let _ = fs::remove_file(foreground_path(root, id));
                runtime.record_detach();
                active_client = None;
            }
        }

        if poll_result.client_writable {
            let client_writable = match active_client.as_mut() {
                Some(client) => client.flush_pending_output()?,
                None => true,
            };
            if !client_writable {
                let _ = fs::remove_file(foreground_path(root, id));
                runtime.record_detach();
                active_client = None;
            }
        }

        if poll_result.pty_readable {
            let output = runtime.read_available_output(active_client.is_some())?;
            for chunk in output.chunks {
                if let Some(client) = active_client.as_mut() {
                    if client.enqueue_frame(&Frame::Output(chunk)).is_err() {
                        let _ = fs::remove_file(foreground_path(root, id));
                        runtime.record_detach();
                        active_client = None;
                        break;
                    }
                }
            }
        }

        runtime.flush_recording_if_idle(poll_result.pty_readable, poll_result.client_readable);

        // Evaluate pending waits on each loop tick.
        // Write errors are intentionally discarded — the client may have disconnected.
        pending_waits.retain_mut(|wait| {
            let elapsed = wait.registered_at.elapsed();
            let elapsed_ms = elapsed.as_millis() as u64;

            // Check timeout first
            if elapsed_ms >= wait.timeout_ms {
                let _ = write_http_wait_result(&mut wait.stream, crate::protocol::WaitStatus::Timeout, elapsed_ms);
                return false;
            }

            // Check conditions (OR semantics — any match wins)
            for condition in &wait.conditions {
                match condition {
                    crate::protocol::WaitCondition::OutputIdle { quiet_ms } => {
                        // Silence measured from max(registration_time, last_output_time)
                        let silence_since = match runtime.last_pty_output_at() {
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
                        if runtime.screen_contains(text.as_str()) {
                            let _ = write_http_wait_result(&mut wait.stream, crate::protocol::WaitStatus::Ready, elapsed_ms);
                            return false;
                        }
                    }
                }
            }

            true // keep waiting
        });

        // Evaluate pending expects by scanning the cast file for text matches.
        if !pending_expects.is_empty() {
            runtime.flush_recording();
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

        if let Some(exit_code) = runtime.exit_code_if_exited()? {
            let output = runtime.drain_output_after_exit(active_client.is_some())?;
            for chunk in output.chunks {
                if let Some(client) = active_client.as_mut() {
                    if client.enqueue_frame(&Frame::Output(chunk)).is_err() {
                        let _ = fs::remove_file(foreground_path(root, id));
                        runtime.record_detach();
                        active_client = None;
                        break;
                    }
                }
            }
            for mut wait in pending_waits.drain(..) {
                let elapsed_ms = wait.registered_at.elapsed().as_millis() as u64;
                let _ = write_http_wait_result(&mut wait.stream, crate::protocol::WaitStatus::SessionGone, elapsed_ms);
            }
            for mut expect in pending_expects.drain(..) {
                let elapsed_ms = expect.registered_at.elapsed().as_millis() as u64;
                let _ = write_http_wait_result(&mut expect.stream, crate::protocol::WaitStatus::SessionGone, elapsed_ms);
            }
            runtime.record_exit_code(exit_code);
            if let Some(client) = active_client.as_mut() {
                let _ = client.flush_pending_output();
            }
            break;
        }
    }

    let session_dir = root.join(id);
    let _ = fs::remove_file(&socket_path);
    let _ = fs::remove_file(daemon_pid_path(root, id));
    let _ = fs::remove_file(foreground_path(root, id));
    if !runtime.should_keep_session_dir() {
        let _ = fs::remove_dir_all(&session_dir);
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
pub fn run_session_daemon(_root: &Path, _session: &SessionMetadata) -> Result<(), String> {
    Err("session daemon is only supported on unix".into())
}

struct HttpRequestState<'a> {
    runtime: &'a mut SessionRuntime,
    active_client: &'a mut Option<ActiveClient>,
    had_foreground_client: &'a mut bool,
    pending_waits: &'a mut Vec<PendingWait>,
    pending_expects: &'a mut Vec<PendingExpect>,
}

fn handle_http_request(
    root: &Path,
    daemon_id: &str,
    stream: &mut SessionStream,
    prefix: &[u8],
    state: &mut HttpRequestState<'_>,
) -> Result<(), String> {
    let request = http_uds::read_request_with_prefix(stream, prefix).map_err(|err| format!("read HTTP request: {err}"))?;
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
            let result = state.runtime.inspect(state.active_client.is_some());
            http_uds::write_json(stream, StatusCode::OK, &http_uds::SessionListResponse { sessions: vec![result] })
                .map_err(|err| format!("write HTTP sessions response: {err}"))
        }
        http_uds::Route::SessionInspect { id } if id == daemon_id => {
            let result = state.runtime.inspect(state.active_client.is_some());
            http_uds::write_json(stream, StatusCode::OK, &result).map_err(|err| format!("write HTTP inspect response: {err}"))
        }
        http_uds::Route::SessionDelete { id } if id == daemon_id => {
            state.runtime.dispatch_signal(TERMINATE_SIGNAL, crate::protocol::SignalTarget::Leader)?;
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
            let replay = state.runtime.apply_attach_state(body.cols, body.rows, &capabilities)?;
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
            state.runtime.record_attach();
            Ok(())
        }
        http_uds::Route::SessionDetach { id } if id == daemon_id => {
            let _ = fs::remove_file(foreground_path(root, daemon_id));
            if state.active_client.is_some() {
                state.runtime.record_detach();
            }
            *state.active_client = None;
            http_uds::write_no_content(stream).map_err(|err| format!("write HTTP detach response: {err}"))
        }
        http_uds::Route::SessionExpect { id } if id == daemon_id => 'expect: {
            let body: http_uds::ExpectRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP expect request: {err}"))?;
            if !state.runtime.recording_active() {
                http_uds::write_error(stream, StatusCode::CONFLICT, "recording not active")
                    .map_err(|err| format!("write HTTP expect error: {err}"))?;
                break 'expect Ok(());
            }

            let cast_path = root.join(daemon_id).join(crate::recording::CAST_FILE_NAME);
            state.runtime.flush_recording();
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
                    state.runtime.write_input(text.as_bytes())?;
                }
                http_uds::InputRequest::Paste { text } => {
                    let bytes = state.runtime.encode_paste(text.as_bytes())?;
                    state.runtime.write_input(&bytes)?;
                }
                http_uds::InputRequest::Key { key } => {
                    let bytes = http_input_key_bytes(key);
                    state.runtime.write_input(&bytes)?;
                }
                http_uds::InputRequest::RawBytes { bytes } => {
                    state.runtime.write_input(&bytes)?;
                }
                http_uds::InputRequest::Resize { cols, rows } => {
                    state.runtime.resize(cols, rows)?;
                }
            }
            http_uds::write_no_content(stream).map_err(|err| format!("write HTTP input response: {err}"))
        }
        http_uds::Route::SessionKeys { id } if id == daemon_id => {
            let body: http_uds::KeysRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP keys request: {err}"))?;
            state.runtime.write_input(&body.bytes)?;
            http_uds::write_no_content(stream).map_err(|err| format!("write HTTP keys response: {err}"))
        }
        http_uds::Route::SessionKeysWithMark { id } if id == daemon_id => {
            let body: http_uds::KeysWithMarkRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP keys-with-mark request: {err}"))?;
            let offset = state.runtime.write_input_with_mark(&body.bytes, body.marker_name)?;
            http_uds::write_json(stream, StatusCode::OK, &http_uds::MarkResponse { offset })
                .map_err(|err| format!("write HTTP keys-with-mark response: {err}"))
        }
        http_uds::Route::SessionRecord { id } if id == daemon_id => {
            let body: http_uds::RecordRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP record request: {err}"))?;
            state.runtime.set_recording(body.enable)?;
            http_uds::write_no_content(stream).map_err(|err| format!("write HTTP record response: {err}"))
        }
        http_uds::Route::SessionMark { id } if id == daemon_id => {
            let body: http_uds::MarkRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP mark request: {err}"))?;
            let offset = state.runtime.mark(body.name)?;
            http_uds::write_json(stream, StatusCode::OK, &http_uds::MarkResponse { offset })
                .map_err(|err| format!("write HTTP mark response: {err}"))
        }
        http_uds::Route::SessionResolveMarker { id } if id == daemon_id => {
            let body: http_uds::ResolveMarkerRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP resolve-marker request: {err}"))?;
            match state.runtime.resolve_marker(&body.name) {
                Some(offset) => http_uds::write_json(stream, StatusCode::OK, &http_uds::MarkResponse { offset })
                    .map_err(|err| format!("write HTTP resolve-marker response: {err}")),
                None => http_uds::write_error(stream, StatusCode::NOT_FOUND, &format!("marker not found: {}", body.name))
                    .map_err(|err| format!("write HTTP resolve-marker error: {err}")),
            }
        }
        http_uds::Route::SessionResolveNextMarker { id } if id == daemon_id => {
            let body: http_uds::ResolveNextMarkerRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP resolve-next-marker request: {err}"))?;
            let offset = state.runtime.resolve_next_marker_after(body.after);
            http_uds::write_json(stream, StatusCode::OK, &http_uds::ResolveNextMarkerResponse { offset })
                .map_err(|err| format!("write HTTP resolve-next-marker response: {err}"))
        }
        http_uds::Route::SessionResize { id } if id == daemon_id => {
            let body: http_uds::ResizeRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP resize request: {err}"))?;
            state.runtime.resize(body.cols, body.rows)?;
            http_uds::write_no_content(stream).map_err(|err| format!("write HTTP resize response: {err}"))
        }
        http_uds::Route::SessionScreen { id } if id == daemon_id => match state.runtime.capture_text() {
            Ok(text) => http_uds::write_json(stream, StatusCode::OK, &http_uds::ScreenResponse { text })
                .map_err(|err| format!("write HTTP screen response: {err}")),
            Err(err) => http_uds::write_error(stream, StatusCode::CONFLICT, &err).map_err(|err| format!("write HTTP screen error: {err}")),
        },
        http_uds::Route::SessionSignal { id } if id == daemon_id => {
            let body: http_uds::SignalRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP signal request: {err}"))?;
            state.runtime.dispatch_signal(body.signal, signal_target_from_http(body.target))?;
            http_uds::write_no_content(stream).map_err(|err| format!("write HTTP signal response: {err}"))
        }
        http_uds::Route::SessionSnapshot { id } if id == daemon_id => {
            let output = state.runtime.read_available_output(state.active_client.is_some())?;
            if let Some(client) = state.active_client.as_mut() {
                for chunk in output.chunks {
                    if client.enqueue_frame(&Frame::Output(chunk)).is_err() {
                        let _ = fs::remove_file(foreground_path(root, daemon_id));
                        state.runtime.record_detach();
                        *state.active_client = None;
                        break;
                    }
                }
            }
            match state.runtime.snapshot(crate::provider::DirtyState::Full) {
                Ok(snapshot) => http_uds::write_json(stream, StatusCode::OK, &http_uds::snapshot_response(snapshot))
                    .map_err(|err| format!("write HTTP snapshot response: {err}")),
                Err(err) => {
                    http_uds::write_error(stream, StatusCode::CONFLICT, &err).map_err(|err| format!("write HTTP snapshot error: {err}"))
                }
            }
        }
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
            if has_text_match {
                if let Err(err) = state.runtime.validate_text_matching() {
                    http_uds::write_error(stream, StatusCode::CONFLICT, &format!("text matching not supported: {err}"))
                        .map_err(|err| format!("write HTTP wait error: {err}"))?;
                    break 'wait Ok(());
                }
            }

            if has_text_match {
                for condition in &conditions {
                    if let crate::protocol::WaitCondition::TextMatch { text } = condition {
                        if state.runtime.screen_contains(text.as_str()) {
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
                timeout_ms: body.timeout_ms,
                registered_at: Instant::now(),
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
    use std::sync::{Arc, Mutex};

    use super::{
        apply_attach_state, attach_foreground, attach_init_capabilities, default_vt_engine, record_pty_output, session_socket_path,
        AttachCleanupGuard, TestReplayProbeVtEngine,
    };
    use crate::{
        http_uds::read_http_request_for_test,
        runtime::{RuntimeLayout, SessionMetadata},
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
            colors: vt::TerminalColors::default(),
        };
        let engine = default_vt_engine(&session).expect("create default vt engine");
        assert_eq!(engine.size(), (super::DEFAULT_TERMINAL_COLS, super::DEFAULT_TERMINAL_ROWS));
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
            colors: vt::TerminalColors::default(),
        };
        let mut engine = default_vt_engine(&session).expect("create default vt engine");
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
