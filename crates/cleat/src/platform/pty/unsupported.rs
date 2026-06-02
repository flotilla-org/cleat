use crate::{
    platform::ipc::{SessionListener, SessionStream},
    runtime::SessionMetadata,
};

pub struct PtyChild;

impl PtyChild {
    pub fn spawn(_session: &SessionMetadata) -> Result<Self, String> {
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
    _listener_fd: i32,
    _client_fd: Option<i32>,
    _client_needs_write: bool,
    _pty_fd: i32,
    _timeout_ms: i32,
) -> Result<PollResult, String> {
    Err("PTY sessions are only supported on Unix".to_string())
}

pub fn exit_code_from_wait_status(_status: &()) -> i32 {
    1
}

pub fn stream_fd(_stream: &SessionStream) -> i32 {
    -1
}

pub fn listener_fd(_listener: &SessionListener) -> i32 {
    -1
}
