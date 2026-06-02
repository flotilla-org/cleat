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

use crate::{
    da::DeviceAttributeTracker,
    platform::{
        daemon::spawn_daemon_process,
        ipc::{
            bind_session_listener, connect_session_stream, session_socket_path as platform_session_socket_path, set_listener_nonblocking,
            set_stream_nonblocking, set_stream_read_timeout, shutdown_stream, SessionStream,
        },
        pty::{exit_code_from_wait_status, poll_session_ready, PtyChild},
        terminal::{
            attach_signal_exit_requested, current_terminal_size, poll_stdin_readable, stdout_is_tty, AttachSignalHandlers,
            TerminalModeGuard,
        },
    },
    protocol::Frame,
    runtime::{RuntimeLayout, SessionMetadata},
    vt::{self, ScreenGrid, VtEngine, VtEngineKind},
};

const FOREGROUND_NAME: &str = "foreground";
const DEFAULT_TERMINAL_COLS: u16 = 80;
const DEFAULT_TERMINAL_ROWS: u16 = 24;
const DETACH_CLEANUP_SEQUENCE: &[u8] = b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1004l\x1b[?1049l\x1b[<u\x1b[?25h";
const REATTACH_CLEAR_SEQUENCE: &[u8] = b"\x1b[2J\x1b[H";
const PTY_READ_BUFFER_SIZE: usize = 64 * 1024;
const MAX_PENDING_CLIENT_OUTPUT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug)]
pub struct ForegroundAttach {
    stream: Arc<Mutex<SessionStream>>,
}

impl ForegroundAttach {
    pub fn relay_stdio(self) -> Result<(), String> {
        let mut cleanup = AttachCleanupGuard::stdout();
        let _tty_mode = TerminalModeGuard::activate()?;
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

        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 4096];
        let stdin_result = loop {
            if !alive.load(Ordering::SeqCst) || attach_signal_exit_requested() {
                break Ok(());
            }
            match poll_stdin_readable(Duration::from_millis(100)) {
                Ok(false) => continue,
                Ok(true) => {}
                Err(_err) if attach_signal_exit_requested() => break Ok(()),
                Err(err) => break Err(err),
            }
            match stdin.read(&mut buf) {
                Ok(0) => break Ok(()),
                Ok(n) => {
                    let mut stream = self.stream.lock().map_err(|_| "attach stream lock poisoned".to_string())?;
                    if let Err(err) = Frame::Input(buf[..n].to_vec()).write(&mut *stream) {
                        if is_graceful_socket_shutdown(&err) {
                            break Ok(());
                        }
                        break Err(format!("write input frame: {err}"));
                    }
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
) -> Result<SessionMetadata, String> {
    // If a named session directory already exists with a live socket, reuse it.
    if let Some(ref id_str) = id {
        let socket_path = session_socket_path(layout.root(), id_str);
        if socket_path.exists() {
            // Daemon is running — return the id. The caller should use inspect()
            // if it needs the session's actual config.
            let vt_engine = vt_engine.unwrap_or_else(vt::default_vt_engine_kind);
            return Ok(SessionMetadata { id: id_str.clone(), vt_engine, cwd, cmd, record });
        }
    }

    // Create a new session and spawn the daemon.
    let vt_engine = vt_engine.unwrap_or_else(vt::default_vt_engine_kind);
    vt_engine.ensure_available()?;
    let mut session = layout.create_session(id, vt_engine, cwd, cmd)?;
    session.record = record;

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
        Frame::AttachInit { cols, rows, capabilities: attach_init_capabilities() }
            .write(&mut stream)
            .map_err(|err| format!("write attach init: {err}"))?;
        match Frame::read(&mut stream).map_err(|err| format!("read attach response: {err}"))? {
            Frame::Ack => return Ok(ForegroundAttach { stream: Arc::new(Mutex::new(stream)) }),
            Frame::Busy => {}
            other => return Err(format!("unexpected attach response: {other:?}")),
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

fn default_vt_engine(kind: VtEngineKind) -> Result<Box<dyn VtEngine>, String> {
    #[cfg(test)]
    if kind == VtEngineKind::Ghostty {
        return Ok(Box::new(TestReplayProbeVtEngine::new(DEFAULT_TERMINAL_COLS, DEFAULT_TERMINAL_ROWS)));
    }

    if std::env::var_os("CARGO_BIN_EXE_cleat").is_some()
        && std::env::var_os("CLEAT_TEST_VT_ENGINE").as_deref() == Some(std::ffi::OsStr::new("replay-probe"))
    {
        return Ok(Box::new(TestReplayProbeVtEngine::new(DEFAULT_TERMINAL_COLS, DEFAULT_TERMINAL_ROWS)));
    }
    vt::make_vt_engine(kind, DEFAULT_TERMINAL_COLS, DEFAULT_TERMINAL_ROWS)
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

fn record_pty_output(engine: &mut dyn VtEngine, bytes: &[u8]) -> Result<(), String> {
    engine.feed(bytes)
}

fn attach_init_capabilities() -> vt::ClientCapabilities {
    vt::ClientCapabilities::conservative_fallback()
}

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

    let pty_child = PtyChild::spawn(session)?;
    pty_child.set_nonblocking()?;
    let mut vt_engine = default_vt_engine(session.vt_engine)?;
    // The DA tracker is the only DA source for the passthrough engine.
    // The ghostty engine answers DA itself via its DeviceAttributes callback,
    // so we skip the tracker there to avoid double replies.
    let mut detached_da = match session.vt_engine {
        vt::VtEngineKind::Passthrough => Some(DeviceAttributeTracker::new()),
        vt::VtEngineKind::Ghostty => None,
    };

    let mut active_client: Option<ActiveClient> = None;
    let mut recorder: Option<crate::recording::SessionRecorder> = None;
    let epoch = Instant::now();
    let mut markers: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    if session.record {
        let (cols, rows) = vt_engine.size();
        match crate::recording::SessionRecorder::new(&root.join(id), cols, rows, session.vt_engine.as_str()) {
            Ok(mut r) => {
                // Bootstrap: emit initial snapshot if VT engine has state
                if let Ok(Some(payload)) = vt_engine.replay_payload(&vt::ClientCapabilities::conservative_fallback()) {
                    let state = String::from_utf8_lossy(&payload);
                    r.write_snapshot(&state, session.vt_engine.as_str(), cols, rows, std::time::Duration::ZERO);
                }
                recorder = Some(r);
            }
            Err(err) => eprintln!("failed to start recording: {err}"),
        }
    }
    let mut had_foreground_client = false;
    let mut pending_waits: Vec<PendingWait> = Vec::new();
    let mut pending_expects: Vec<PendingExpect> = Vec::new();
    let mut last_pty_output_at: Option<Instant> = None;
    loop {
        let poll_result = poll_session_ready(
            &listener,
            active_client.as_ref().map(|client| &client.stream),
            active_client.as_ref().map(|client| !client.pending_output.is_empty()).unwrap_or(false),
            &pty_child,
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
                    match Frame::read(&mut stream) {
                        Ok(Frame::AttachInit { cols, rows, capabilities }) => {
                            if active_client.is_none() {
                                pty_child.resize(cols, rows)?;
                                let replay = apply_attach_state(vt_engine.as_mut(), cols, rows, &capabilities)?;
                                Frame::Ack.write(&mut stream).map_err(|err| format!("write attach ack: {err}"))?;
                                set_stream_nonblocking(&stream, true).map_err(|err| format!("set client nonblocking: {err}"))?;
                                let mut client = ActiveClient::new(stream);
                                if let Some(payload) = replay {
                                    if !payload.is_empty() {
                                        if had_foreground_client {
                                            client.enqueue_frame(&Frame::Output(REATTACH_CLEAR_SEQUENCE.to_vec()))?;
                                        }
                                        client.enqueue_frame(&Frame::Output(payload))?;
                                    }
                                }
                                let _ = fs::write(foreground_path(root, id), b"1");
                                active_client = Some(client);
                                had_foreground_client = true;
                                if let Some(ref mut rec) = recorder {
                                    rec.event(crate::asciicast::EventCode::Custom('a'), r#"{"client":"foreground"}"#, epoch.elapsed());
                                }
                            } else {
                                let _ = Frame::Busy.write(&mut stream);
                            }
                        }
                        Ok(Frame::Detach) => {
                            let _ = fs::remove_file(foreground_path(root, id));
                            if let Some(ref mut rec) = recorder {
                                rec.event(crate::asciicast::EventCode::Custom('d'), r#"{"client":"foreground"}"#, epoch.elapsed());
                            }
                            active_client = None;
                        }
                        Ok(Frame::Capture) => match vt_engine.screen_text() {
                            Ok(text) => {
                                let _ = Frame::Output(text.into_bytes()).write(&mut stream);
                            }
                            Err(err) => {
                                let _ = Frame::Error(err).write(&mut stream);
                            }
                        },
                        Ok(Frame::SendKeys(bytes)) => {
                            if let Some(ref mut rec) = recorder {
                                rec.input(&bytes, epoch.elapsed());
                            }
                            if let Err(err) = pty_child.write_all(&bytes) {
                                let _ = Frame::Error(err).write(&mut stream);
                            }
                        }
                        Ok(Frame::SendKeysWithMark { bytes, marker_name }) => {
                            if let Some(ref mut rec) = recorder {
                                rec.flush();
                                rec.event(crate::asciicast::EventCode::Marker, &marker_name, epoch.elapsed());
                                let offset = rec.bytes_written();
                                markers.insert(marker_name, offset);
                                rec.input(&bytes, epoch.elapsed());
                                if let Err(err) = pty_child.write_all(&bytes) {
                                    let _ = Frame::Error(err).write(&mut stream);
                                } else {
                                    let _ = Frame::MarkResult { offset }.write(&mut stream);
                                }
                            } else {
                                let _ = Frame::Error("recording not active".to_string()).write(&mut stream);
                            }
                        }
                        Ok(Frame::Inspect) => {
                            let result = build_inspect_result(session, vt_engine.as_ref(), &active_client, &pty_child, &recorder, &markers);
                            match serde_json::to_vec(&result) {
                                Ok(json) => {
                                    let _ = Frame::InspectResult(json).write(&mut stream);
                                }
                                Err(err) => {
                                    let _ = Frame::Error(format!("serialize inspect: {err}")).write(&mut stream);
                                }
                            }
                        }
                        Ok(Frame::Signal { signal, target }) => match pty_child.dispatch_signal(signal, target) {
                            Ok(()) => {
                                if let Some(ref mut rec) = recorder {
                                    let target_str = match target {
                                        crate::protocol::SignalTarget::Foreground => "foreground",
                                        crate::protocol::SignalTarget::Leader => "leader",
                                        crate::protocol::SignalTarget::Tree => "tree",
                                    };
                                    rec.event(
                                        crate::asciicast::EventCode::Custom('s'),
                                        &serde_json::json!({"signal": signal, "target": target_str}).to_string(),
                                        epoch.elapsed(),
                                    );
                                }
                                let _ = Frame::Ack.write(&mut stream);
                            }
                            Err(err) => {
                                let _ = Frame::Error(err).write(&mut stream);
                            }
                        },
                        Ok(Frame::Mark { name }) => {
                            if let Some(ref mut rec) = recorder {
                                rec.flush();
                                if let Some(ref marker_name) = name {
                                    // Emit standard asciicast "m" event
                                    rec.event(crate::asciicast::EventCode::Marker, marker_name, epoch.elapsed());
                                    // Store the offset *after* the marker event so that
                                    // read_events_since starts at the first event following
                                    // the marker rather than at the marker line itself.
                                    markers.insert(marker_name.clone(), rec.bytes_written());
                                }
                                let _ = Frame::MarkResult { offset: rec.bytes_written() }.write(&mut stream);
                            } else {
                                let _ = Frame::Error("recording not active".to_string()).write(&mut stream);
                            }
                        }
                        Ok(Frame::ResolveMarker { name }) => {
                            if let Some(offset) = markers.get(&name) {
                                let _ = Frame::MarkResult { offset: *offset }.write(&mut stream);
                            } else {
                                let _ = Frame::Error(format!("marker not found: {name}")).write(&mut stream);
                            }
                        }
                        Ok(Frame::ResolveNextMarker { after }) => {
                            // Markers are appended at the current recording offset, so byte-offset
                            // order matches creation order. min(offset > after) picks the
                            // chronologically-next marker. Back-filling markers would break this.
                            let next = markers.iter().filter(|(_, &offset)| offset > after).map(|(_, &offset)| offset).min();
                            let reply = match next {
                                Some(offset) => Frame::MarkResult { offset },
                                None => Frame::MarkNotFound,
                            };
                            let _ = reply.write(&mut stream);
                        }
                        Ok(Frame::RecordControl { enable }) => {
                            if enable && recorder.is_none() {
                                // First-time activation: create new recorder
                                let (cols, rows) = vt_engine.size();
                                match crate::recording::SessionRecorder::new(&root.join(id), cols, rows, session.vt_engine.as_str()) {
                                    Ok(mut r) => {
                                        if let Ok(Some(payload)) =
                                            vt_engine.replay_payload(&vt::ClientCapabilities::conservative_fallback())
                                        {
                                            let state = String::from_utf8_lossy(&payload);
                                            r.write_snapshot(&state, session.vt_engine.as_str(), cols, rows, epoch.elapsed());
                                        }
                                        recorder = Some(r);
                                        let _ = Frame::Ack.write(&mut stream);
                                    }
                                    Err(err) => {
                                        let _ = Frame::Error(err).write(&mut stream);
                                    }
                                }
                            } else if enable {
                                // Resume from pause
                                if let Some(ref mut rec) = recorder {
                                    if rec.is_paused() {
                                        rec.resume(epoch.elapsed());
                                        // Emit a VT snapshot so the resumed portion has screen context
                                        if let Ok(Some(payload)) =
                                            vt_engine.replay_payload(&vt::ClientCapabilities::conservative_fallback())
                                        {
                                            let (cols, rows) = vt_engine.size();
                                            let state = String::from_utf8_lossy(&payload);
                                            rec.write_snapshot(&state, session.vt_engine.as_str(), cols, rows, epoch.elapsed());
                                        }
                                    }
                                }
                                let _ = Frame::Ack.write(&mut stream);
                            } else if !enable && recorder.as_ref().is_some_and(|r| !r.is_paused()) {
                                // Pause recording (keep recorder alive for gap tracking)
                                if let Some(ref mut rec) = recorder {
                                    rec.pause(epoch.elapsed());
                                }
                                let _ = Frame::Ack.write(&mut stream);
                            } else {
                                let _ = Frame::Ack.write(&mut stream);
                            }
                        }
                        Ok(Frame::Wait { conditions, timeout_ms }) => 'wait: {
                            if conditions.is_empty() {
                                let _ = Frame::Error("at least one wait condition is required".to_string()).write(&mut stream);
                                break 'wait;
                            }

                            // Validate: TextMatch requires screen_text support
                            let has_text_match = conditions.iter().any(|c| matches!(c, crate::protocol::WaitCondition::TextMatch { .. }));
                            if has_text_match {
                                if let Err(err) = vt_engine.screen_text() {
                                    let _ = Frame::Error(format!("text matching not supported: {err}")).write(&mut stream);
                                    break 'wait;
                                }
                            }

                            // Check --text immediately at registration
                            if has_text_match {
                                if let Ok(screen) = vt_engine.screen_text() {
                                    for condition in &conditions {
                                        if let crate::protocol::WaitCondition::TextMatch { text } = condition {
                                            if screen.contains(text.as_str()) {
                                                let _ = Frame::WaitResult { status: crate::protocol::WaitStatus::Ready, elapsed_ms: 0 }
                                                    .write(&mut stream);
                                                break 'wait;
                                            }
                                        }
                                    }
                                }
                            }

                            // Register for async evaluation
                            if let Err(err) = set_stream_nonblocking(&stream, true) {
                                let _ = Frame::Error(format!("set nonblocking: {err}")).write(&mut stream);
                                break 'wait;
                            }
                            pending_waits.push(PendingWait { stream, conditions, timeout_ms, registered_at: Instant::now() });
                        }
                        Ok(Frame::Expect { text, since_offset, timeout_ms }) => 'expect: {
                            if recorder.is_none() {
                                let _ = Frame::Error("recording not active".to_string()).write(&mut stream);
                                break 'expect;
                            }
                            // Check immediately — text may already be in the recording
                            let cast_path = root.join(id).join(crate::recording::CAST_FILE_NAME);
                            if let Some(ref mut rec) = recorder {
                                rec.flush();
                            }
                            if cast_path.exists() {
                                if let Ok(events) = crate::cast_reader::read_output_since(&cast_path, since_offset) {
                                    let output: String = events.iter().map(|e| e.data.as_str()).collect();
                                    if output.contains(&text) {
                                        let _ = Frame::ExpectResult { status: crate::protocol::WaitStatus::Ready, elapsed_ms: 0 }
                                            .write(&mut stream);
                                        break 'expect;
                                    }
                                }
                            }
                            if let Err(err) = set_stream_nonblocking(&stream, true) {
                                let _ = Frame::Error(format!("set nonblocking: {err}")).write(&mut stream);
                                break 'expect;
                            }
                            let initial_file_size = std::fs::metadata(&cast_path).map(|m| m.len()).unwrap_or(0);
                            pending_expects.push(PendingExpect {
                                stream,
                                text,
                                since_offset,
                                last_checked_file_size: initial_file_size,
                                timeout_ms,
                                registered_at: Instant::now(),
                            });
                        }
                        Ok(other) => {
                            let _ = Frame::Error(format!("unrecognized request: {other:?}")).write(&mut stream);
                        }
                        Err(err) => {
                            let _ = Frame::Error(format!("failed to read request: {err}")).write(&mut stream);
                        }
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(err) => return Err(format!("accept client: {err}")),
            }
        }

        if poll_result.client_readable {
            let mut client_disconnected = false;
            let mut pending = VecDeque::new();
            if let Some(stream) = active_client.as_mut() {
                loop {
                    match Frame::read(&mut stream.stream) {
                        Ok(frame) => pending.push_back(frame),
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(err)
                            if matches!(
                                err.kind(),
                                std::io::ErrorKind::UnexpectedEof
                                    | std::io::ErrorKind::BrokenPipe
                                    | std::io::ErrorKind::ConnectionReset
                                    | std::io::ErrorKind::ConnectionAborted
                            ) =>
                        {
                            client_disconnected = true;
                            break;
                        }
                        Err(err) => return Err(format!("read client frame: {err}")),
                    }
                }
            }

            while let Some(frame) = pending.pop_front() {
                match frame {
                    Frame::Input(bytes) => {
                        if let Some(ref mut rec) = recorder {
                            rec.input(&bytes, epoch.elapsed());
                        }
                        pty_child.write_all(&bytes)?;
                    }
                    Frame::Resize { cols, rows } => {
                        if let Some(ref mut rec) = recorder {
                            rec.event(crate::asciicast::EventCode::Resize, &format!("{}x{}", cols, rows), epoch.elapsed());
                        }
                        pty_child.resize(cols, rows)?;
                        vt_engine.resize(cols, rows)?;
                    }
                    _ => {}
                }
            }

            if client_disconnected && active_client.is_some() {
                let _ = fs::remove_file(foreground_path(root, id));
                if let Some(ref mut rec) = recorder {
                    rec.event(crate::asciicast::EventCode::Custom('d'), r#"{"client":"foreground"}"#, epoch.elapsed());
                }
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
                if let Some(ref mut rec) = recorder {
                    rec.event(crate::asciicast::EventCode::Custom('d'), r#"{"client":"foreground"}"#, epoch.elapsed());
                }
                active_client = None;
            }
        }

        if poll_result.pty_readable {
            loop {
                let mut buf = [0u8; PTY_READ_BUFFER_SIZE];
                match pty_child.read_output(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        last_pty_output_at = Some(Instant::now());
                        record_pty_output(vt_engine.as_mut(), &buf[..n])?;
                        if let Some(ref mut rec) = recorder {
                            let elapsed = epoch.elapsed();
                            rec.output(&buf[..n], elapsed);
                            if rec.output_bytes_since_snapshot() >= 256 * 1024 {
                                if let Ok(Some(payload)) = vt_engine.replay_payload(&vt::ClientCapabilities::conservative_fallback()) {
                                    let (cols, rows) = vt_engine.size();
                                    let state = String::from_utf8_lossy(&payload);
                                    rec.write_snapshot(&state, session.vt_engine.as_str(), cols, rows, elapsed);
                                } else {
                                    rec.reset_output_bytes_since_snapshot();
                                }
                            }
                        }
                        // Drain engine replies every iteration so the buffer never accumulates
                        // stale replies across an attach→detach transition. When attached, the
                        // host terminal is authoritative for query responses, so we discard.
                        let engine_reply = vt_engine.drain_replies();
                        if active_client.is_none() {
                            if let Some(ref mut tracker) = detached_da {
                                for reply in tracker.push(&buf[..n]) {
                                    pty_child.write_all(&reply)?;
                                }
                            }
                            if !engine_reply.is_empty() {
                                pty_child.write_all(&engine_reply)?;
                            }
                        }
                        if let Some(client) = active_client.as_mut() {
                            if client.enqueue_frame(&Frame::Output(buf[..n].to_vec())).is_err() {
                                let _ = fs::remove_file(foreground_path(root, id));
                                if let Some(ref mut rec) = recorder {
                                    rec.event(crate::asciicast::EventCode::Custom('d'), r#"{"client":"foreground"}"#, epoch.elapsed());
                                }
                                active_client = None;
                            }
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(err) => return Err(format!("read pty output: {err}")),
                }
            }
        }

        if let Some(ref mut rec) = recorder {
            if !poll_result.pty_readable && !poll_result.client_readable {
                rec.flush();
            }
        }

        // Evaluate pending waits on each loop tick.
        // Write errors are intentionally discarded — the client may have disconnected.
        pending_waits.retain_mut(|wait| {
            let elapsed = wait.registered_at.elapsed();
            let elapsed_ms = elapsed.as_millis() as u64;

            // Check timeout first
            if elapsed_ms >= wait.timeout_ms {
                let _ = Frame::WaitResult { status: crate::protocol::WaitStatus::Timeout, elapsed_ms }.write(&mut wait.stream);
                return false;
            }

            // Check conditions (OR semantics — any match wins)
            for condition in &wait.conditions {
                match condition {
                    crate::protocol::WaitCondition::OutputIdle { quiet_ms } => {
                        // Silence measured from max(registration_time, last_output_time)
                        let silence_since = match last_pty_output_at {
                            Some(t) if t > wait.registered_at => t,
                            _ => wait.registered_at,
                        };
                        let quiet_duration = silence_since.elapsed().as_millis() as u64;
                        if quiet_duration >= *quiet_ms {
                            let _ = Frame::WaitResult { status: crate::protocol::WaitStatus::Ready, elapsed_ms }.write(&mut wait.stream);
                            return false;
                        }
                    }
                    crate::protocol::WaitCondition::TextMatch { text } => {
                        if let Ok(screen) = vt_engine.screen_text() {
                            if screen.contains(text.as_str()) {
                                let _ =
                                    Frame::WaitResult { status: crate::protocol::WaitStatus::Ready, elapsed_ms }.write(&mut wait.stream);
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
            if let Some(ref mut rec) = recorder {
                rec.flush();
            }
            let cast_path = root.join(id).join(crate::recording::CAST_FILE_NAME);
            pending_expects.retain_mut(|expect| {
                let elapsed = expect.registered_at.elapsed();
                let elapsed_ms = elapsed.as_millis() as u64;

                if elapsed_ms >= expect.timeout_ms {
                    let _ = Frame::ExpectResult { status: crate::protocol::WaitStatus::Timeout, elapsed_ms }.write(&mut expect.stream);
                    return false;
                }

                if cast_path.exists() {
                    let file_size = std::fs::metadata(&cast_path).map(|m| m.len()).unwrap_or(0);
                    if file_size > expect.last_checked_file_size {
                        expect.last_checked_file_size = file_size;
                        if let Ok(events) = crate::cast_reader::read_output_since(&cast_path, expect.since_offset) {
                            let output: String = events.iter().map(|e| e.data.as_str()).collect();
                            if output.contains(&expect.text) {
                                let _ = Frame::ExpectResult { status: crate::protocol::WaitStatus::Ready, elapsed_ms }
                                    .write(&mut expect.stream);
                                return false;
                            }
                        }
                    }
                }

                true
            });
        }

        if let Some(status) = pty_child.exited()? {
            drain_pty_output_after_exit(
                &pty_child,
                vt_engine.as_mut(),
                &mut recorder,
                &mut active_client,
                &mut detached_da,
                root,
                id,
                session.vt_engine,
                epoch,
                &mut last_pty_output_at,
            )?;
            for mut wait in pending_waits.drain(..) {
                let elapsed_ms = wait.registered_at.elapsed().as_millis() as u64;
                let _ = Frame::WaitResult { status: crate::protocol::WaitStatus::SessionGone, elapsed_ms }.write(&mut wait.stream);
            }
            for mut expect in pending_expects.drain(..) {
                let elapsed_ms = expect.registered_at.elapsed().as_millis() as u64;
                let _ = Frame::ExpectResult { status: crate::protocol::WaitStatus::SessionGone, elapsed_ms }.write(&mut expect.stream);
            }
            if let Some(ref mut rec) = recorder {
                // Flush any held-back incomplete UTF-8 bytes before the exit
                // event so they appear in the correct order in the cast file.
                rec.flush_final();
                let code = exit_code_from_wait_status(&status);
                rec.event(crate::asciicast::EventCode::Exit, &code.to_string(), epoch.elapsed());
            }
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
    if recorder.is_none() {
        let _ = fs::remove_dir_all(&session_dir);
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
pub fn run_session_daemon(_root: &Path, _session: &SessionMetadata) -> Result<(), String> {
    Err("session daemon is only supported on unix".into())
}

#[cfg(any(unix, windows))]
fn build_inspect_result(
    session: &SessionMetadata,
    vt_engine: &dyn VtEngine,
    active_client: &Option<ActiveClient>,
    pty_child: &PtyChild,
    recorder: &Option<crate::recording::SessionRecorder>,
    markers: &std::collections::HashMap<String, u64>,
) -> crate::protocol::InspectResult {
    let (cols, rows) = vt_engine.size();
    let foreground_pgid = pty_child.foreground_pgid();

    crate::protocol::InspectResult {
        session: crate::protocol::SessionInspect {
            id: session.id.clone(),
            state: "running".to_string(),
            vt_engine: session.vt_engine.as_str().to_string(),
            vt_engine_status: crate::vt::vt_engine_status(session.vt_engine).to_string(),
            functional_vt_available: crate::vt::functional_vt_available(),
            cwd: session.cwd.clone(),
            cmd: session.cmd.clone(),
        },
        terminal: crate::protocol::TerminalInspect { rows, cols },
        process: crate::protocol::ProcessInspect {
            leader_pid: pty_child.leader_pid(),
            foreground_pgid,
            leader_cwd: pty_child.leader_cwd(),
            foreground_cwd: pty_child.foreground_cwd(),
        },
        attachments: if active_client.is_some() {
            vec![crate::protocol::AttachmentInspect { role: "controller".to_string() }]
        } else {
            vec![]
        },
        recording: crate::protocol::RecordingInspect {
            active: recorder.as_ref().is_some_and(|r| !r.is_paused()),
            bytes_written: recorder.as_ref().map(|r| r.bytes_written()).unwrap_or(0),
            markers: markers.clone(),
        },
    }
}

fn drain_pty_output_after_exit(
    pty_child: &PtyChild,
    vt_engine: &mut dyn VtEngine,
    recorder: &mut Option<crate::recording::SessionRecorder>,
    active_client: &mut Option<ActiveClient>,
    detached_da: &mut Option<DeviceAttributeTracker>,
    root: &Path,
    id: &str,
    vt_engine_kind: VtEngineKind,
    epoch: Instant,
    last_pty_output_at: &mut Option<Instant>,
) -> Result<(), String> {
    loop {
        let mut buf = [0u8; PTY_READ_BUFFER_SIZE];
        match pty_child.read_output(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                *last_pty_output_at = Some(Instant::now());
                record_pty_output(vt_engine, &buf[..n])?;
                if let Some(ref mut rec) = recorder {
                    let elapsed = epoch.elapsed();
                    rec.output(&buf[..n], elapsed);
                    if rec.output_bytes_since_snapshot() >= 256 * 1024 {
                        if let Ok(Some(payload)) = vt_engine.replay_payload(&vt::ClientCapabilities::conservative_fallback()) {
                            let (cols, rows) = vt_engine.size();
                            let state = String::from_utf8_lossy(&payload);
                            rec.write_snapshot(&state, vt_engine_kind.as_str(), cols, rows, elapsed);
                        } else {
                            rec.reset_output_bytes_since_snapshot();
                        }
                    }
                }

                let engine_reply = vt_engine.drain_replies();
                if active_client.is_none() {
                    if let Some(ref mut tracker) = detached_da {
                        for reply in tracker.push(&buf[..n]) {
                            pty_child.write_all(&reply)?;
                        }
                    }
                    if !engine_reply.is_empty() {
                        pty_child.write_all(&engine_reply)?;
                    }
                }
                if let Some(client) = active_client.as_mut() {
                    if client.enqueue_frame(&Frame::Output(buf[..n].to_vec())).is_err() {
                        let _ = fs::remove_file(foreground_path(root, id));
                        if let Some(ref mut rec) = recorder {
                            rec.event(crate::asciicast::EventCode::Custom('d'), r#"{"client":"foreground"}"#, epoch.elapsed());
                        }
                        *active_client = None;
                    }
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(err) => return Err(format!("read pty output after exit: {err}")),
        }
    }
    Ok(())
}

struct ActiveClient {
    stream: SessionStream,
    pending_output: Vec<u8>,
}

impl ActiveClient {
    fn new(stream: SessionStream) -> Self {
        Self { stream, pending_output: Vec::new() }
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
        apply_attach_state, attach_init_capabilities, default_vt_engine, record_pty_output, AttachCleanupGuard, TestReplayProbeVtEngine,
    };
    use crate::vt::{self, VtEngine};

    #[test]
    fn cleanup_guard_writes_on_drop() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let guard = AttachCleanupGuard::test_buffer(Arc::clone(&output));

        drop(guard);

        assert_eq!(
            *output.lock().expect("lock output"),
            b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1004l\x1b[?1049l\x1b[<u\x1b[?25h"
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
            b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1004l\x1b[?1049l\x1b[<u\x1b[?25h"
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
        let mut client = super::ActiveClient { stream, pending_output: vec![0; super::MAX_PENDING_CLIENT_OUTPUT_BYTES - 1] };

        let err = client.enqueue_frame(&super::Frame::Output(vec![1])).expect_err("backlog should overflow");

        assert!(err.contains("client output backlog exceeded"));
    }

    #[test]
    fn default_vt_engine_starts_with_default_size() {
        let engine = default_vt_engine(vt::default_vt_engine_kind()).expect("create default vt engine");
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
        let mut engine = default_vt_engine(vt::default_vt_engine_kind()).expect("create default vt engine");
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
