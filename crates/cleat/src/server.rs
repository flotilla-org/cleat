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
    runtime::{discoverable_runtime_roots, validate_daemon_name, validate_runtime_name, DaemonCoordinates, RuntimeLayout, TerminalSize},
    session::{
        attach_foreground, attach_packet_foreground, ensure_session_started, run_session_daemon, start_session_in_running_daemon,
        watch_foreground, ForegroundAttach, SessionStartOptions,
    },
    vt::VtEngineKind,
};

// A total response deadline, rather than an idle timeout that a trickling peer
// could extend indefinitely. Windows pipe reads honor the same timeout contract.
struct DaemonResponseReader<'a> {
    stream: &'a mut SessionStream,
    deadline: Instant,
}

impl std::io::Read for DaemonResponseReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::TimedOut, "daemon response deadline exceeded"))?;
        set_stream_read_timeout(self.stream, Some(remaining)).map_err(std::io::Error::other)?;
        self.stream.read(buffer)
    }
}

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
    /// The hosting epoch this holder believes its session is at. Mutating
    /// requests state it, and the daemon refuses them when it is stale.
    hosting_epoch: Option<u64>,
}

/// Options for [`SessionService::transfer`].
#[derive(Debug, Clone, Default)]
pub struct TransferOptions {
    /// Drop attached clients whose protocol the target refuses, instead of
    /// refusing the transfer.
    pub drop_incompatible: bool,
    /// Bound on everything before the target is ready; defaults to 10 s.
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DrainGeneration {
    pub name: String,
    pub generation: Option<u64>,
    pub build: Option<crate::build_info::BuildInfo>,
    pub session_count: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DrainReport {
    pub changed: bool,
    pub installed: crate::build_info::BuildInfo,
    pub old: DrainGeneration,
    pub current: DrainGeneration,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonInstance {
    name: String,
    pid: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttachOptions {
    /// Enable recording before granting the attachment; false preserves its current state.
    pub record: bool,
    pub identity: AttachmentIdentity,
    pub strict: bool,
    pub take: bool,
}

impl DaemonInstance {
    pub fn name(&self) -> &str {
        self.name.split('@').next().unwrap_or(&self.name)
    }

    pub fn address(&self) -> &str {
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
        Self { layout, hosting_epoch: None }
    }

    /// State `epoch` as this holder's hosting epoch on every request.
    pub fn with_hosting_epoch(mut self, epoch: Option<u64>) -> Self {
        self.hosting_epoch = epoch;
        self
    }

    pub fn discover() -> Result<Self, String> {
        Ok(Self::new(RuntimeLayout::discover()?))
    }

    pub fn with_daemon(&self, daemon_name: String) -> Result<Self, String> {
        Ok(Self::new(self.layout.clone().with_daemon(daemon_name)?).with_hosting_epoch(self.hosting_epoch))
    }

    /// Resolve an id through the logical alias. Prefer live claims; retain dead
    /// recordings as recreatable husks when no live generation claims the id.
    pub fn for_session(&self, id: &str) -> Result<Self, String> {
        validate_runtime_name(id)?;
        if self.layout.daemon_name().contains('@') {
            return Ok(self.clone());
        }
        let names: Vec<_> = self
            .layout
            .generation_names()?
            .into_iter()
            .filter(|name| self.with_daemon(name.clone()).is_ok_and(|candidate| candidate.session_dir(id).is_dir()))
            .collect();
        // A single candidate needs no connection probe (and existing-only attach
        // must leave its first HTTP response untouched).
        if names.len() == 1 {
            return self.with_daemon(names[0].clone());
        }
        if names.is_empty() {
            // Missed in the named daemon: a transferred session lives on in
            // another daemon on this root. Only a live owner is followed.
            return match self.daemon_owning_session(id) {
                Ok(owner) => self.with_daemon(owner.address().to_string()),
                Err(_) => Ok(self.clone()),
            };
        }
        let mut live = Vec::new();
        let mut husks = Vec::new();
        for name in names {
            let candidate = self.with_daemon(name.clone())?;
            if daemon_control_is_unavailable(&candidate.layout) {
                husks.push(name);
                continue;
            }
            let directory: http_uds::SessionListResponse = candidate.http_json_daemon(Method::GET, "/sessions", &())?;
            if directory.sessions.iter().any(|session| session.session.id == id) {
                live.push(name);
            } else {
                husks.push(name);
            }
        }
        let candidates = if live.is_empty() { husks } else { live };
        match candidates.as_slice() {
            [] => Ok(self.clone()),
            [name] => self.with_daemon(name.clone()),
            _ => Err(format!(
                "session {id} exists in multiple daemons ({}); use --server to select the target daemon instead",
                candidates.join(", ")
            )),
        }
    }

    /// Recreate a dead generation's retained session on the current generation.
    /// Live sessions still resolve to their existing host; explicit addresses
    /// remain pinned and cannot silently change generations.
    pub fn for_recreation(&self, id: &str) -> Result<Self, String> {
        let source = self.for_session(id)?;
        if self.layout.daemon_name().contains('@') || !source.session_dir(id).is_dir() || !daemon_control_is_unavailable(&source.layout) {
            return Ok(source);
        }
        let target = Self::new(self.layout.prepare_generation()?);
        let source_dir = source.session_dir(id);
        let target_dir = target.session_dir(id);
        if source_dir != target_dir && source_dir.exists() {
            if target_dir.exists() {
                return Err(format!("session {id} exists in multiple daemons; use --server to select the target daemon instead"));
            }
            std::fs::rename(&source_dir, &target_dir).map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    format!("session {id} retained state changed during recreation; retry the attach")
                } else {
                    format!("adopt retained session {id} for recreation: {e}")
                }
            })?;
        }
        Ok(target)
    }

    /// Move a live session to the daemon `to` names (Transfer). The source
    /// daemon runs the exchange; the session keeps running throughout.
    #[cfg(unix)]
    pub fn transfer(&self, id: &str, to: &SessionService, options: TransferOptions) -> Result<crate::protocol::TransferResult, String> {
        if !self.layout.session_dir(id).exists() {
            return Err(format!("missing session {id}"));
        }
        crate::session::ensure_daemon_started(&to.layout)?;
        let target = to.layout.resolved()?;
        let runtime_root = std::path::absolute(target.root()).map_err(|err| format!("resolve target runtime root: {err}"))?;
        let timeout = options.timeout.unwrap_or(Duration::from_secs(10));
        let request = http_uds::SessionTransferRequest {
            runtime_root: runtime_root.display().to_string(),
            daemon: target.daemon_name().to_string(),
            drop_incompatible: options.drop_incompatible,
            timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
        };
        self.http_json_with_read_timeout(
            id,
            Method::POST,
            &format!("/sessions/{id}/transfer"),
            &request,
            timeout.saturating_add(Duration::from_secs(15)),
        )
    }

    pub fn layout_root(&self) -> &std::path::Path {
        self.layout.root()
    }

    /// Query only the running daemon, without starting one or negotiating packets.
    pub fn daemon_build_info(&self) -> Result<Option<crate::build_info::BuildInfo>, String> {
        Ok(self.daemon_build_status()?.build)
    }

    pub(crate) fn daemon_build_status(&self) -> Result<crate::build_info::DaemonBuildStatus, String> {
        self.daemon_status_at("/")
    }

    fn daemon_status_at(&self, endpoint: &str) -> Result<crate::build_info::DaemonBuildStatus, String> {
        let response = self.daemon_request(Method::GET, endpoint)?;
        if response.status != StatusCode::OK {
            return Err(http_error_message(response));
        }
        let status: crate::build_info::DaemonBuildStatus =
            serde_json::from_slice(&response.body).map_err(|err| format!("parse daemon build status: {err}"))?;
        Ok(status)
    }

    /// Roll the logical alias to this binary without moving any live sessions.
    pub fn drain(&self) -> Result<DrainReport, String> {
        self.drain_using(
            |layout| crate::platform::daemon::spawn_daemon_process(layout.root(), layout.daemon_name()),
            Duration::from_secs(10),
        )
    }

    fn drain_using(
        &self,
        start: impl FnOnce(&RuntimeLayout) -> Result<(), String>,
        health_deadline: Duration,
    ) -> Result<DrainReport, String> {
        if self.layout.daemon_name().contains('@') {
            return Err("server drain requires a logical name; use --server without @generation".into());
        }
        let _lock = self.layout.lock_generations()?;
        let old_service = Self::new(self.layout.resolved()?);
        let old_status = old_service.daemon_build_status()?;
        let installed = crate::build_info::BuildInfo::current();
        if installed.git_sha.is_none() {
            return Err("installed client has no git SHA; rebuild with revision metadata before draining".into());
        }
        let old = DrainGeneration {
            name: old_service.layout.daemon_name().to_string(),
            generation: old_service.layout.generation(),
            build: old_status.build,
            session_count: old_status.session_count,
        };
        if old
            .build
            .as_ref()
            .is_some_and(|build| build.git_sha == installed.git_sha && build.protocol_version == installed.protocol_version)
        {
            return Ok(DrainReport { changed: false, installed, current: old.clone(), old, warning: None });
        }
        // Query the count separately: pre-drain daemons don't include it in status.
        let response = old_service.daemon_request(Method::GET, "/sessions")?;
        if response.status != StatusCode::OK {
            return Err(http_error_message(response));
        }
        #[derive(serde::Deserialize)]
        struct SessionCount {
            sessions: Vec<serde::de::IgnoredAny>,
        }
        let sessions: SessionCount = serde_json::from_slice(&response.body).map_err(|e| format!("read old sessions: {e}"))?;
        let mut old = DrainGeneration { session_count: sessions.sessions.len(), ..old };
        let successor = self.layout.allocate_generation()?;
        let new_service = Self::new(successor.clone());
        start(&successor)?;
        let deadline = Instant::now() + health_deadline;
        let status = loop {
            match new_service.daemon_status_at("/healthz") {
                Ok(status) => break status,
                Err(err) if Instant::now() >= deadline => {
                    return Err(format!("successor {} failed health check; alias unchanged: {err}", successor.daemon_name()));
                }
                Err(_) => thread::sleep(Duration::from_millis(20)),
            }
        };
        if status
            .build
            .as_ref()
            .is_none_or(|build| build.git_sha != installed.git_sha || build.protocol_version != installed.protocol_version)
        {
            return Err("successor build does not match installed client; alias unchanged".into());
        }
        self.layout.set_current_generation(successor.generation().ok_or("missing successor generation")?)?;
        let warning = match old_service.daemon_request(Method::POST, "/drain") {
            Ok(response) if response.status == StatusCode::OK => {
                let status: crate::build_info::DaemonBuildStatus =
                    serde_json::from_slice(&response.body).map_err(|e| format!("alias moved, but invalid drain response: {e}"))?;
                old.session_count = status.session_count;
                None
            }
            Ok(response) if response.status == StatusCode::NOT_FOUND || response.status == StatusCode::METHOD_NOT_ALLOWED => {
                Some(format!("{} cannot be told to drain; left serving. New sessions use {}", old.name, successor.daemon_name()))
            }
            Ok(response) => return Err(format!("alias moved, but old daemon drain failed: {}", http_error_message(response))),
            Err(err) => return Err(format!("alias moved, but old daemon drain failed: {err}")),
        };
        let current = DrainGeneration {
            name: successor.daemon_name().to_string(),
            generation: successor.generation(),
            build: status.build,
            session_count: status.session_count,
        };
        Ok(DrainReport { changed: true, installed, old, current, warning })
    }

    fn daemon_request(&self, method: Method, path: &str) -> Result<http_uds::HttpResponse, String> {
        let mut stream = try_connect_session_stream(&self.layout.socket_path()).map_err(|err| format!("connect daemon: {err}"))?;
        set_stream_read_timeout(&stream, Some(Duration::from_secs(2)))?;
        crate::platform::ipc::set_stream_write_timeout(&stream, Some(Duration::from_secs(2)))?;
        http_uds::write_request(&mut stream, method, path, &[]).map_err(|err| format!("write daemon request: {err}"))?;
        let mut reader = DaemonResponseReader { stream: &mut stream, deadline: Instant::now() + Duration::from_secs(2) };
        http_uds::read_response(&mut reader).map_err(|err| format!("read daemon response: {err}"))
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
                if path.is_symlink() || !path.is_dir() || !path.join("sessions").is_dir() {
                    continue;
                }
                let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                if validate_daemon_name(name).is_err() {
                    continue;
                }
                let address = if name.contains('@') { name.to_string() } else { format!("{name}@legacy") };
                let layout = RuntimeLayout::new(root.clone()).with_daemon(address.clone()).expect("validated daemon");
                let service = Self::new(layout.clone());
                let status = service.daemon_build_status();
                daemons.push(DaemonCoordinates {
                    name: address,
                    runtime_root: root.clone(),
                    generation: layout.generation(),
                    alive: status.is_ok(),
                    drain_state: status.as_ref().map(|s| s.drain_state.clone()).unwrap_or_else(|_| {
                        std::fs::read_to_string(layout.daemon_dir().join("drain-state")).unwrap_or_else(|_| "serving".into())
                    }),
                    build: status.ok().and_then(|s| s.build).or_else(|| {
                        std::fs::read(layout.daemon_dir().join("build.json")).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok())
                    }),
                });
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
        if self.layout.logical_name() != daemon.name() {
            return Err(format!("daemon instance {} does not match target {}", daemon.name, self.layout.daemon_name()));
        }
        let layout = self.layout.resolved()?;
        if layout.daemon_name() != daemon.address() {
            return Err("source daemon instance changed".into());
        }
        let session = start_session_in_running_daemon(&layout, daemon.pid, name, vt_engine, cwd, cmd, options)?;
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
            conpty: None,
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
            if daemon_control_is_unavailable(&daemon_service.layout) {
                continue;
            }
            let pid_before = daemon_service.registered_daemon_pid();
            match daemon_service.http_json::<_, crate::protocol::InspectResult>(id, Method::GET, &format!("/sessions/{id}"), &()) {
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
            let mut daemon_service = self.with_daemon(daemon_name)?;
            if daemon_service.layout.daemon_name().ends_with("@legacy") {
                let logical = self.with_daemon(daemon_service.layout.logical_name().to_string())?;
                // Preserve pre-generation adoption on list, but never follow the
                // logical alias when enumerating a retired legacy host.
                if logical.layout.generation().is_none() {
                    daemon_service = logical;
                }
            }
            sessions.extend(daemon_service.list_one_daemon_with_selectors(selectors)?);
        }
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(sessions)
    }

    fn daemon_names(&self, scope: ListScope) -> Result<Vec<String>, String> {
        match scope {
            ListScope::Current => self.layout.generation_names(),
            ListScope::All => {
                let entries = std::fs::read_dir(self.layout.root())
                    .map_err(|err| format!("read runtime root {}: {err}", self.layout.root().display()))?;
                let mut names = Vec::new();
                for entry in entries {
                    let entry = entry.map_err(|err| format!("read runtime entry: {err}"))?;
                    let path = entry.path();
                    if path.is_symlink() || !path.is_dir() || !path.join("sessions").is_dir() {
                        continue;
                    }
                    if let Some(name) = path.file_name().and_then(|n| n.to_str()).filter(|name| validate_daemon_name(name).is_ok()) {
                        names.push(if name.contains('@') { name.to_string() } else { format!("{name}@legacy") });
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
        if self.layout.daemon_name().contains('@') && !is_session_daemon_alive(self.layout.root(), self.layout.daemon_name()) {
            let mut sessions = sweep_dead_daemon_sessions(&self.layout, "daemon generation is dead; session is recreatable".into())?;
            sessions.retain(|session| session_matches_selectors(session, selectors));
            return Ok(sessions);
        }
        crate::session::ensure_daemon_started(&self.layout)?;

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
                        conpty: result.session.conpty,
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
            // Shutdown polling must never auto-start the daemon it is waiting on.
            if !self.layout.session_dir(id).exists()
                || self.http_json::<_, crate::protocol::InspectResult>(id, Method::GET, &format!("/sessions/{id}"), &()).is_err()
            {
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
        crate::session::ensure_daemon_started(&self.layout)?;

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
        let info = if no_create {
            let id = name.ok_or_else(|| "attach --no-create requires a session id".to_string())?;
            validate_runtime_name(&id)?;
            if !self.layout.session_dir(&id).exists() {
                return Err(format!("missing session {id}"));
            }
            // inspect() may auto-start a daemon. An existing-only attachment
            // must neither start a replacement nor clean up retained state on
            // a connection, permission, protocol or session lookup failure.
            let result = self.http_json(&id, Method::GET, &format!("/sessions/{id}"), &())?;
            session_info_from_inspect(result, SessionStatus::Attached)
        } else {
            let session = ensure_session_started(&self.layout, name, vt_engine, cwd, cmd, SessionStartOptions {
                record: options.record,
                ..Default::default()
            })?;
            // Get real config before taking the foreground slot.
            if let Ok(result) = self.inspect(&session.id) {
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
                    conpty: None,
                    error: None,
                }
            }
        };
        // Recording is part of setup, before observers can see the foreground
        // grant and act on it (including killing the session).
        if options.record {
            self.record(&info.id, true)?;
        }
        // Passthrough has no structured render surface, so it remains on the
        // legacy byte stream. Functional interactive terminals use packets.
        let attach = if info.vt_engine == VtEngineKind::Passthrough {
            attach_foreground(&self.layout, &info.id, options.identity, options.strict, options.take)?
        } else {
            attach_packet_foreground(
                &self.layout,
                &info.id,
                options.identity,
                crate::packet::ChannelRole::Controller,
                options.strict,
                options.take,
            )?
        };
        Ok((info, attach))
    }

    pub fn watch(&self, id: &str, identity: AttachmentIdentity) -> Result<ForegroundAttach, String> {
        if !self.layout.session_dir(id).exists() {
            return Err(format!("missing session {id}"));
        }
        let vt_engine = self.inspect(id)?.session.vt_engine;
        if vt_engine == VtEngineKind::Passthrough.as_str() {
            watch_foreground(&self.layout, id, identity)
        } else {
            attach_packet_foreground(&self.layout, id, identity, crate::packet::ChannelRole::Watcher, false, false)
        }
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
        crate::session::ensure_daemon_started(&self.layout)?;
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
        http_uds::write_request_with_epoch(&mut stream, method, path, &body, self.hosting_epoch)
            .map_err(|err| format!("write HTTP request: {err}"))?;
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
            conpty: None,
            error: Some(err.clone()),
        });
    }
    remove_stale_daemon_file(layout.socket_path(), "socket")?;
    if layout.generation().is_none() {
        remove_stale_daemon_file(layout.daemon_pid_path(), "pid")?;
    }
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
        conpty: result.session.conpty,
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
    fn list_starts_dead_daemon_without_sweeping_retained_sessions() {
        let temp = tempfile::tempdir().expect("tempdir");
        let layout = RuntimeLayout::new(temp.path().to_path_buf());
        let kept_dir = create_test_session_dir(temp.path(), "kept");
        let discarded_dir = create_test_session_dir(temp.path(), "discarded");
        fs::write(kept_dir.join(crate::recording::CAST_FILE_NAME), b"{\"version\":3}\n").expect("write cast");
        fs::write(layout.foreground_path("kept"), b"12345").expect("write foreground marker");
        fs::write(layout.daemon_pid_path(), "999999999").expect("write stale pid");

        let service = SessionService::new(layout.clone());
        let sessions = service.list_all_with_selectors(&[]).expect("list all");

        assert!(sessions.is_empty(), "new daemon has no live sessions");
        assert!(kept_dir.exists(), "recreatable session should be preserved");
        assert!(kept_dir.join(crate::recording::CAST_FILE_NAME).exists(), "recording should remain");
        assert!(layout.foreground_path("kept").exists(), "list should not mutate retained session state");
        assert!(discarded_dir.exists(), "list should not remove retained session state");
        assert_ne!(fs::read_to_string(layout.daemon_pid_path()).expect("read daemon pid"), "999999999");
        crate::platform::daemon::terminate_session_daemon_if_expected(temp.path(), crate::runtime::DEFAULT_DAEMON_NAME);
    }

    #[test]
    fn list_without_sessions_does_not_start_a_daemon() {
        let temp = tempfile::tempdir().expect("tempdir");
        let layout = RuntimeLayout::new(temp.path().to_path_buf());
        let service = SessionService::new(layout.clone());

        assert!(service.list().expect("list empty runtime").is_empty());
        assert!(!layout.daemon_dir().exists(), "empty list should not create a daemon directory");
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

#[cfg(test)]
#[path = "server_drain_tests.rs"]
mod drain_tests;
