use std::{
    io::Write,
    path::Path,
    thread,
    time::{Duration, Instant},
};

use http::{Method, StatusCode};
use serde::de::DeserializeOwned;

use crate::{
    http_uds,
    platform::{
        daemon::is_session_daemon_alive,
        ipc::{set_stream_read_timeout, try_connect_session_stream, SessionStream},
    },
    protocol::{AttachmentIdentity, SessionInfo, SessionStatus},
    runtime::{discoverable_runtime_roots, validate_runtime_name, DaemonCoordinates, RuntimeLayout, TerminalSize},
    session::{
        attach_foreground, ensure_session_started, run_session_daemon, start_session_in_running_daemon, watch_foreground, ForegroundAttach,
        SessionStartOptions,
    },
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonInstance {
    name: String,
    pid: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttachOptions {
    pub identity: AttachmentIdentity,
    pub strict: bool,
    pub take: bool,
}

impl DaemonInstance {
    pub fn name(&self) -> &str {
        &self.name
    }
}

#[derive(Debug, Clone, Copy)]
enum ListScope {
    Current,
    All,
}

impl SessionService {
    pub fn new(layout: RuntimeLayout) -> Self {
        Self { layout }
    }

    pub fn discover() -> Result<Self, String> {
        Ok(Self::new(RuntimeLayout::discover()?))
    }

    pub fn with_daemon(&self, daemon_name: String) -> Result<Self, String> {
        Ok(Self::new(self.layout.clone().with_daemon(daemon_name)?))
    }

    pub fn layout_root(&self) -> &std::path::Path {
        self.layout.root()
    }

    pub fn discover_daemons(&self) -> Vec<DaemonCoordinates> {
        let mut roots = discoverable_runtime_roots();
        roots.push(self.layout.root().to_path_buf());
        roots.sort();
        roots.dedup();

        let mut daemons = Vec::new();
        for root in roots {
            let Ok(entries) = std::fs::read_dir(&root) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() || !path.join("sessions").is_dir() {
                    continue;
                }
                let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                if validate_runtime_name(name).is_err() {
                    continue;
                }
                daemons.push(DaemonCoordinates { name: name.to_string(), runtime_root: root.clone() });
            }
        }
        daemons.sort_by(|left, right| left.runtime_root.cmp(&right.runtime_root).then_with(|| left.name.cmp(&right.name)));
        daemons.dedup();
        daemons
    }

    pub fn session_dir(&self, id: &str) -> std::path::PathBuf {
        self.layout.session_dir(id)
    }

    pub fn create(
        &self,
        name: Option<String>,
        vt_engine: Option<VtEngineKind>,
        cwd: Option<std::path::PathBuf>,
        cmd: Option<String>,
        record: bool,
    ) -> Result<SessionInfo, String> {
        self.create_with_size(name, vt_engine, cwd, cmd, record, TerminalSize::default())
    }

    pub fn create_with_size(
        &self,
        name: Option<String>,
        vt_engine: Option<VtEngineKind>,
        cwd: Option<std::path::PathBuf>,
        cmd: Option<String>,
        record: bool,
        initial_size: TerminalSize,
    ) -> Result<SessionInfo, String> {
        self.create_with_options(name, vt_engine, cwd, cmd, SessionStartOptions {
            record,
            initial_size,
            colors: crate::vt::TerminalColors::default(),
            tags: Vec::new(),
            environment: Vec::new(),
        })
    }

    pub fn create_with_options(
        &self,
        name: Option<String>,
        vt_engine: Option<VtEngineKind>,
        cwd: Option<std::path::PathBuf>,
        cmd: Option<String>,
        options: SessionStartOptions,
    ) -> Result<SessionInfo, String> {
        let session = ensure_session_started(&self.layout, name, vt_engine, cwd, cmd, options)?;
        Ok(self.session_info_after_create(session))
    }

    pub fn create_with_options_in_running_daemon(
        &self,
        daemon: &DaemonInstance,
        name: Option<String>,
        vt_engine: Option<VtEngineKind>,
        cwd: Option<std::path::PathBuf>,
        cmd: Option<String>,
        options: SessionStartOptions,
    ) -> Result<SessionInfo, String> {
        if self.layout.daemon_name() != daemon.name {
            return Err(format!("daemon instance {} does not match target {}", daemon.name, self.layout.daemon_name()));
        }
        let session = start_session_in_running_daemon(&self.layout, daemon.pid, name, vt_engine, cwd, cmd, options)?;
        Ok(self.session_info_after_create(session))
    }

    fn session_info_after_create(&self, session: crate::runtime::SessionMetadata) -> SessionInfo {
        // If the daemon was already running, get real config via inspect.
        if let Ok(result) = self.inspect(&session.id) {
            return session_info_from_inspect(result, SessionStatus::Detached);
        }
        SessionInfo {
            id: session.id,
            vt_engine: session.vt_engine,
            vt_engine_status: crate::vt::vt_engine_status(session.vt_engine).to_string(),
            functional_vt_available: crate::vt::functional_vt_available(),
            cwd: session.cwd,
            cmd: session.cmd,
            tags: session.tags,
            status: SessionStatus::Detached,
            screen_activity: crate::protocol::ScreenActivity::Stable,
            stable_since: None,
            last_output_at: None,
            controller: None,
            error: None,
        }
    }

    pub fn list(&self) -> Result<Vec<SessionInfo>, String> {
        self.list_with_selectors(&[])
    }

    pub fn list_with_selectors(&self, selectors: &[String]) -> Result<Vec<SessionInfo>, String> {
        self.list_daemons(selectors, ListScope::Current)
    }

    pub fn list_all_with_selectors(&self, selectors: &[String]) -> Result<Vec<SessionInfo>, String> {
        self.list_daemons(selectors, ListScope::All)
    }

    pub fn daemon_owning_session(&self, id: &str) -> Result<DaemonInstance, String> {
        if !self.layout.root().exists() {
            return Err(format!("missing session {id}"));
        }
        let mut owners = Vec::new();
        let mut candidate_errors = Vec::new();
        for daemon_name in self.daemon_names(ListScope::All)? {
            let daemon_service = self.with_daemon(daemon_name.clone())?;
            if !daemon_service.session_dir(id).is_dir() {
                continue;
            }
            let pid_before = daemon_service.registered_daemon_pid();
            match daemon_service.inspect(id) {
                Ok(_) => match (pid_before, daemon_service.registered_daemon_pid()) {
                    (Ok(before), Ok(after)) if before == after => owners.push(DaemonInstance { name: daemon_name, pid: after }),
                    (Ok(_), Ok(_)) => candidate_errors.push(format!("{daemon_name}: daemon changed while resolving session")),
                    (_, Err(err)) | (Err(err), _) => candidate_errors.push(format!("{daemon_name}: {err}")),
                },
                Err(err) => candidate_errors.push(format!("{daemon_name}: {err}")),
            }
        }

        match owners.as_slice() {
            [] if candidate_errors.is_empty() => Err(format!("missing session {id}")),
            [] => Err(format!("unable to resolve session {id}: {}", candidate_errors.join("; "))),
            [daemon] => Ok(daemon.clone()),
            _ => Err(format!(
                "session {id} exists in multiple daemons ({}); use --server to select the target daemon instead",
                owners.iter().map(|daemon| daemon.name.as_str()).collect::<Vec<_>>().join(", ")
            )),
        }
    }

    fn registered_daemon_pid(&self) -> Result<u32, String> {
        let path = self.layout.daemon_pid_path();
        let value = std::fs::read_to_string(&path).map_err(|err| format!("read daemon registration {}: {err}", path.display()))?;
        value.trim().parse().map_err(|err| format!("parse daemon registration {}: {err}", path.display()))
    }

    fn list_daemons(&self, selectors: &[String], scope: ListScope) -> Result<Vec<SessionInfo>, String> {
        if !self.layout.root().exists() {
            return Ok(vec![]);
        }

        let mut sessions = Vec::new();
        for daemon_name in self.daemon_names(scope)? {
            let daemon_service = self.with_daemon(daemon_name)?;
            sessions.extend(daemon_service.list_one_daemon_with_selectors(selectors)?);
        }
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(sessions)
    }

    fn daemon_names(&self, scope: ListScope) -> Result<Vec<String>, String> {
        match scope {
            ListScope::Current => {
                if self.layout.sessions_dir().is_dir() {
                    Ok(vec![self.layout.daemon_name().to_string()])
                } else {
                    Ok(vec![])
                }
            }
            ListScope::All => {
                let entries = std::fs::read_dir(self.layout.root())
                    .map_err(|err| format!("read runtime root {}: {err}", self.layout.root().display()))?;
                let mut names = Vec::new();
                for entry in entries {
                    let entry = entry.map_err(|err| format!("read runtime entry: {err}"))?;
                    let path = entry.path();
                    if !path.is_dir() || !path.join("sessions").is_dir() {
                        continue;
                    }
                    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        names.push(name.to_string());
                    }
                }
                names.sort();
                Ok(names)
            }
        }
    }

    fn list_one_daemon_with_selectors(&self, selectors: &[String]) -> Result<Vec<SessionInfo>, String> {
        if !self.layout.sessions_dir().is_dir() {
            return Ok(vec![]);
        }
        if daemon_control_is_unavailable(&self.layout) {
            return sweep_dead_daemon_sessions(&self.layout, "daemon control socket is unavailable".to_string())
                .map(|sessions| sessions.into_iter().filter(|session| session_matches_selectors(session, selectors)).collect());
        }

        match self.http_json_daemon::<_, http_uds::SessionListResponse>(Method::GET, "/sessions", &()) {
            Ok(result) => {
                let mut sessions = Vec::new();
                for result in result.sessions {
                    let status =
                        if has_controller_attachment(&result.attachments) { SessionStatus::Attached } else { SessionStatus::Detached };
                    let info = SessionInfo {
                        id: result.session.id,
                        vt_engine: parse_vt_engine_kind(&result.session.vt_engine),
                        vt_engine_status: result.session.vt_engine_status,
                        functional_vt_available: result.session.functional_vt_available,
                        cwd: result.session.cwd,
                        cmd: result.session.cmd,
                        tags: result.session.tags,
                        status,
                        screen_activity: result.screen_activity,
                        stable_since: result.stable_since,
                        last_output_at: result.last_output_at,
                        controller: controller_identity(&result.attachments),
                        error: None,
                    };
                    if session_matches_selectors(&info, selectors) {
                        sessions.push(info);
                    }
                }
                Ok(sessions)
            }
            Err(err) => {
                if daemon_control_is_unavailable(&self.layout) {
                    sweep_dead_daemon_sessions(&self.layout, err)
                        .map(|sessions| sessions.into_iter().filter(|session| session_matches_selectors(session, selectors)).collect())
                } else {
                    Err(err)
                }
            }
        }
    }

    pub fn kill(&self, id: &str) -> Result<(), String> {
        self.kill_with_purge(id, false)
    }

    pub fn kill_with_purge(&self, id: &str, purge: bool) -> Result<(), String> {
        if !self.layout.session_dir(id).exists() {
            return Err(format!("missing session {id}"));
        }
        if self.layout.socket_path().exists() && self.http_no_content(id, Method::DELETE, &format!("/sessions/{id}"), &()).is_ok() {
            self.wait_for_session_shutdown(id);
            if !self.layout.session_dir(id).exists() {
                return Ok(());
            }
        }
        if daemon_control_is_unavailable(&self.layout) {
            let _ = sweep_dead_daemon_sessions(&self.layout, "daemon control socket is unavailable".to_string())?;
            if purge && self.layout.session_dir(id).exists() {
                return self.layout.remove_session(id);
            }
            return Ok(());
        }
        if purge || !crate::recreate::session_is_recreatable(&self.layout.session_dir(id)) {
            self.layout.remove_session(id)
        } else {
            self.remove_volatile_session_files(id);
            Ok(())
        }
    }

    fn wait_for_session_shutdown(&self, id: &str) {
        for _ in 0..50 {
            if !self.layout.session_dir(id).exists() || self.inspect(id).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn remove_volatile_session_files(&self, id: &str) {
        let _ = std::fs::remove_file(self.layout.foreground_path(id));
    }

    pub fn detach(&self, id: &str) -> Result<(), String> {
        if !self.layout.session_dir(id).exists() {
            return Err(format!("missing session {id}"));
        }

        self.http_no_content(id, Method::POST, &format!("/sessions/{id}/detach"), &())
    }

    pub fn capture(&self, id: &str) -> Result<String, String> {
        if !self.layout.session_dir(id).exists() {
            return Err(format!("missing session {id}"));
        }

        let response: http_uds::ScreenResponse = self.http_json(id, Method::GET, &format!("/sessions/{id}/screen"), &())?;
        Ok(response.text)
    }

    pub fn capture_slice_raw(&self, id: &str, start: StartBound, end: EndBound) -> Result<(String, SliceOutcome), String> {
        self.capture_slice_inner(id, start, end)
    }

    pub fn capture_slice_text(&self, id: &str, start: StartBound, end: EndBound) -> Result<(String, SliceOutcome), String> {
        // Today raw and text produce the same output; separation is for
        // future VT-rendered transcripts.
        self.capture_slice_inner(id, start, end)
    }

    /// Resolve start and end bounds into byte offsets in the cast file.
    /// Returns `(start_offset, end_offset, end_status)` where `end_status` is
    /// `Some(FallbackReason)` when a soft-ceiling bound fell back to EOF.
    ///
    /// Used by both `capture_slice_inner` (which then reads the byte range)
    /// and `replay` (which streams it).
    pub fn resolve_slice_range(
        &self,
        id: &str,
        start: StartBound,
        end: EndBound,
        cast_path: &std::path::Path,
    ) -> Result<(u64, u64, Option<FallbackReason>), String> {
        let start_offset = match start {
            StartBound::Offset(o) => o,
            StartBound::Marker(name) => self.resolve_marker(id, &name)?,
        };

        let file_size = std::fs::metadata(cast_path).map_err(|e| format!("stat cast file: {e}"))?.len();

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
                // Strict "after start" for named markers — equal-offset is
                // almost always a typo (e.g. `--since-marker m1 --until-marker m1`).
                // Raw offsets keep `<` (above) so `--since 0 --until 0` is a
                // legal empty slice.
                if o <= start_offset {
                    return Err(format!("marker '{name}' at offset {o} is not after start offset {start_offset}"));
                }
                (o, None)
            }
            EndBound::NextMarker => match self.resolve_next_marker_after(id, start_offset)? {
                Some(o) => (o, None),
                None => (file_size, Some(FallbackReason::NoMarkerAfterStart)),
            },
            EndBound::IdleGap(duration) => match crate::cast_reader::find_idle_gap_after(cast_path, start_offset, duration)? {
                Some(o) => (o, None),
                None => (file_size, Some(FallbackReason::NoIdleGap(duration))),
            },
        };

        Ok((start_offset, end_offset, end_status))
    }

    fn capture_slice_inner(&self, id: &str, start: StartBound, end: EndBound) -> Result<(String, SliceOutcome), String> {
        let cast_path = self.layout.session_dir(id).join(crate::recording::CAST_FILE_NAME);
        if !cast_path.exists() {
            return Err(format!("no recording for session {id}"));
        }

        let (start_offset, end_offset, end_status) = self.resolve_slice_range(id, start, end, &cast_path)?;

        let events = crate::cast_reader::read_output_between(&cast_path, start_offset, end_offset)?;
        let output: String = events.iter().map(|e| e.data.as_str()).collect();
        Ok((output, SliceOutcome { start_offset, end_offset, end_status }))
    }

    pub fn send_keys(&self, id: &str, bytes: &[u8]) -> Result<(), String> {
        if !self.layout.session_dir(id).exists() {
            return Err(format!("missing session {id}"));
        }

        self.http_no_content(id, Method::POST, &format!("/sessions/{id}/keys"), &http_uds::KeysRequest { bytes: bytes.to_vec() })
    }

    pub fn send_keys_with_mark(&self, id: &str, bytes: &[u8], marker_name: &str) -> Result<u64, String> {
        if !self.layout.session_dir(id).exists() {
            return Err(format!("missing session {id}"));
        }
        let response: http_uds::MarkResponse =
            self.http_json(id, Method::POST, &format!("/sessions/{id}/keys-with-mark"), &http_uds::KeysWithMarkRequest {
                bytes: bytes.to_vec(),
                marker_name: marker_name.to_string(),
            })?;
        Ok(response.offset)
    }

    pub(crate) fn send_input(&self, id: &str, input: &http_uds::InputRequest) -> Result<(), String> {
        if !self.layout.session_dir(id).exists() {
            return Err(format!("missing session {id}"));
        }

        self.http_no_content(id, Method::POST, &format!("/sessions/{id}/input"), input)
    }

    pub(crate) fn send_paste_with_mark(&self, id: &str, text: &str, marker_name: &str) -> Result<u64, String> {
        if !self.layout.session_dir(id).exists() {
            return Err(format!("missing session {id}"));
        }

        let response: http_uds::MarkResponse =
            self.http_json(id, Method::POST, &format!("/sessions/{id}/paste-with-mark"), &http_uds::PasteWithMarkRequest {
                text: text.to_string(),
                marker_name: marker_name.to_string(),
            })?;
        Ok(response.offset)
    }

    pub fn attach(
        &self,
        name: Option<String>,
        vt_engine: Option<VtEngineKind>,
        cwd: Option<std::path::PathBuf>,
        cmd: Option<String>,
        no_create: bool,
        options: AttachOptions,
    ) -> Result<(SessionInfo, ForegroundAttach), String> {
        let session = if no_create {
            let id = name.ok_or_else(|| "attach --no-create requires a session id".to_string())?;
            if !self.layout.session_dir(&id).exists() {
                return Err(format!("missing session {id}"));
            }
            if self.inspect(&id).is_err() {
                if crate::recreate::session_is_recreatable(&self.layout.session_dir(&id)) {
                    return Err(format!("session {id} has a stale daemon (use attach without --no-create to recreate)"));
                }
                let _ = self.layout.remove_session(&id);
                return Err(format!("session {id} has a stale daemon (cleaned up)"));
            }
            let vt_engine = vt_engine.unwrap_or_else(crate::vt::default_vt_engine_kind);
            crate::runtime::SessionMetadata {
                id,
                vt_engine,
                cwd,
                cmd,
                record: false,
                initial_size: TerminalSize::default(),
                colors: crate::vt::TerminalColors::default(),
                tags: Vec::new(),
                environment: Vec::new(),
            }
        } else {
            ensure_session_started(&self.layout, name, vt_engine, cwd, cmd, SessionStartOptions::default())?
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
                tags: session.tags,
                status: SessionStatus::Attached,
                screen_activity: crate::protocol::ScreenActivity::Stable,
                stable_since: None,
                last_output_at: None,
                controller: None,
                error: None,
            }
        };
        let attach = attach_foreground(&self.layout, &info.id, options.identity, options.strict, options.take)?;
        Ok((info, attach))
    }

    pub fn watch(&self, id: &str, identity: AttachmentIdentity) -> Result<ForegroundAttach, String> {
        if !self.layout.session_dir(id).exists() {
            return Err(format!("missing session {id}"));
        }
        watch_foreground(&self.layout, id, identity)
    }

    pub fn connect_packets(
        &self,
        id: &str,
    ) -> Result<(crate::packet::PacketClient<SessionStream>, crate::packet::DirectorySnapshot), String> {
        if !self.layout.session_dir(id).exists() {
            return Err(format!("missing session {id}"));
        }
        self.connect_directory(&[]).and_then(|(client, directory)| {
            if directory.sessions.iter().any(|entry| entry.session_id == id) {
                Ok((client, directory))
            } else {
                Err(format!("session {id} was not present in packet directory"))
            }
        })
    }

    pub fn connect_directory(
        &self,
        selectors: &[String],
    ) -> Result<(crate::packet::PacketClient<SessionStream>, crate::packet::DirectorySnapshot), String> {
        let (client, directory, _) = self.connect_subscription(selectors, None)?;
        Ok((client, directory))
    }

    pub fn connect_activity(
        &self,
        selectors: &[String],
        stable_threshold: Duration,
    ) -> Result<(crate::packet::PacketClient<SessionStream>, crate::packet::ActivitySnapshot), String> {
        if stable_threshold.is_zero() {
            return Err("screen activity stability threshold must be greater than zero".to_string());
        }
        let stable_threshold_ms = u64::try_from(stable_threshold.as_millis())
            .map_err(|_| "screen activity stability threshold exceeds u64 milliseconds".to_string())?;
        if stable_threshold_ms == 0 {
            return Err("screen activity stability threshold must be at least one millisecond".to_string());
        }
        let (client, _, activity) = self.connect_subscription(selectors, Some(stable_threshold_ms))?;
        let activity = activity.ok_or_else(|| "packet stream did not send an activity snapshot".to_string())?;
        Ok((client, activity))
    }

    fn connect_subscription(
        &self,
        selectors: &[String],
        screen_activity_stable_ms: Option<u64>,
    ) -> Result<
        (crate::packet::PacketClient<SessionStream>, crate::packet::DirectorySnapshot, Option<crate::packet::ActivitySnapshot>),
        String,
    > {
        crate::session::ensure_daemon_started(&self.layout)?;

        let socket_path = self.layout.socket_path();
        let mut stream = connect_session_socket(&socket_path)?;
        let body = serde_json::to_vec(&http_uds::PacketSubscribeRequest { selectors: selectors.to_vec(), screen_activity_stable_ms })
            .map_err(|err| format!("serialize packet subscription request: {err}"))?;
        write!(
            stream,
            "POST /connect HTTP/1.1\r\nHost: cleat\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: Upgrade\r\nUpgrade: cleat-packet/1\r\n\r\n",
            body.len()
        )
        .map_err(|err| format!("write packet upgrade request: {err}"))?;
        stream.write_all(&body).map_err(|err| format!("write packet subscription request: {err}"))?;
        let response = http_uds::read_response_head(&mut stream).map_err(|err| format!("read packet upgrade response: {err}"))?;
        if response.status != StatusCode::SWITCHING_PROTOCOLS {
            return Err(format!("unexpected packet response: {}", response.status));
        }

        let mut client = crate::packet::PacketClient::new(stream);
        let hello = client.read_frame().map_err(|err| format!("read packet hello: {err}"))?;
        if hello.channel != crate::packet::CHANNEL_CONTROL || hello.msg_type != crate::packet::MSG_CONTROL_HELLO {
            return Err("packet stream did not start with control hello".to_string());
        }
        let hello = hello.decode::<crate::packet::ControlHello>().map_err(|err| format!("decode packet hello: {err}"))?;
        if hello.version != crate::packet::PROTOCOL_VERSION {
            return Err(format!("unsupported packet protocol version {}", hello.version));
        }

        let directory = client.read_frame().map_err(|err| format!("read packet directory: {err}"))?;
        if directory.channel != crate::packet::CHANNEL_CONTROL || directory.msg_type != crate::packet::MSG_CONTROL_DIRECTORY_SNAPSHOT {
            return Err("packet stream did not send a directory snapshot after hello".to_string());
        }
        let directory = directory.decode::<crate::packet::DirectorySnapshot>().map_err(|err| format!("decode packet directory: {err}"))?;
        let activity = if screen_activity_stable_ms.is_some() {
            let frame = client.read_frame().map_err(|err| format!("read packet activity snapshot: {err}"))?;
            if frame.channel != crate::packet::CHANNEL_CONTROL || frame.msg_type != crate::packet::MSG_CONTROL_ACTIVITY_SNAPSHOT {
                return Err("packet stream did not send an activity snapshot after the directory".to_string());
            }
            Some(frame.decode::<crate::packet::ActivitySnapshot>().map_err(|err| format!("decode packet activity snapshot: {err}"))?)
        } else {
            None
        };
        Ok((client, directory, activity))
    }

    pub fn inspect(&self, id: &str) -> Result<crate::protocol::InspectResult, String> {
        if !self.layout.session_dir(id).exists() {
            return Err(format!("missing session {id}"));
        }
        self.http_json(id, Method::GET, &format!("/sessions/{id}"), &())
    }

    pub fn update_tags(&self, id: &str, add: Vec<String>, remove: Vec<String>) -> Result<Vec<String>, String> {
        if !self.layout.session_dir(id).exists() {
            return Err(format!("missing session {id}"));
        }
        let response: http_uds::TagResponse =
            self.http_json(id, Method::POST, &format!("/sessions/{id}/tags"), &http_uds::TagRequest { add, remove })?;
        Ok(response.tags)
    }

    pub fn signal(&self, id: &str, signal: i32, target: crate::protocol::SignalTarget) -> Result<(), String> {
        if !self.layout.session_dir(id).exists() {
            return Err(format!("missing session {id}"));
        }
        self.http_no_content(id, Method::POST, &format!("/sessions/{id}/signal"), &http_uds::SignalRequest {
            signal,
            target: signal_target_to_http(target),
        })
    }

    pub fn mark(&self, id: &str) -> Result<u64, String> {
        self.mark_impl(id, None)
    }

    pub fn named_mark(&self, id: &str, name: &str) -> Result<u64, String> {
        self.mark_impl(id, Some(name))
    }

    fn mark_impl(&self, id: &str, name: Option<&str>) -> Result<u64, String> {
        if !self.layout.session_dir(id).exists() {
            return Err(format!("missing session {id}"));
        }
        let response: http_uds::MarkResponse =
            self.http_json(id, Method::POST, &format!("/sessions/{id}/mark"), &http_uds::MarkRequest { name: name.map(str::to_string) })?;
        Ok(response.offset)
    }

    pub fn resolve_marker(&self, id: &str, name: &str) -> Result<u64, String> {
        if !self.layout.session_dir(id).exists() {
            return Err(format!("missing session {id}"));
        }
        let response: http_uds::MarkResponse =
            self.http_json(id, Method::POST, &format!("/sessions/{id}/resolve-marker"), &http_uds::ResolveMarkerRequest {
                name: name.to_string(),
            })?;
        Ok(response.offset)
    }

    pub fn resolve_next_marker_after(&self, id: &str, after: u64) -> Result<Option<u64>, String> {
        if !self.layout.session_dir(id).exists() {
            return Err(format!("missing session {id}"));
        }
        let response: http_uds::ResolveNextMarkerResponse =
            self.http_json(id, Method::POST, &format!("/sessions/{id}/resolve-next-marker"), &http_uds::ResolveNextMarkerRequest {
                after,
            })?;
        Ok(response.offset)
    }

    pub fn record(&self, id: &str, enable: bool) -> Result<(), String> {
        if !self.layout.session_dir(id).exists() {
            return Err(format!("missing session {id}"));
        }
        self.http_no_content(id, Method::POST, &format!("/sessions/{id}/record"), &http_uds::RecordRequest { enable })
    }

    pub fn wait(
        &self,
        id: &str,
        conditions: Vec<crate::protocol::WaitCondition>,
        timeout_ms: u64,
    ) -> Result<(crate::protocol::WaitStatus, u64), String> {
        if !self.layout.session_dir(id).exists() {
            return Err(format!("missing session {id}"));
        }
        let conditions = conditions.into_iter().map(wait_condition_to_http).collect();
        let response: http_uds::WaitResultResponse = self.http_json_with_read_timeout(
            id,
            Method::POST,
            &format!("/sessions/{id}/wait"),
            &http_uds::WaitRequest { conditions, timeout_ms },
            Duration::from_millis(timeout_ms.saturating_add(5000)),
        )?;
        Ok((wait_status_from_http(response.status), response.elapsed_ms))
    }

    pub fn expect(&self, id: &str, text: &str, since_offset: u64, timeout_ms: u64) -> Result<(crate::protocol::WaitStatus, u64), String> {
        if !self.layout.session_dir(id).exists() {
            return Err(format!("missing session {id}"));
        }
        let response: http_uds::WaitResultResponse = self.http_json_with_read_timeout(
            id,
            Method::POST,
            &format!("/sessions/{id}/expect"),
            &http_uds::ExpectRequest { text: text.to_string(), since_offset, timeout_ms },
            Duration::from_millis(timeout_ms.saturating_add(5000)),
        )?;
        Ok((wait_status_from_http(response.status), response.elapsed_ms))
    }

    pub fn serve(&self) -> Result<(), String> {
        run_session_daemon(self.layout.root(), self.layout.daemon_name())
    }

    fn http_json<T: serde::Serialize, R: DeserializeOwned>(&self, id: &str, method: Method, path: &str, body: &T) -> Result<R, String> {
        let response = self.http_request(id, method.clone(), path, body)?;
        if response.status != StatusCode::OK {
            return Err(http_error_message(response));
        }
        serde_json::from_slice(&response.body).map_err(|err| format!("parse HTTP response: {err}"))
    }

    fn http_json_with_read_timeout<T: serde::Serialize, R: DeserializeOwned>(
        &self,
        id: &str,
        method: Method,
        path: &str,
        body: &T,
        read_timeout: Duration,
    ) -> Result<R, String> {
        let response = self.http_request_with_read_timeout(id, method, path, body, Some(read_timeout))?;
        if response.status != StatusCode::OK {
            return Err(http_error_message(response));
        }
        serde_json::from_slice(&response.body).map_err(|err| format!("parse HTTP response: {err}"))
    }

    fn http_no_content<T: serde::Serialize>(&self, id: &str, method: Method, path: &str, body: &T) -> Result<(), String> {
        let response = self.http_request(id, method, path, body)?;
        if response.status == StatusCode::NO_CONTENT {
            Ok(())
        } else {
            Err(http_error_message(response))
        }
    }

    fn http_request<T: serde::Serialize>(&self, id: &str, method: Method, path: &str, body: &T) -> Result<http_uds::HttpResponse, String> {
        self.http_request_with_read_timeout(id, method, path, body, None)
    }

    fn http_json_daemon<T: serde::Serialize, R: DeserializeOwned>(&self, method: Method, path: &str, body: &T) -> Result<R, String> {
        let response = self.http_request_with_read_timeout("", method, path, body, None)?;
        if response.status != StatusCode::OK {
            return Err(http_error_message(response));
        }
        serde_json::from_slice(&response.body).map_err(|err| format!("parse HTTP response: {err}"))
    }

    fn http_request_with_read_timeout<T: serde::Serialize>(
        &self,
        _id: &str,
        method: Method,
        path: &str,
        body: &T,
        read_timeout: Option<Duration>,
    ) -> Result<http_uds::HttpResponse, String> {
        let socket_path = self.layout.socket_path();
        let mut stream = connect_session_socket(&socket_path)?;
        if let Some(timeout) = read_timeout {
            set_stream_read_timeout(&stream, Some(timeout))?;
        }
        let body = if method == Method::GET || method == Method::DELETE {
            Vec::new()
        } else {
            serde_json::to_vec(body).map_err(|err| format!("serialize HTTP request: {err}"))?
        };
        http_uds::write_request(&mut stream, method, path, &body).map_err(|err| format!("write HTTP request: {err}"))?;
        http_uds::read_response(&mut stream).map_err(|err| format!("read HTTP response: {err}"))
    }
}

fn session_matches_selectors(session: &SessionInfo, selectors: &[String]) -> bool {
    selectors.iter().all(|selector| session.tags.contains(selector))
}

fn session_socket_is_stale(socket_path: &Path) -> bool {
    try_connect_session_stream(socket_path).is_err()
}

fn daemon_control_is_unavailable(layout: &RuntimeLayout) -> bool {
    if !is_session_daemon_alive(layout.root(), layout.daemon_name()) {
        return true;
    }
    if !layout.socket_path().exists() {
        return !layout.daemon_pid_path().exists();
    }
    session_socket_is_stale(&layout.socket_path())
}

fn sweep_dead_daemon_sessions(layout: &RuntimeLayout, err: String) -> Result<Vec<SessionInfo>, String> {
    let mut sessions = Vec::new();
    let entries = std::fs::read_dir(layout.sessions_dir()).map_err(|read_err| {
        format!("read daemon sessions directory {} after inspect failure ({err}): {read_err}", layout.sessions_dir().display())
    })?;
    for entry in entries {
        let entry = entry.map_err(|read_err| format!("read daemon session entry: {read_err}"))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(id) = path.file_name().and_then(|name| name.to_str()).map(str::to_string) else {
            continue;
        };
        if !crate::recreate::session_is_recreatable(&path) {
            layout.remove_session(&id)?;
            continue;
        }
        let _ = std::fs::remove_file(layout.foreground_path(&id));
        sessions.push(SessionInfo {
            id,
            vt_engine: crate::vt::default_vt_engine_kind(),
            vt_engine_status: String::new(),
            functional_vt_available: false,
            cwd: None,
            cmd: None,
            tags: Vec::new(),
            status: SessionStatus::Detached,
            screen_activity: crate::protocol::ScreenActivity::Stable,
            stable_since: None,
            last_output_at: None,
            controller: None,
            error: Some(err.clone()),
        });
    }
    remove_stale_daemon_file(layout.socket_path(), "socket")?;
    remove_stale_daemon_file(layout.daemon_pid_path(), "pid")?;
    Ok(sessions)
}

fn remove_stale_daemon_file(path: std::path::PathBuf, label: &str) -> Result<(), String> {
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!("remove stale daemon {label} {}: {err}", path.display())),
    }
}

/// Resolve start and end bounds into byte offsets against a cast file without
/// going through the daemon. Marker-based bounds are rejected; the CLI is
/// expected to prevent these combinations via clap's `conflicts_with = "path"`
/// on the marker flags.
///
/// Mirrors [`SessionService::resolve_slice_range`] for path-based callers.
pub fn resolve_range_for_path(
    cast_path: &std::path::Path,
    start: StartBound,
    end: EndBound,
) -> Result<(u64, u64, Option<FallbackReason>), String> {
    let start_offset = match start {
        StartBound::Offset(o) => o,
        StartBound::Marker(_) => {
            return Err("path-based replay does not support marker start bounds".to_string());
        }
    };

    let file_size = std::fs::metadata(cast_path).map_err(|e| format!("stat cast file: {e}"))?.len();

    let (end_offset, end_status) = match end {
        EndBound::EndOfRecording => (file_size, None),
        EndBound::Offset(o) => {
            if o < start_offset {
                return Err(format!("end offset {o} precedes start offset {start_offset}"));
            }
            (o, None)
        }
        EndBound::Marker(_) | EndBound::NextMarker => {
            return Err("path-based replay does not support marker end bounds".to_string());
        }
        EndBound::IdleGap(duration) => match crate::cast_reader::find_idle_gap_after(cast_path, start_offset, duration)? {
            Some(o) => (o, None),
            None => (file_size, Some(FallbackReason::NoIdleGap(duration))),
        },
    };

    Ok((start_offset, end_offset, end_status))
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
        tags: result.session.tags,
        status,
        screen_activity: result.screen_activity,
        stable_since: result.stable_since,
        last_output_at: result.last_output_at,
        controller: controller_identity(&result.attachments),
        error: None,
    }
}

fn has_controller_attachment(attachments: &[crate::protocol::AttachmentInspect]) -> bool {
    attachments.iter().any(|attachment| attachment.role == "controller")
}

fn controller_identity(attachments: &[crate::protocol::AttachmentInspect]) -> Option<AttachmentIdentity> {
    attachments.iter().find(|attachment| attachment.role == "controller").map(|attachment| attachment.identity.clone())
}

fn signal_target_to_http(target: crate::protocol::SignalTarget) -> http_uds::SignalTargetRequest {
    match target {
        crate::protocol::SignalTarget::Foreground => http_uds::SignalTargetRequest::Foreground,
        crate::protocol::SignalTarget::Leader => http_uds::SignalTargetRequest::Leader,
        crate::protocol::SignalTarget::Tree => http_uds::SignalTargetRequest::Tree,
    }
}

fn wait_condition_to_http(condition: crate::protocol::WaitCondition) -> http_uds::WaitConditionRequest {
    match condition {
        crate::protocol::WaitCondition::OutputIdle { quiet_ms } => http_uds::WaitConditionRequest::OutputIdle { quiet_ms },
        crate::protocol::WaitCondition::TextMatch { text } => http_uds::WaitConditionRequest::TextMatch { text },
        crate::protocol::WaitCondition::ScreenStable { stable_ms } => http_uds::WaitConditionRequest::ScreenStable { stable_ms },
    }
}

fn wait_status_from_http(status: http_uds::WaitStatusResponse) -> crate::protocol::WaitStatus {
    match status {
        http_uds::WaitStatusResponse::Ready => crate::protocol::WaitStatus::Ready,
        http_uds::WaitStatusResponse::Timeout => crate::protocol::WaitStatus::Timeout,
        http_uds::WaitStatusResponse::SessionGone => crate::protocol::WaitStatus::SessionGone,
    }
}

fn http_error_message(response: http_uds::HttpResponse) -> String {
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&response.body) {
        if let Some(message) = value.get("error").and_then(|value| value.as_str()) {
            return message.to_string();
        }
    }
    format!("HTTP request returned {}", response.status)
}

fn connect_session_socket(socket_path: &Path) -> Result<SessionStream, String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match try_connect_session_stream(socket_path) {
            Ok(stream) => return Ok(stream),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound && Instant::now() < deadline => {
                // Socket not yet created — daemon may still be starting up.
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => return Err(format!("connect {}: {err}", socket_path.display())),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        fs,
        os::unix::net::UnixListener,
        path::Path,
        process::Command,
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };

    use super::SessionService;
    use crate::{
        http_uds::{self, read_http_request_for_test},
        protocol::{WaitCondition, WaitStatus},
        runtime::RuntimeLayout,
        session::{daemon_pid_path, session_socket_path},
    };

    fn create_test_session_dir(root: &Path, id: &str) -> std::path::PathBuf {
        let layout = RuntimeLayout::new(root.to_path_buf());
        layout.ensure_daemon_dirs().expect("create daemon dirs");
        let session_dir = layout.session_dir(id);
        fs::create_dir_all(&session_dir).expect("create session dir");
        session_dir
    }

    #[test]
    fn kill_does_not_signal_unrelated_process_from_stale_pid_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));
        let _session_dir = create_test_session_dir(temp.path(), "alpha");

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
    fn send_keys_posts_http_request_to_session_socket() {
        let temp = tempfile::tempdir().expect("tempdir");
        let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));
        let _session_dir = create_test_session_dir(temp.path(), "alpha");

        let socket_path = session_socket_path(temp.path(), "alpha");
        let listener = UnixListener::bind(&socket_path).expect("bind socket");
        let (tx, rx) = mpsc::channel();
        let reader = thread::spawn(move || {
            use std::io::Write;

            let (mut stream, _) = listener.accept().expect("accept connection");
            let request = read_http_request_for_test(&mut stream);
            tx.send(request).expect("send request");
            stream.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").expect("write response");
        });

        service.send_keys("alpha", b"hello\r").expect("send keys");
        let request = rx.recv_timeout(Duration::from_secs(1)).expect("receive request");

        reader.join().expect("join reader");
        assert!(request.starts_with("POST /sessions/alpha/keys HTTP/1.1\r\n"), "{request}");
        assert!(request.ends_with(r#"{"bytes":[104,101,108,108,111,13]}"#), "{request}");
    }

    #[test]
    fn send_input_posts_http_request_to_session_socket() {
        let temp = tempfile::tempdir().expect("tempdir");
        let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));
        let _session_dir = create_test_session_dir(temp.path(), "alpha");

        let socket_path = session_socket_path(temp.path(), "alpha");
        let listener = UnixListener::bind(&socket_path).expect("bind socket");
        let (tx, rx) = mpsc::channel();
        let reader = thread::spawn(move || {
            use std::io::Write;

            let (mut stream, _) = listener.accept().expect("accept connection");
            let request = read_http_request_for_test(&mut stream);
            tx.send(request).expect("send request");
            stream.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").expect("write response");
        });

        service.send_input("alpha", &http_uds::InputRequest::Paste { text: "hello".to_string() }).expect("send input");
        let request = rx.recv_timeout(Duration::from_secs(1)).expect("receive request");

        reader.join().expect("join reader");
        assert!(request.starts_with("POST /sessions/alpha/input HTTP/1.1\r\n"), "{request}");
        assert!(request.ends_with(r#"{"kind":"paste","text":"hello"}"#), "{request}");
    }

    #[test]
    fn send_paste_with_mark_posts_http_request_to_session_socket() {
        let temp = tempfile::tempdir().expect("tempdir");
        let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));
        let _session_dir = create_test_session_dir(temp.path(), "alpha");

        let socket_path = session_socket_path(temp.path(), "alpha");
        let listener = UnixListener::bind(&socket_path).expect("bind socket");
        let (tx, rx) = mpsc::channel();
        let reader = thread::spawn(move || {
            use std::io::Write;

            let (mut stream, _) = listener.accept().expect("accept connection");
            let request = read_http_request_for_test(&mut stream);
            tx.send(request).expect("send request");
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 13\r\nConnection: close\r\n\r\n{\"offset\":42}",
                )
                .expect("write response");
        });

        let offset = service.send_paste_with_mark("alpha", "hello", "m1").expect("send paste with mark");
        let request = rx.recv_timeout(Duration::from_secs(1)).expect("receive request");

        reader.join().expect("join reader");
        assert_eq!(offset, 42);
        assert!(request.starts_with("POST /sessions/alpha/paste-with-mark HTTP/1.1\r\n"), "{request}");
        assert!(request.ends_with(r#"{"text":"hello","marker_name":"m1"}"#), "{request}");
    }

    #[test]
    fn kill_deletes_session_over_http_when_socket_is_available() {
        let temp = tempfile::tempdir().expect("tempdir");
        let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));
        let session_dir = create_test_session_dir(temp.path(), "alpha");

        let socket_path = session_socket_path(temp.path(), "alpha");
        let listener = UnixListener::bind(&socket_path).expect("bind socket");
        let (tx, rx) = mpsc::channel();
        let reader = thread::spawn(move || {
            use std::io::Write;

            let (mut stream, _) = listener.accept().expect("accept connection");
            let request = read_http_request_for_test(&mut stream);
            tx.send(request).expect("send request");
            stream.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").expect("write response");
        });

        service.kill("alpha").expect("kill session");
        let request = rx.recv_timeout(Duration::from_secs(1)).expect("receive request");

        reader.join().expect("join reader");
        assert!(request.starts_with("DELETE /sessions/alpha HTTP/1.1\r\n"), "{request}");
        assert!(!session_dir.exists(), "kill should remove the local session directory");
    }

    #[test]
    fn kill_purge_preserved_recording_without_socket_returns_promptly() {
        let temp = tempfile::tempdir().expect("tempdir");
        let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));
        let session_dir = create_test_session_dir(temp.path(), "alpha");
        fs::write(session_dir.join(crate::recording::CAST_FILE_NAME), b"{\"version\":3}\n").expect("write cast");

        let started = Instant::now();
        service.kill_with_purge("alpha", true).expect("purge preserved recording");

        assert!(started.elapsed() < Duration::from_secs(1), "purge should not wait for a missing socket");
        assert!(!session_dir.exists(), "purge should remove the preserved recording directory");
    }

    #[test]
    fn list_sweeps_dead_daemon_and_preserves_only_recreatable_sessions() {
        let temp = tempfile::tempdir().expect("tempdir");
        let layout = RuntimeLayout::new(temp.path().to_path_buf());
        let kept_dir = create_test_session_dir(temp.path(), "kept");
        let discarded_dir = create_test_session_dir(temp.path(), "discarded");
        fs::write(kept_dir.join(crate::recording::CAST_FILE_NAME), b"{\"version\":3}\n").expect("write cast");
        fs::write(layout.foreground_path("kept"), b"12345").expect("write foreground marker");
        fs::write(layout.daemon_pid_path(), "999999999").expect("write stale pid");

        let service = SessionService::new(layout.clone());
        let sessions = service.list_all_with_selectors(&[]).expect("list all");

        assert_eq!(sessions.iter().map(|session| session.id.as_str()).collect::<Vec<_>>(), vec!["kept"]);
        assert_eq!(sessions[0].status, crate::protocol::SessionStatus::Detached);
        assert!(sessions[0].error.as_deref().unwrap_or_default().contains("daemon control socket"));
        assert!(kept_dir.exists(), "recreatable session should be preserved");
        assert!(kept_dir.join(crate::recording::CAST_FILE_NAME).exists(), "recording should remain");
        assert!(!layout.foreground_path("kept").exists(), "volatile foreground marker should be removed");
        assert!(!discarded_dir.exists(), "non-recreatable session should be removed");
        assert!(!layout.daemon_pid_path().exists(), "stale daemon pid should be removed");
    }

    #[test]
    fn list_http_error_from_reachable_daemon_does_not_sweep_sessions() {
        let temp = tempfile::tempdir().expect("tempdir");
        let layout = RuntimeLayout::new(temp.path().to_path_buf());
        let kept_dir = create_test_session_dir(temp.path(), "kept");
        let discarded_dir = create_test_session_dir(temp.path(), "discarded");
        fs::write(kept_dir.join(crate::recording::CAST_FILE_NAME), b"{\"version\":3}\n").expect("write cast");
        fs::write(layout.daemon_pid_path(), std::process::id().to_string()).expect("write pid");

        let listener = UnixListener::bind(layout.socket_path()).expect("bind socket");
        let reader = thread::spawn(move || {
            for index in 0..3 {
                use std::io::Write;

                let (mut stream, _) = listener.accept().expect("accept connection");
                if index == 1 {
                    let _request = read_http_request_for_test(&mut stream);
                    stream
                        .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 4\r\nConnection: close\r\n\r\noops")
                        .expect("write error response");
                }
            }
        });

        let service = SessionService::new(layout.clone());
        let err = service.list_all_with_selectors(&[]).expect_err("live daemon HTTP error should propagate");

        reader.join().expect("join reader");
        assert!(err.contains("500 Internal Server Error"), "{err}");
        assert!(kept_dir.exists(), "recreatable session should remain");
        assert!(discarded_dir.exists(), "non-recreatable live session should not be swept");
        assert!(layout.socket_path().exists(), "daemon socket should remain reachable");
        assert!(layout.daemon_pid_path().exists(), "daemon pid should remain");
    }

    #[test]
    fn detach_posts_http_request_to_session_socket() {
        let temp = tempfile::tempdir().expect("tempdir");
        let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));
        let _session_dir = create_test_session_dir(temp.path(), "alpha");

        let socket_path = session_socket_path(temp.path(), "alpha");
        let listener = UnixListener::bind(&socket_path).expect("bind socket");
        let (tx, rx) = mpsc::channel();
        let reader = thread::spawn(move || {
            use std::io::Write;

            let (mut stream, _) = listener.accept().expect("accept connection");
            let request = read_http_request_for_test(&mut stream);
            tx.send(request).expect("send request");
            stream.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").expect("write response");
        });

        service.detach("alpha").expect("detach");
        let request = rx.recv_timeout(Duration::from_secs(1)).expect("receive request");

        reader.join().expect("join reader");
        assert!(request.starts_with("POST /sessions/alpha/detach HTTP/1.1\r\n"), "{request}");
    }

    #[test]
    fn wait_posts_http_request_to_session_socket() {
        let temp = tempfile::tempdir().expect("tempdir");
        let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));
        let _session_dir = create_test_session_dir(temp.path(), "alpha");

        let socket_path = session_socket_path(temp.path(), "alpha");
        let listener = UnixListener::bind(&socket_path).expect("bind socket");
        let (tx, rx) = mpsc::channel();
        let reader = thread::spawn(move || {
            use std::io::Write;

            let (mut stream, _) = listener.accept().expect("accept connection");
            let request = read_http_request_for_test(&mut stream);
            tx.send(request).expect("send request");
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 34\r\nConnection: close\r\n\r\n{\"status\":\"ready\",\"elapsed_ms\":42}",
                )
                .expect("write response");
        });

        let result = service
            .wait("alpha", vec![WaitCondition::OutputIdle { quiet_ms: 250 }, WaitCondition::ScreenStable { stable_ms: 750 }], 5000)
            .expect("wait");
        let request = rx.recv_timeout(Duration::from_secs(1)).expect("receive request");

        reader.join().expect("join reader");
        assert_eq!(result, (WaitStatus::Ready, 42));
        assert!(request.starts_with("POST /sessions/alpha/wait HTTP/1.1\r\n"), "{request}");
        assert!(
            request.ends_with(
                r#"{"conditions":[{"kind":"output_idle","quiet_ms":250},{"kind":"screen_stable","stable_ms":750}],"timeout_ms":5000}"#
            ),
            "{request}"
        );
    }

    #[test]
    fn expect_posts_http_request_to_session_socket() {
        let temp = tempfile::tempdir().expect("tempdir");
        let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));
        let _session_dir = create_test_session_dir(temp.path(), "alpha");

        let socket_path = session_socket_path(temp.path(), "alpha");
        let listener = UnixListener::bind(&socket_path).expect("bind socket");
        let (tx, rx) = mpsc::channel();
        let reader = thread::spawn(move || {
            use std::io::Write;

            let (mut stream, _) = listener.accept().expect("accept connection");
            let request = read_http_request_for_test(&mut stream);
            tx.send(request).expect("send request");
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 37\r\nConnection: close\r\n\r\n{\"status\":\"timeout\",\"elapsed_ms\":500}",
                )
                .expect("write response");
        });

        let result = service.expect("alpha", "DONE", 123, 500).expect("expect");
        let request = rx.recv_timeout(Duration::from_secs(1)).expect("receive request");

        reader.join().expect("join reader");
        assert_eq!(result, (WaitStatus::Timeout, 500));
        assert!(request.starts_with("POST /sessions/alpha/expect HTTP/1.1\r\n"), "{request}");
        assert!(request.ends_with(r#"{"text":"DONE","since_offset":123,"timeout_ms":500}"#), "{request}");
    }
}
