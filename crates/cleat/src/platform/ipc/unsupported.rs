use std::{
    io::{self, Read, Write},
    path::Path,
    time::Duration,
};

#[derive(Debug)]
pub struct SessionStream;

#[derive(Debug)]
pub struct SessionListener;

impl SessionStream {
    pub fn try_clone(&self) -> io::Result<Self> {
        Err(unsupported_io())
    }
}

impl Read for SessionStream {
    fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
        Err(unsupported_io())
    }
}

impl Write for SessionStream {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(unsupported_io())
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(unsupported_io())
    }
}

pub fn connect_session_stream(_socket_path: &Path) -> Result<SessionStream, String> {
    Err("session IPC is only supported on Unix".to_string())
}

pub fn try_connect_session_stream(_socket_path: &Path) -> io::Result<SessionStream> {
    Err(unsupported_io())
}

pub fn bind_session_listener(_socket_path: &Path) -> Result<SessionListener, String> {
    Err("session IPC listener is only supported on Unix".to_string())
}

pub fn set_listener_nonblocking(_listener: &SessionListener, _nonblocking: bool) -> Result<(), String> {
    Err("session IPC listener is only supported on Unix".to_string())
}

pub fn set_stream_nonblocking(_stream: &SessionStream, _nonblocking: bool) -> Result<(), String> {
    Err("session IPC stream is only supported on Unix".to_string())
}

pub fn set_stream_read_timeout(_stream: &SessionStream, _timeout: Option<Duration>) -> Result<(), String> {
    Err("session IPC stream is only supported on Unix".to_string())
}

pub fn shutdown_stream(_stream: &SessionStream) {}

fn unsupported_io() -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, "session IPC is only supported on Unix")
}
