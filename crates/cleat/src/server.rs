use std::{
    path::Path,
    thread,
    time::{Duration, Instant},
};

use http::{Method, StatusCode};
use serde::de::DeserializeOwned;

use crate::{
    http_uds,
    platform::{
        daemon::{daemon_pid_path, is_session_daemon_alive, terminate_session_daemon_if_expected},
        ipc::{set_stream_read_timeout, try_connect_session_stream, SessionStream},
    },
    protocol::{SessionInfo, SessionStatus},
    runtime::{RuntimeLayout, TerminalSize},
    session::{
        attach_foreground, ensure_session_started, foreground_path, run_session_daemon, session_socket_path, watch_foreground,
        ForegroundAttach, SessionStartOptions,
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
        let session = ensure_session_started(&self.layout, name, vt_engine, cwd, cmd, SessionStartOptions {
            record,
            initial_size,
            colors: crate::vt::TerminalColors::default(),
        })?;
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
                if session_dir_is_empty(&path) {
                    let _ = self.layout.remove_session(&id);
                }
                continue;
            }
            if !daemon_pid_path(self.layout.root(), &id).exists()
                && !crate::recreate::session_is_recreatable(&path)
                && session_dir_contains_only(&path, &socket_path)
            {
                let _ = self.layout.remove_session(&id);
                continue;
            }
            if !is_session_daemon_alive(self.layout.root(), &id) {
                // Stale daemon. Only garbage-collect it if there is nothing to
                // recreate from; a crashed session with a recording survives so a
                // later create/attach can respawn it from its prior output.
                if !crate::recreate::session_is_recreatable(&path) {
                    let _ = self.layout.remove_session(&id);
                }
                continue;
            }
            match self.inspect(&id) {
                Ok(result) => {
                    let status =
                        if has_controller_attachment(&result.attachments) { SessionStatus::Attached } else { SessionStatus::Detached };
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
        self.kill_with_purge(id, false)
    }

    pub fn kill_with_purge(&self, id: &str, purge: bool) -> Result<(), String> {
        if !self.layout.root().join(id).exists() {
            return Err(format!("missing session {id}"));
        }
        let pid_path = daemon_pid_path(self.layout.root(), id);
        if self.http_no_content(id, Method::DELETE, &format!("/sessions/{id}"), &()).is_ok() {
            if pid_path.exists() {
                self.wait_for_session_shutdown(id);
            }
            if !self.layout.root().join(id).exists() {
                return Ok(());
            }
        }
        terminate_session_daemon_if_expected(self.layout.root(), id);
        self.wait_for_session_shutdown(id);
        if !self.layout.root().join(id).exists() {
            return Ok(());
        }
        if purge || !crate::recreate::session_is_recreatable(&self.layout.root().join(id)) {
            self.layout.remove_session(id)
        } else {
            self.remove_volatile_session_files(id);
            Ok(())
        }
    }

    fn wait_for_session_shutdown(&self, id: &str) {
        for _ in 0..50 {
            if !self.layout.root().join(id).exists()
                || !daemon_pid_path(self.layout.root(), id).exists()
                || !is_session_daemon_alive(self.layout.root(), id)
            {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn remove_volatile_session_files(&self, id: &str) {
        let _ = std::fs::remove_file(session_socket_path(self.layout.root(), id));
        let _ = std::fs::remove_file(daemon_pid_path(self.layout.root(), id));
        let _ = std::fs::remove_file(foreground_path(self.layout.root(), id));
    }

    pub fn detach(&self, id: &str) -> Result<(), String> {
        if !self.layout.root().join(id).exists() {
            return Err(format!("missing session {id}"));
        }

        self.http_no_content(id, Method::POST, &format!("/sessions/{id}/detach"), &())
    }

    pub fn capture(&self, id: &str) -> Result<String, String> {
        if !self.layout.root().join(id).exists() {
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
        let cast_path = self.layout.root().join(id).join(crate::recording::CAST_FILE_NAME);
        if !cast_path.exists() {
            return Err(format!("no recording for session {id}"));
        }

        let (start_offset, end_offset, end_status) = self.resolve_slice_range(id, start, end, &cast_path)?;

        let events = crate::cast_reader::read_output_between(&cast_path, start_offset, end_offset)?;
        let output: String = events.iter().map(|e| e.data.as_str()).collect();
        Ok((output, SliceOutcome { start_offset, end_offset, end_status }))
    }

    pub fn send_keys(&self, id: &str, bytes: &[u8]) -> Result<(), String> {
        if !self.layout.root().join(id).exists() {
            return Err(format!("missing session {id}"));
        }

        self.http_no_content(id, Method::POST, &format!("/sessions/{id}/keys"), &http_uds::KeysRequest { bytes: bytes.to_vec() })
    }

    pub fn send_keys_with_mark(&self, id: &str, bytes: &[u8], marker_name: &str) -> Result<u64, String> {
        if !self.layout.root().join(id).exists() {
            return Err(format!("missing session {id}"));
        }
        let response: http_uds::MarkResponse =
            self.http_json(id, Method::POST, &format!("/sessions/{id}/keys-with-mark"), &http_uds::KeysWithMarkRequest {
                bytes: bytes.to_vec(),
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
    ) -> Result<(SessionInfo, ForegroundAttach), String> {
        let session = if no_create {
            let id = name.ok_or_else(|| "attach --no-create requires a session id".to_string())?;
            let socket_path = session_socket_path(self.layout.root(), &id);
            if !socket_path.exists() {
                return Err(format!("missing session {id}"));
            }
            if !is_session_daemon_alive(self.layout.root(), &id) {
                // The daemon crashed. --no-create forbids respawning, so report it
                // as stale. Preserve a recoverable recording (a later attach without
                // --no-create recreates from it); only remove a non-recreatable husk.
                if crate::recreate::session_is_recreatable(&self.layout.root().join(&id)) {
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
                status: SessionStatus::Attached,
                error: None,
            }
        };
        let attach = attach_foreground(&self.layout, &info.id)?;
        Ok((info, attach))
    }

    pub fn watch(&self, id: &str) -> Result<ForegroundAttach, String> {
        if !self.layout.root().join(id).exists() {
            return Err(format!("missing session {id}"));
        }
        watch_foreground(&self.layout, id)
    }

    pub fn inspect(&self, id: &str) -> Result<crate::protocol::InspectResult, String> {
        if !self.layout.root().join(id).exists() {
            return Err(format!("missing session {id}"));
        }
        self.http_json(id, Method::GET, &format!("/sessions/{id}"), &())
    }

    pub fn signal(&self, id: &str, signal: i32, target: crate::protocol::SignalTarget) -> Result<(), String> {
        if !self.layout.root().join(id).exists() {
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
        if !self.layout.root().join(id).exists() {
            return Err(format!("missing session {id}"));
        }
        let response: http_uds::MarkResponse =
            self.http_json(id, Method::POST, &format!("/sessions/{id}/mark"), &http_uds::MarkRequest { name: name.map(str::to_string) })?;
        Ok(response.offset)
    }

    pub fn resolve_marker(&self, id: &str, name: &str) -> Result<u64, String> {
        if !self.layout.root().join(id).exists() {
            return Err(format!("missing session {id}"));
        }
        let response: http_uds::MarkResponse =
            self.http_json(id, Method::POST, &format!("/sessions/{id}/resolve-marker"), &http_uds::ResolveMarkerRequest {
                name: name.to_string(),
            })?;
        Ok(response.offset)
    }

    pub fn resolve_next_marker_after(&self, id: &str, after: u64) -> Result<Option<u64>, String> {
        if !self.layout.root().join(id).exists() {
            return Err(format!("missing session {id}"));
        }
        let response: http_uds::ResolveNextMarkerResponse =
            self.http_json(id, Method::POST, &format!("/sessions/{id}/resolve-next-marker"), &http_uds::ResolveNextMarkerRequest {
                after,
            })?;
        Ok(response.offset)
    }

    pub fn record(&self, id: &str, enable: bool) -> Result<(), String> {
        if !self.layout.root().join(id).exists() {
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
        if !self.layout.root().join(id).exists() {
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
        if !self.layout.root().join(id).exists() {
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

    pub fn serve(&self, session: &crate::runtime::SessionMetadata) -> Result<(), String> {
        run_session_daemon(self.layout.root(), session)
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

    fn http_request_with_read_timeout<T: serde::Serialize>(
        &self,
        id: &str,
        method: Method,
        path: &str,
        body: &T,
        read_timeout: Option<Duration>,
    ) -> Result<http_uds::HttpResponse, String> {
        let socket_path = session_socket_path(self.layout.root(), id);
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

fn session_dir_is_empty(path: &Path) -> bool {
    std::fs::read_dir(path).map(|mut entries| entries.next().is_none()).unwrap_or(false)
}

fn session_dir_contains_only(path: &Path, expected: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(path) else {
        return false;
    };
    let mut count = 0;
    for entry in entries {
        let Ok(entry) = entry else {
            return false;
        };
        count += 1;
        if entry.path() != expected {
            return false;
        }
    }
    count == 1
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
        status,
        error: None,
    }
}

fn has_controller_attachment(attachments: &[crate::protocol::AttachmentInspect]) -> bool {
    attachments.iter().any(|attachment| attachment.role == "controller")
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
    use std::{fs, os::unix::net::UnixListener, process::Command, sync::mpsc, thread, time::Duration};

    use super::SessionService;
    use crate::{
        http_uds::read_http_request_for_test,
        protocol::{WaitCondition, WaitStatus},
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
    fn send_keys_posts_http_request_to_session_socket() {
        let temp = tempfile::tempdir().expect("tempdir");
        let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));
        let session_dir = temp.path().join("alpha");
        fs::create_dir_all(&session_dir).expect("create session dir");

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
    fn kill_deletes_session_over_http_when_socket_is_available() {
        let temp = tempfile::tempdir().expect("tempdir");
        let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));
        let session_dir = temp.path().join("alpha");
        fs::create_dir_all(&session_dir).expect("create session dir");

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
    fn detach_posts_http_request_to_session_socket() {
        let temp = tempfile::tempdir().expect("tempdir");
        let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));
        let session_dir = temp.path().join("alpha");
        fs::create_dir_all(&session_dir).expect("create session dir");

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
        let session_dir = temp.path().join("alpha");
        fs::create_dir_all(&session_dir).expect("create session dir");

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

        let result = service.wait("alpha", vec![WaitCondition::OutputIdle { quiet_ms: 250 }], 5000).expect("wait");
        let request = rx.recv_timeout(Duration::from_secs(1)).expect("receive request");

        reader.join().expect("join reader");
        assert_eq!(result, (WaitStatus::Ready, 42));
        assert!(request.starts_with("POST /sessions/alpha/wait HTTP/1.1\r\n"), "{request}");
        assert!(request.ends_with(r#"{"conditions":[{"kind":"output_idle","quiet_ms":250}],"timeout_ms":5000}"#), "{request}");
    }

    #[test]
    fn expect_posts_http_request_to_session_socket() {
        let temp = tempfile::tempdir().expect("tempdir");
        let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));
        let session_dir = temp.path().join("alpha");
        fs::create_dir_all(&session_dir).expect("create session dir");

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
        fs::write(daemon_pid_path(temp.path(), "broken-session"), std::process::id().to_string()).expect("write pid");

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

    #[test]
    fn list_cleans_up_socket_only_session_without_pid_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));

        let session_dir = temp.path().join("socket-only");
        fs::create_dir_all(&session_dir).expect("create session dir");
        let socket_path = session_socket_path(temp.path(), "socket-only");
        let listener = UnixListener::bind(&socket_path).expect("bind socket");
        drop(listener);

        let sessions = service.list().expect("list sessions");
        assert!(sessions.is_empty(), "socket-only husk should not appear in list");
        assert!(!session_dir.exists(), "socket-only husk should be cleaned up");
    }

    #[test]
    fn list_cleans_up_empty_session_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));

        let session_dir = temp.path().join("empty-session");
        fs::create_dir_all(&session_dir).expect("create session dir");

        let sessions = service.list().expect("list sessions");
        assert!(sessions.is_empty(), "empty session dir should not appear in list");
        assert!(!session_dir.exists(), "empty session dir should be cleaned up");
    }

    #[test]
    fn list_preserves_stale_sessions_that_have_a_recording() {
        let temp = tempfile::tempdir().expect("tempdir");
        let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));

        // A crashed daemon: stale socket + dead PID, but a recording survives.
        let session_dir = temp.path().join("crashed-session");
        fs::create_dir_all(&session_dir).expect("create session dir");
        let socket_path = session_socket_path(temp.path(), "crashed-session");
        let listener = UnixListener::bind(&socket_path).expect("bind socket");
        drop(listener);
        fs::write(daemon_pid_path(temp.path(), "crashed-session"), "999999999").expect("write pid");
        fs::write(session_dir.join(crate::recording::CAST_FILE_NAME), b"{\"version\":3}\n").expect("write cast");

        let sessions = service.list().expect("list sessions");
        assert!(sessions.is_empty(), "stale session should not appear in list");
        assert!(session_dir.exists(), "a recreatable session directory must survive the stale sweep");
    }
}
