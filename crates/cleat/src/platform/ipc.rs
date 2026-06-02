#[cfg(not(unix))]
use std::io::{Read, Write};
use std::{
    io,
    net::Shutdown,
    path::{Path, PathBuf},
    time::Duration,
};

const SOCKET_NAME: &str = "socket";

pub fn session_socket_path(root: &Path, id: &str) -> PathBuf {
    root.join(id).join(SOCKET_NAME)
}

#[cfg(unix)]
pub type SessionStream = std::os::unix::net::UnixStream;

#[cfg(unix)]
pub type SessionListener = std::os::unix::net::UnixListener;

#[cfg(unix)]
pub fn connect_session_stream(socket_path: &Path) -> Result<SessionStream, String> {
    try_connect_session_stream(socket_path).map_err(|err| format!("connect {}: {err}", socket_path.display()))
}

#[cfg(unix)]
pub fn try_connect_session_stream(socket_path: &Path) -> io::Result<SessionStream> {
    SessionStream::connect(socket_path)
}

#[cfg(unix)]
pub fn bind_session_listener(socket_path: &Path) -> Result<SessionListener, String> {
    SessionListener::bind(socket_path).map_err(|err| format!("bind socket {}: {err}", socket_path.display()))
}

#[cfg(unix)]
pub fn set_listener_nonblocking(listener: &SessionListener, nonblocking: bool) -> Result<(), String> {
    listener.set_nonblocking(nonblocking).map_err(|err| format!("set listener nonblocking: {err}"))
}

#[cfg(unix)]
pub fn set_stream_nonblocking(stream: &SessionStream, nonblocking: bool) -> Result<(), String> {
    stream.set_nonblocking(nonblocking).map_err(|err| format!("set stream nonblocking: {err}"))
}

#[cfg(unix)]
pub fn set_stream_read_timeout(stream: &SessionStream, timeout: Option<Duration>) -> Result<(), String> {
    stream.set_read_timeout(timeout).map_err(|err| format!("set stream read timeout: {err}"))
}

#[cfg(unix)]
pub fn shutdown_stream(stream: &SessionStream) {
    let _ = stream.shutdown(Shutdown::Both);
}

#[cfg(not(unix))]
#[derive(Debug)]
pub struct SessionStream;

#[cfg(not(unix))]
impl SessionStream {
    pub fn try_clone(&self) -> Result<Self, std::io::Error> {
        Err(unsupported_io())
    }
}

#[cfg(not(unix))]
impl Read for SessionStream {
    fn read(&mut self, _buf: &mut [u8]) -> Result<usize, std::io::Error> {
        Err(unsupported_io())
    }
}

#[cfg(not(unix))]
impl Write for SessionStream {
    fn write(&mut self, _buf: &[u8]) -> Result<usize, std::io::Error> {
        Err(unsupported_io())
    }

    fn flush(&mut self) -> Result<(), std::io::Error> {
        Err(unsupported_io())
    }
}

#[cfg(not(unix))]
pub fn connect_session_stream(_socket_path: &Path) -> Result<SessionStream, String> {
    Err("session IPC is only supported on Unix".to_string())
}

#[cfg(not(unix))]
pub fn try_connect_session_stream(_socket_path: &Path) -> io::Result<SessionStream> {
    Err(unsupported_io())
}

#[cfg(not(unix))]
pub fn shutdown_stream(_stream: &SessionStream) {}

#[cfg(not(unix))]
fn unsupported_io() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Unsupported, "session IPC is only supported on Unix")
}
