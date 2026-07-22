use crate::{
    platform::ipc::{SessionListener, SessionStream},
    runtime::{AmbientSessionCoordinates, SessionMetadata},
};

pub struct PtyChild;

impl PtyChild {
    pub fn spawn_with_ambient(_session: &SessionMetadata, _coordinates: Option<&AmbientSessionCoordinates>) -> Result<Self, String> {
        Err("PTY sessions are only supported on Unix".to_string())
    }
}

pub struct PollResult {
    pub listener_readable: bool,
    pub client_readable: bool,
    pub client_writable: bool,
    pub pty_readable: bool,
}

pub fn poll_session_ready(
    _listener: &SessionListener,
    _client: Option<&SessionStream>,
    _client_needs_write: bool,
    _pty_child: &PtyChild,
    _timeout_ms: i32,
) -> Result<PollResult, String> {
    Err("PTY sessions are only supported on Unix".to_string())
}

pub fn exit_code_from_wait_status(_status: &()) -> i32 {
    1
}
