use std::{io, net::Shutdown, path::Path, time::Duration};

pub type SessionStream = std::os::unix::net::UnixStream;
pub type SessionListener = std::os::unix::net::UnixListener;

pub fn validate_session_socket_path(socket_path: &Path) -> Result<(), String> {
    std::os::unix::net::SocketAddr::from_pathname(socket_path).map(|_| ()).map_err(|err| {
        format!(
            "Unix socket path {} is too long or otherwise unusable: {err}; set CLEAT_RUNTIME_DIR to a shorter persistent path",
            socket_path.display()
        )
    })
}

pub fn connect_session_stream(socket_path: &Path) -> Result<SessionStream, String> {
    try_connect_session_stream(socket_path).map_err(|err| format!("connect {}: {err}", socket_path.display()))
}

pub fn try_connect_session_stream(socket_path: &Path) -> io::Result<SessionStream> {
    SessionStream::connect(socket_path)
}

pub fn bind_session_listener(socket_path: &Path) -> Result<SessionListener, String> {
    SessionListener::bind(socket_path).map_err(|err| format!("bind socket {}: {err}", socket_path.display()))
}

pub fn set_listener_nonblocking(listener: &SessionListener, nonblocking: bool) -> Result<(), String> {
    listener.set_nonblocking(nonblocking).map_err(|err| format!("set listener nonblocking: {err}"))
}

pub fn set_stream_nonblocking(stream: &SessionStream, nonblocking: bool) -> Result<(), String> {
    stream.set_nonblocking(nonblocking).map_err(|err| format!("set stream nonblocking: {err}"))
}

pub fn set_stream_read_timeout(stream: &SessionStream, timeout: Option<Duration>) -> Result<(), String> {
    stream.set_read_timeout(timeout).map_err(|err| format!("set stream read timeout: {err}"))
}

pub fn set_stream_write_timeout(stream: &SessionStream, timeout: Option<Duration>) -> Result<(), String> {
    stream.set_write_timeout(timeout).map_err(|err| format!("set stream write timeout: {err}"))
}

pub fn shutdown_stream(stream: &SessionStream) {
    let _ = stream.shutdown(Shutdown::Both);
}
