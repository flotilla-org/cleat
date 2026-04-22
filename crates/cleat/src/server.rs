use std::{
    os::unix::net::UnixStream,
    path::Path,
    time::{Duration, Instant},
};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use crate::{
    protocol::{Frame, SessionInfo, SessionStatus},
    runtime::RuntimeLayout,
    session::{attach_foreground, ensure_session_started, run_session_daemon, session_socket_path, ForegroundAttach},
    vt::VtEngineKind,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartBound {
    Offset(u64),
    Marker(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum EndBound {
    Offset(u64),
    Marker(String),
    NextMarker,
    IdleGap(Duration),
    EndOfRecording,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FallbackReason {
    /// `--until-next-marker` hit EOF without finding another marker.
    NoMarkerAfterStart,
    /// `--until-idle <dur>` hit EOF without finding a gap of that duration.
    NoIdleGap(Duration),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceOutcome {
    /// Byte offset where the slice started (resolved from `StartBound`).
    pub start_offset: u64,
    /// Byte offset where the slice ended, exclusive (resolved from `EndBound`,
    /// or file size if the soft ceiling fell back to EOF).
    pub end_offset: u64,
    /// `None` if the intended end bound was reached. `Some(reason)` when a
    /// soft-ceiling fallback to EOF kicked in. Primarily for future JSON
    /// output; the CLI uses it to decide whether to emit a stderr note.
    pub end_status: Option<FallbackReason>,
}

#[derive(Debug, Clone)]
pub struct SessionService {
    layout: RuntimeLayout,
}

impl SessionService {
    pub fn new(layout: RuntimeLayout) -> Self {
        Self { layout }
    }

    pub fn discover() -> Self {
        Self::new(RuntimeLayout::discover())
    }

    pub fn layout_root(&self) -> &std::path::Path {
        self.layout.root()
    }

    pub fn create(
        &self,
        name: Option<String>,
        vt_engine: Option<VtEngineKind>,
        cwd: Option<std::path::PathBuf>,
        cmd: Option<String>,
        record: bool,
    ) -> Result<SessionInfo, String> {
        let session = ensure_session_started(&self.layout, name, vt_engine, cwd, cmd, record)?;
        // If the daemon was already running, get real config via inspect.
        if let Ok(result) = self.inspect(&session.id) {
            return Ok(session_info_from_inspect(result, SessionStatus::Detached));
        }
        Ok(SessionInfo {
            id: session.id,
            vt_engine: session.vt_engine,
            vt_engine_status: crate::vt::vt_engine_status(session.vt_engine).to_string(),
            functional_vt_available: crate::vt::functional_vt_available(),
            cwd: session.cwd,
            cmd: session.cmd,
            status: SessionStatus::Detached,
            error: None,
        })
    }

    pub fn list(&self) -> Result<Vec<SessionInfo>, String> {
        if !self.layout.root().exists() {
            return Ok(vec![]);
        }

        let mut sessions = Vec::new();
        let entries =
            std::fs::read_dir(self.layout.root()).map_err(|err| format!("read runtime root {}: {err}", self.layout.root().display()))?;

        for entry in entries {
            let entry = entry.map_err(|err| format!("read runtime entry: {err}"))?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let id = match path.file_name().and_then(|n| n.to_str()) {
                Some(name) => name.to_string(),
                None => continue,
            };
            let socket_path = session_socket_path(self.layout.root(), &id);
            if !socket_path.exists() {
                continue;
            }
            if !is_session_daemon_alive(self.layout.root(), &id) {
                let _ = self.layout.remove_session(&id);
                continue;
            }
            match self.inspect(&id) {
                Ok(result) => {
                    let status = if result.attachments.is_empty() { SessionStatus::Detached } else { SessionStatus::Attached };
                    sessions.push(SessionInfo {
                        id: result.session.id,
                        vt_engine: parse_vt_engine_kind(&result.session.vt_engine),
                        vt_engine_status: result.session.vt_engine_status,
                        functional_vt_available: result.session.functional_vt_available,
                        cwd: result.session.cwd,
                        cmd: result.session.cmd,
                        status,
                        error: None,
                    });
                }
                Err(err) => {
                    sessions.push(SessionInfo {
                        id,
                        vt_engine: crate::vt::default_vt_engine_kind(),
                        vt_engine_status: String::new(),
                        functional_vt_available: false,
                        cwd: None,
                        cmd: None,
                        status: SessionStatus::Detached,
                        error: Some(err),
                    });
                }
            }
        }
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(sessions)
    }

    pub fn kill(&self, id: &str) -> Result<(), String> {
        if !self.layout.root().join(id).exists() {
            return Err(format!("missing session {id}"));
        }
        let pid_path = crate::session::daemon_pid_path(self.layout.root(), id);
        if let Ok(Some(pid)) = std::fs::read_to_string(&pid_path).map(|value| value.trim().parse::<i32>().ok()) {
            if is_expected_bollard_process(pid) {
                // SAFETY: the pid was verified to belong to a cleat process before signaling it.
                unsafe {
                    libc::kill(pid, libc::SIGTERM);
                }
            }
        }
        self.layout.remove_session(id)
    }

    pub fn detach(&self, id: &str) -> Result<(), String> {
        if !self.layout.root().join(id).exists() {
            return Err(format!("missing session {id}"));
        }

        let socket_path = session_socket_path(self.layout.root(), id);
        let mut stream = connect_session_socket(&socket_path)?;
        Frame::Detach.write(&mut stream).map_err(|err| format!("write detach request: {err}"))?;
        Ok(())
    }

    pub fn capture(&self, id: &str) -> Result<String, String> {
        if !self.layout.root().join(id).exists() {
            return Err(format!("missing session {id}"));
        }

        let socket_path = session_socket_path(self.layout.root(), id);
        let mut stream = connect_session_socket(&socket_path)?;
        Frame::Capture.write(&mut stream).map_err(|err| format!("write capture request: {err}"))?;
        match Frame::read(&mut stream).map_err(|err| format!("read capture response: {err}"))? {
            Frame::Output(bytes) => String::from_utf8(bytes).map_err(|err| format!("capture response was not valid utf-8: {err}")),
            Frame::Error(message) => Err(message),
            other => Err(format!("unexpected capture response: {other:?}")),
        }
    }

    pub fn capture_slice_raw(&self, id: &str, start: StartBound, end: EndBound) -> Result<(String, SliceOutcome), String> {
        self.capture_slice_inner(id, start, end)
    }

    pub fn capture_slice_text(&self, id: &str, start: StartBound, end: EndBound) -> Result<(String, SliceOutcome), String> {
        // Today raw and text produce the same output; separation is for
        // future VT-rendered transcripts.
        self.capture_slice_inner(id, start, end)
    }

    fn capture_slice_inner(&self, id: &str, start: StartBound, end: EndBound) -> Result<(String, SliceOutcome), String> {
        let cast_path = self.layout.root().join(id).join(crate::recording::CAST_FILE_NAME);
        if !cast_path.exists() {
            return Err(format!("no recording for session {id}"));
        }

        let start_offset = match start {
            StartBound::Offset(o) => o,
            StartBound::Marker(name) => self.resolve_marker(id, &name)?,
        };

        let file_size = std::fs::metadata(&cast_path).map_err(|e| format!("stat cast file: {e}"))?.len();

        let (end_offset, end_status) = match end {
            EndBound::EndOfRecording => (file_size, None),
            EndBound::Offset(o) => {
                if o < start_offset {
                    return Err(format!("end offset {o} precedes start offset {start_offset}"));
                }
                (o, None)
            }
            EndBound::Marker(name) => {
                let o = self.resolve_marker(id, &name)?;
                // Strict "after start" for named markers — equal-offset is almost
                // always a typo (e.g. `--since-marker m1 --until-marker m1`).
                // Raw offsets keep `<` so `--since 0 --until 0` is a legal empty slice.
                if o <= start_offset {
                    return Err(format!("marker '{name}' at offset {o} is not after start offset {start_offset}"));
                }
                (o, None)
            }
            EndBound::NextMarker => match self.resolve_next_marker_after(id, start_offset)? {
                Some(o) => (o, None),
                None => (file_size, Some(FallbackReason::NoMarkerAfterStart)),
            },
            EndBound::IdleGap(duration) => match crate::cast_reader::find_idle_gap_after(&cast_path, start_offset, duration)? {
                Some(o) => (o, None),
                None => (file_size, Some(FallbackReason::NoIdleGap(duration))),
            },
        };

        let events = crate::cast_reader::read_output_between(&cast_path, start_offset, end_offset)?;
        let output: String = events.iter().map(|e| e.data.as_str()).collect();
        Ok((output, SliceOutcome { start_offset, end_offset, end_status }))
    }

    pub fn send_keys(&self, id: &str, bytes: &[u8]) -> Result<(), String> {
        if !self.layout.root().join(id).exists() {
            return Err(format!("missing session {id}"));
        }

        let socket_path = session_socket_path(self.layout.root(), id);
        let mut stream = connect_session_socket(&socket_path)?;
        Frame::SendKeys(bytes.to_vec()).write(&mut stream).map_err(|err| format!("write send-keys request: {err}"))
    }

    pub fn send_keys_with_mark(&self, id: &str, bytes: &[u8], marker_name: &str) -> Result<u64, String> {
        if !self.layout.root().join(id).exists() {
            return Err(format!("missing session {id}"));
        }
        let socket_path = session_socket_path(self.layout.root(), id);
        let mut stream = connect_session_socket(&socket_path)?;
        Frame::SendKeysWithMark { bytes: bytes.to_vec(), marker_name: marker_name.to_string() }
            .write(&mut stream)
            .map_err(|err| format!("write send-keys-with-mark request: {err}"))?;
        match Frame::read(&mut stream).map_err(|err| format!("read send-keys-with-mark response: {err}"))? {
            Frame::MarkResult { offset } => Ok(offset),
            Frame::Error(message) => Err(message),
            other => Err(format!("unexpected send-keys-with-mark response: {other:?}")),
        }
    }

    pub fn attach(
        &self,
        name: Option<String>,
        vt_engine: Option<VtEngineKind>,
        cwd: Option<std::path::PathBuf>,
        cmd: Option<String>,
        no_create: bool,
    ) -> Result<(SessionInfo, ForegroundAttach), String> {
        let session = if no_create {
            let id = name.ok_or_else(|| "attach --no-create requires a session id".to_string())?;
            let socket_path = session_socket_path(self.layout.root(), &id);
            if !socket_path.exists() {
                return Err(format!("missing session {id}"));
            }
            if !is_session_daemon_alive(self.layout.root(), &id) {
                let _ = self.layout.remove_session(&id);
                return Err(format!("session {id} has a stale daemon (cleaned up)"));
            }
            let vt_engine = vt_engine.unwrap_or_else(crate::vt::default_vt_engine_kind);
            crate::runtime::SessionMetadata { id, vt_engine, cwd, cmd, record: false }
        } else {
            ensure_session_started(&self.layout, name, vt_engine, cwd, cmd, false)?
        };
        // Get real config from the daemon before attaching (which takes the foreground slot).
        let info = if let Ok(result) = self.inspect(&session.id) {
            session_info_from_inspect(result, SessionStatus::Attached)
        } else {
            SessionInfo {
                id: session.id.clone(),
                vt_engine: session.vt_engine,
                vt_engine_status: crate::vt::vt_engine_status(session.vt_engine).to_string(),
                functional_vt_available: crate::vt::functional_vt_available(),
                cwd: session.cwd,
                cmd: session.cmd,
                status: SessionStatus::Attached,
                error: None,
            }
        };
        let attach = attach_foreground(&self.layout, &info.id)?;
        Ok((info, attach))
    }

    pub fn inspect(&self, id: &str) -> Result<crate::protocol::InspectResult, String> {
        if !self.layout.root().join(id).exists() {
            return Err(format!("missing session {id}"));
        }
        let socket_path = session_socket_path(self.layout.root(), id);
        let mut stream = connect_session_socket(&socket_path)?;
        Frame::Inspect.write(&mut stream).map_err(|err| format!("write inspect request: {err}"))?;
        match Frame::read(&mut stream).map_err(|err| format!("read inspect response: {err}"))? {
            Frame::InspectResult(json) => serde_json::from_slice(&json).map_err(|err| format!("parse inspect response: {err}")),
            Frame::Error(message) => Err(message),
            other => Err(format!("unexpected inspect response: {other:?}")),
        }
    }

    pub fn signal(&self, id: &str, signal: i32, target: crate::protocol::SignalTarget) -> Result<(), String> {
        if !self.layout.root().join(id).exists() {
            return Err(format!("missing session {id}"));
        }
        let socket_path = session_socket_path(self.layout.root(), id);
        let mut stream = connect_session_socket(&socket_path)?;
        Frame::Signal { signal, target }.write(&mut stream).map_err(|err| format!("write signal request: {err}"))?;
        match Frame::read(&mut stream).map_err(|err| format!("read signal response: {err}"))? {
            Frame::Ack => Ok(()),
            Frame::Error(message) => Err(message),
            other => Err(format!("unexpected signal response: {other:?}")),
        }
    }

    pub fn mark(&self, id: &str) -> Result<u64, String> {
        self.mark_impl(id, None)
    }

    pub fn named_mark(&self, id: &str, name: &str) -> Result<u64, String> {
        self.mark_impl(id, Some(name))
    }

    fn mark_impl(&self, id: &str, name: Option<&str>) -> Result<u64, String> {
        if !self.layout.root().join(id).exists() {
            return Err(format!("missing session {id}"));
        }
        let socket_path = session_socket_path(self.layout.root(), id);
        let mut stream = connect_session_socket(&socket_path)?;
        Frame::Mark { name: name.map(|n| n.to_string()) }.write(&mut stream).map_err(|e| format!("write mark: {e}"))?;
        match Frame::read(&mut stream).map_err(|e| format!("read mark response: {e}"))? {
            Frame::MarkResult { offset } => Ok(offset),
            Frame::Error(msg) => Err(msg),
            other => Err(format!("unexpected mark response: {other:?}")),
        }
    }

    pub fn resolve_marker(&self, id: &str, name: &str) -> Result<u64, String> {
        if !self.layout.root().join(id).exists() {
            return Err(format!("missing session {id}"));
        }
        let socket_path = session_socket_path(self.layout.root(), id);
        let mut stream = connect_session_socket(&socket_path)?;
        Frame::ResolveMarker { name: name.to_string() }.write(&mut stream).map_err(|e| format!("write resolve: {e}"))?;
        match Frame::read(&mut stream).map_err(|e| format!("read resolve response: {e}"))? {
            Frame::MarkResult { offset } => Ok(offset),
            Frame::Error(msg) => Err(msg),
            other => Err(format!("unexpected resolve response: {other:?}")),
        }
    }

    pub fn resolve_next_marker_after(&self, id: &str, after: u64) -> Result<Option<u64>, String> {
        if !self.layout.root().join(id).exists() {
            return Err(format!("missing session {id}"));
        }
        let socket_path = session_socket_path(self.layout.root(), id);
        let mut stream = connect_session_socket(&socket_path)?;
        Frame::ResolveNextMarker { after }.write(&mut stream).map_err(|e| format!("write resolve-next: {e}"))?;
        match Frame::read(&mut stream).map_err(|e| format!("read resolve-next response: {e}"))? {
            Frame::MarkResult { offset } => Ok(Some(offset)),
            Frame::MarkNotFound => Ok(None),
            Frame::Error(msg) => Err(msg),
            other => Err(format!("unexpected resolve-next response: {other:?}")),
        }
    }

    pub fn record(&self, id: &str, enable: bool) -> Result<(), String> {
        if !self.layout.root().join(id).exists() {
            return Err(format!("missing session {id}"));
        }
        let socket_path = session_socket_path(self.layout.root(), id);
        let mut stream = connect_session_socket(&socket_path)?;
        Frame::RecordControl { enable }.write(&mut stream).map_err(|err| format!("write record control: {err}"))?;
        match Frame::read(&mut stream).map_err(|err| format!("read record response: {err}"))? {
            Frame::Ack => Ok(()),
            Frame::Error(message) => Err(message),
            other => Err(format!("unexpected record response: {other:?}")),
        }
    }

    pub fn wait(
        &self,
        id: &str,
        conditions: Vec<crate::protocol::WaitCondition>,
        timeout_ms: u64,
    ) -> Result<(crate::protocol::WaitStatus, u64), String> {
        if !self.layout.root().join(id).exists() {
            return Err(format!("missing session {id}"));
        }
        let socket_path = session_socket_path(self.layout.root(), id);
        let mut stream = connect_session_socket(&socket_path)?;
        // The wait response can take up to timeout_ms plus some overhead.
        // Remove any default read timeout so the blocking read succeeds.
        stream.set_read_timeout(Some(Duration::from_millis(timeout_ms + 5000))).map_err(|err| format!("set read timeout: {err}"))?;
        Frame::Wait { conditions, timeout_ms }.write(&mut stream).map_err(|err| format!("write wait request: {err}"))?;
        match Frame::read(&mut stream).map_err(|err| format!("read wait response: {err}"))? {
            Frame::WaitResult { status, elapsed_ms } => Ok((status, elapsed_ms)),
            Frame::Error(message) => Err(message),
            other => Err(format!("unexpected wait response: {other:?}")),
        }
    }

    pub fn expect(&self, id: &str, text: &str, since_offset: u64, timeout_ms: u64) -> Result<(crate::protocol::WaitStatus, u64), String> {
        if !self.layout.root().join(id).exists() {
            return Err(format!("missing session {id}"));
        }
        let socket_path = session_socket_path(self.layout.root(), id);
        let mut stream = connect_session_socket(&socket_path)?;
        stream.set_read_timeout(Some(Duration::from_millis(timeout_ms + 5000))).map_err(|err| format!("set read timeout: {err}"))?;
        Frame::Expect { text: text.to_string(), since_offset, timeout_ms }
            .write(&mut stream)
            .map_err(|err| format!("write expect request: {err}"))?;
        match Frame::read(&mut stream).map_err(|err| format!("read expect response: {err}"))? {
            Frame::ExpectResult { status, elapsed_ms } => Ok((status, elapsed_ms)),
            Frame::Error(message) => Err(message),
            other => Err(format!("unexpected expect response: {other:?}")),
        }
    }

    pub fn serve(&self, session: &crate::runtime::SessionMetadata) -> Result<(), String> {
        run_session_daemon(self.layout.root(), session)
    }
}

fn parse_vt_engine_kind(s: &str) -> VtEngineKind {
    match s {
        "ghostty" => VtEngineKind::Ghostty,
        _ => VtEngineKind::Passthrough,
    }
}

fn session_info_from_inspect(result: crate::protocol::InspectResult, status: SessionStatus) -> SessionInfo {
    SessionInfo {
        id: result.session.id,
        vt_engine: parse_vt_engine_kind(&result.session.vt_engine),
        vt_engine_status: result.session.vt_engine_status,
        functional_vt_available: result.session.functional_vt_available,
        cwd: result.session.cwd,
        cmd: result.session.cmd,
        status,
        error: None,
    }
}

fn connect_session_socket(socket_path: &Path) -> Result<UnixStream, String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match UnixStream::connect(socket_path) {
            Ok(stream) => return Ok(stream),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound && Instant::now() < deadline => {
                // Socket not yet created — daemon may still be starting up.
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => return Err(format!("connect {}: {err}", socket_path.display())),
        }
    }
}

/// Check whether the daemon for a session is still alive by reading its PID file
/// and verifying the process exists and is a cleat process.
///
/// Returns `true` if the daemon is alive OR if the PID file is missing (the daemon
/// may still be starting up — the socket is bound before the PID file is written).
/// Returns `false` only when the PID file exists and the process is dead, which is
/// the definitive signal that the daemon has exited and the session is stale.
fn is_session_daemon_alive(root: &Path, id: &str) -> bool {
    let pid_path = crate::session::daemon_pid_path(root, id);
    let Ok(contents) = std::fs::read_to_string(&pid_path) else {
        // No PID file yet — daemon may still be starting up. Don't treat as stale.
        return true;
    };
    let Some(pid) = contents.trim().parse::<i32>().ok() else {
        return false;
    };
    is_expected_bollard_process(pid)
}

fn is_expected_bollard_process(pid: i32) -> bool {
    let mut sys = System::new();
    let sysinfo_pid = Pid::from(pid as usize);
    sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[sysinfo_pid]), true, ProcessRefreshKind::nothing());
    sys.process(sysinfo_pid).map(|process| process.name().to_string_lossy().contains("cleat")).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::net::UnixListener, process::Command, sync::mpsc, thread, time::Duration};

    use super::SessionService;
    use crate::{
        protocol::Frame,
        runtime::RuntimeLayout,
        session::{daemon_pid_path, session_socket_path},
    };

    #[test]
    fn kill_does_not_signal_unrelated_process_from_stale_pid_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));
        let session_dir = temp.path().join("alpha");
        fs::create_dir_all(&session_dir).expect("create session dir");

        let mut child = Command::new("sleep").arg("30").spawn().expect("spawn sleep");
        fs::write(daemon_pid_path(temp.path(), "alpha"), child.id().to_string()).expect("write pid");

        service.kill("alpha").expect("kill session");

        thread::sleep(Duration::from_millis(50));
        assert!(child.try_wait().expect("try_wait").is_none(), "unrelated process should still be alive");

        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn send_keys_missing_session_is_an_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));

        let err = service.send_keys("missing", b"hello").expect_err("missing session should error");

        assert!(err.contains("missing"));
    }

    #[test]
    fn send_keys_writes_frame_to_session_socket() {
        let temp = tempfile::tempdir().expect("tempdir");
        let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));
        let session_dir = temp.path().join("alpha");
        fs::create_dir_all(&session_dir).expect("create session dir");

        let socket_path = session_socket_path(temp.path(), "alpha");
        let listener = UnixListener::bind(&socket_path).expect("bind socket");
        let (tx, rx) = mpsc::channel();
        let reader = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept connection");
            let frame = Frame::read(&mut stream).expect("read frame");
            tx.send(frame).expect("send frame");
        });

        service.send_keys("alpha", b"hello\r").expect("send keys");
        let frame = rx.recv_timeout(Duration::from_secs(1)).expect("receive frame");

        reader.join().expect("join reader");
        assert_eq!(frame, Frame::SendKeys(b"hello\r".to_vec()));
    }

    #[test]
    fn list_includes_sessions_with_inspect_failure() {
        let temp = tempfile::tempdir().expect("tempdir");
        let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));

        // Create a session directory with a live socket that accepts but immediately closes
        // the connection, simulating a daemon that doesn't respond to inspect.
        let session_dir = temp.path().join("broken-session");
        fs::create_dir_all(&session_dir).expect("create session dir");
        let socket_path = session_socket_path(temp.path(), "broken-session");
        let listener = UnixListener::bind(&socket_path).expect("bind socket");
        // Spawn a thread that accepts and immediately drops connections (simulates broken daemon).
        thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                drop(stream);
            }
        });
        // No PID file means is_session_daemon_alive returns true (assumes starting up).

        let sessions = service.list().expect("list sessions");
        assert_eq!(sessions.len(), 1, "broken session should appear in list");
        assert_eq!(sessions[0].id, "broken-session");
        assert!(sessions[0].error.is_some(), "should have error field set");
    }

    #[test]
    fn list_skips_and_cleans_up_stale_sessions() {
        let temp = tempfile::tempdir().expect("tempdir");
        let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));

        // Create a session directory with a socket file and a PID file pointing to a dead process.
        let session_dir = temp.path().join("stale-session");
        fs::create_dir_all(&session_dir).expect("create session dir");
        let socket_path = session_socket_path(temp.path(), "stale-session");
        // Create a socket file that nobody is listening on, then drop the listener.
        let listener = UnixListener::bind(&socket_path).expect("bind socket");
        drop(listener);
        // Write a PID that doesn't exist.
        fs::write(daemon_pid_path(temp.path(), "stale-session"), "999999999").expect("write pid");

        let sessions = service.list().expect("list sessions");
        assert!(sessions.is_empty(), "stale session should not appear in list");
        assert!(!session_dir.exists(), "stale session directory should be cleaned up");
    }
}
