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
            cwd: session.cwd,
            cmd: session.cmd,
            status: SessionStatus::Detached,
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
            if let Ok(result) = self.inspect(&id) {
                let status = if result.attachments.is_empty() { SessionStatus::Detached } else { SessionStatus::Attached };
                sessions.push(SessionInfo {
                    id: result.session.id,
                    vt_engine: parse_vt_engine_kind(&result.session.vt_engine),
                    cwd: result.session.cwd,
                    cmd: result.session.cmd,
                    status,
                });
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

    pub fn capture_since_raw(&self, id: &str, offset: u64) -> Result<String, String> {
        let cast_path = self.layout.root().join(id).join(crate::recording::CAST_FILE_NAME);
        if !cast_path.exists() {
            return Err(format!("no recording for session {id}"));
        }
        let events = crate::cast_reader::read_output_since(&cast_path, offset)?;
        let output: String = events.iter().map(|e| e.data.as_str()).collect();
        Ok(output)
    }

    pub fn capture_since_text(&self, id: &str, offset: u64) -> Result<String, String> {
        let cast_path = self.layout.root().join(id).join(crate::recording::CAST_FILE_NAME);
        if !cast_path.exists() {
            return Err(format!("no recording for session {id}"));
        }
        let events = crate::cast_reader::read_output_since(&cast_path, offset)?;
        // Phase 1: concatenate output event data directly.
        // Full VT replay (snapshot + engine) is a future enhancement.
        let output: String = events.iter().map(|e| e.data.as_str()).collect();
        Ok(output)
    }

    pub fn send_keys(&self, id: &str, bytes: &[u8]) -> Result<(), String> {
        if !self.layout.root().join(id).exists() {
            return Err(format!("missing session {id}"));
        }

        let socket_path = session_socket_path(self.layout.root(), id);
        let mut stream = connect_session_socket(&socket_path)?;
        Frame::SendKeys(bytes.to_vec()).write(&mut stream).map_err(|err| format!("write send-keys request: {err}"))
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
                cwd: session.cwd,
                cmd: session.cmd,
                status: SessionStatus::Attached,
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
        if !self.layout.root().join(id).exists() {
            return Err(format!("missing session {id}"));
        }
        let socket_path = session_socket_path(self.layout.root(), id);
        let mut stream = connect_session_socket(&socket_path)?;
        Frame::Mark { name: None }.write(&mut stream).map_err(|e| format!("write mark: {e}"))?;
        match Frame::read(&mut stream).map_err(|e| format!("read mark response: {e}"))? {
            Frame::MarkResult { offset } => Ok(offset),
            Frame::Error(msg) => Err(msg),
            other => Err(format!("unexpected mark response: {other:?}")),
        }
    }

    pub fn named_mark(&self, id: &str, name: &str) -> Result<u64, String> {
        if !self.layout.root().join(id).exists() {
            return Err(format!("missing session {id}"));
        }
        let socket_path = session_socket_path(self.layout.root(), id);
        let mut stream = connect_session_socket(&socket_path)?;
        Frame::Mark { name: Some(name.to_string()) }.write(&mut stream).map_err(|e| format!("write mark: {e}"))?;
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
        cwd: result.session.cwd,
        cmd: result.session.cmd,
        status,
    }
}

fn connect_session_socket(socket_path: &Path) -> Result<UnixStream, String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match UnixStream::connect(socket_path) {
            Ok(stream) => return Ok(stream),
            Err(err)
                if matches!(err.kind(), std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound)
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => return Err(format!("connect {}: {err}", socket_path.display())),
        }
    }
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
}
