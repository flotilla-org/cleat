use std::{
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    io::{self, Read, Write},
    path::Path,
    ptr::{null, null_mut},
    sync::Mutex,
    time::Duration,
};

use windows_sys::Win32::{
    Foundation::{
        CloseHandle, DuplicateHandle, GetLastError, DUPLICATE_SAME_ACCESS, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_NO_DATA,
        ERROR_OPERATION_ABORTED, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_PIPE_LISTENING, GENERIC_READ, GENERIC_WRITE, HANDLE,
        INVALID_HANDLE_VALUE,
    },
    Storage::FileSystem::{CreateFileW, ReadFile, WriteFile, OPEN_EXISTING, PIPE_ACCESS_DUPLEX},
    System::{
        Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, PeekNamedPipe, SetNamedPipeHandleState, WaitNamedPipeW, PIPE_NOWAIT, PIPE_READMODE_BYTE,
            PIPE_TYPE_BYTE, PIPE_WAIT,
        },
        Threading::GetCurrentProcess,
    },
};

#[derive(Debug)]
pub struct SessionStream {
    handle: HANDLE,
}

#[derive(Debug)]
pub struct SessionListener {
    pipe_name: Vec<u16>,
    pending: Mutex<Option<HANDLE>>,
}

// Win32 HANDLE values are process-local kernel handle values. These wrappers
// own their handles and close them on drop, so moving them between threads is
// equivalent to moving a Unix file descriptor wrapper.
unsafe impl Send for SessionStream {}
unsafe impl Send for SessionListener {}
unsafe impl Sync for SessionListener {}

impl SessionListener {
    pub fn accept(&self) -> io::Result<(SessionStream, ())> {
        let mut pending = self.pending.lock().map_err(|_| io::Error::other("session listener mutex poisoned"))?;
        let handle = pending.take().ok_or_else(|| io::Error::other("session listener has no pending pipe instance"))?;

        let connected = unsafe { ConnectNamedPipe(handle, null_mut()) };
        if connected == 0 {
            let error = unsafe { GetLastError() };
            match error {
                ERROR_PIPE_CONNECTED => {}
                ERROR_PIPE_LISTENING | ERROR_NO_DATA => {
                    *pending = Some(handle);
                    return Err(io::Error::new(io::ErrorKind::WouldBlock, "no named-pipe client is waiting"));
                }
                _ => {
                    unsafe {
                        CloseHandle(handle);
                    }
                    return Err(io_error_from_code(error));
                }
            }
        }

        *pending = Some(create_pipe_instance(&self.pipe_name, true)?);
        Ok((SessionStream { handle }, ()))
    }
}

impl Drop for SessionListener {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.pending.lock() {
            if let Some(handle) = pending.take() {
                unsafe {
                    CloseHandle(handle);
                }
            }
        }
    }
}

impl SessionStream {
    pub fn try_clone(&self) -> io::Result<Self> {
        let process = unsafe { GetCurrentProcess() };
        let mut handle = null_mut();
        let ok = unsafe { DuplicateHandle(process, self.handle, process, &mut handle, 0, 0, DUPLICATE_SAME_ACCESS) };
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self { handle })
        }
    }

    pub(crate) fn has_available_bytes(&self) -> Result<bool, String> {
        handle_has_available_bytes(self.handle)
    }
}

impl Drop for SessionStream {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

impl Read for SessionStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut read = 0;
        let ok = unsafe { ReadFile(self.handle, buf.as_mut_ptr(), buf.len().min(u32::MAX as usize) as u32, &mut read, null_mut()) };
        if ok == 0 {
            let error = unsafe { GetLastError() };
            match error {
                ERROR_NO_DATA | ERROR_PIPE_LISTENING => Err(io::Error::new(io::ErrorKind::WouldBlock, "named pipe would block")),
                ERROR_BROKEN_PIPE | ERROR_OPERATION_ABORTED => Ok(0),
                _ => Err(io_error_from_code(error)),
            }
        } else {
            Ok(read as usize)
        }
    }
}

impl Write for SessionStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut written = 0;
        let ok = unsafe { WriteFile(self.handle, buf.as_ptr(), buf.len().min(u32::MAX as usize) as u32, &mut written, null_mut()) };
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(written as usize)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn connect_session_stream(socket_path: &Path) -> Result<SessionStream, String> {
    try_connect_session_stream(socket_path).map_err(|err| format!("connect {}: {err}", socket_path.display()))
}

pub fn try_connect_session_stream(socket_path: &Path) -> io::Result<SessionStream> {
    let pipe_name = pipe_name_for_socket_path(socket_path);
    let handle = open_pipe(&pipe_name)?;
    Ok(SessionStream { handle })
}

pub fn bind_session_listener(socket_path: &Path) -> Result<SessionListener, String> {
    let pipe_name = pipe_name_for_socket_path(socket_path);
    let handle = create_pipe_instance(&pipe_name, true).map_err(|err| format!("create named pipe {}: {err}", display_wide(&pipe_name)))?;
    fs::write(socket_path, display_wide(&pipe_name)).map_err(|err| format!("write named-pipe marker {}: {err}", socket_path.display()))?;

    Ok(SessionListener { pipe_name, pending: Mutex::new(Some(handle)) })
}

pub fn set_listener_nonblocking(listener: &SessionListener, nonblocking: bool) -> Result<(), String> {
    let pending = listener.pending.lock().map_err(|_| "session listener mutex poisoned".to_string())?;
    if let Some(handle) = *pending {
        set_pipe_nonblocking(handle, nonblocking)?;
    }
    Ok(())
}

pub fn set_stream_nonblocking(stream: &SessionStream, nonblocking: bool) -> Result<(), String> {
    set_pipe_nonblocking(stream.handle, nonblocking)
}

pub fn set_stream_read_timeout(_stream: &SessionStream, _timeout: Option<Duration>) -> Result<(), String> {
    Ok(())
}

pub fn shutdown_stream(_stream: &SessionStream) {}

fn create_pipe_instance(pipe_name: &[u16], nonblocking: bool) -> io::Result<HANDLE> {
    let pipe_mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | if nonblocking { PIPE_NOWAIT } else { PIPE_WAIT };
    let handle = unsafe { CreateNamedPipeW(pipe_name.as_ptr(), PIPE_ACCESS_DUPLEX, pipe_mode, 255, 64 * 1024, 64 * 1024, 0, null()) };
    if handle == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        Ok(handle)
    }
}

fn open_pipe(pipe_name: &[u16]) -> io::Result<HANDLE> {
    loop {
        let handle = unsafe { CreateFileW(pipe_name.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, null(), OPEN_EXISTING, 0, null_mut()) };
        if handle != INVALID_HANDLE_VALUE {
            set_pipe_nonblocking(handle, false).map_err(io::Error::other)?;
            return Ok(handle);
        }

        let error = unsafe { GetLastError() };
        match error {
            ERROR_PIPE_BUSY => {
                let waited = unsafe { WaitNamedPipeW(pipe_name.as_ptr(), 5_000) };
                if waited == 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            ERROR_FILE_NOT_FOUND => return Err(io_error_from_code(error)),
            _ => return Err(io_error_from_code(error)),
        }
    }
}

fn set_pipe_nonblocking(handle: HANDLE, nonblocking: bool) -> Result<(), String> {
    let mode = PIPE_READMODE_BYTE | if nonblocking { PIPE_NOWAIT } else { PIPE_WAIT };
    let ok = unsafe { SetNamedPipeHandleState(handle, &mode, null(), null()) };
    if ok == 0 {
        Err(format!("set named-pipe handle state: {}", io::Error::last_os_error()))
    } else {
        Ok(())
    }
}

pub(crate) fn handle_has_available_bytes(handle: HANDLE) -> Result<bool, String> {
    let mut available = 0;
    let ok = unsafe { PeekNamedPipe(handle, null_mut(), 0, null_mut(), &mut available, null_mut()) };
    if ok == 0 {
        let err = unsafe { GetLastError() };
        match err {
            ERROR_BROKEN_PIPE | ERROR_OPERATION_ABORTED | ERROR_NO_DATA => Ok(false),
            _ => Err(format!("peek named pipe: {}", io_error_from_code(err))),
        }
    } else {
        Ok(available > 0)
    }
}

fn pipe_name_for_socket_path(socket_path: &Path) -> Vec<u16> {
    let mut hasher = DefaultHasher::new();
    socket_path.to_string_lossy().hash(&mut hasher);
    format!(r"\\.\pipe\cleat-{:016x}", hasher.finish()).encode_utf16().chain(Some(0)).collect()
}

fn display_wide(value: &[u16]) -> String {
    String::from_utf16_lossy(value.strip_suffix(&[0]).unwrap_or(value))
}

fn io_error_from_code(code: u32) -> io::Error {
    io::Error::from_raw_os_error(code as i32)
}

#[cfg(test)]
mod tests {
    use std::{io::Read, thread};

    use super::*;

    #[test]
    fn named_pipe_stream_round_trips_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let socket_path = temp.path().join("socket");
        let listener = bind_session_listener(&socket_path).expect("bind listener");
        set_listener_nonblocking(&listener, true).expect("listener nonblocking");

        let client = thread::spawn({
            let socket_path = socket_path.clone();
            move || {
                let mut stream = connect_session_stream(&socket_path).expect("connect client");
                stream.write_all(b"ping").expect("write ping");
                let mut buf = [0u8; 4];
                stream.read_exact(&mut buf).expect("read pong");
                assert_eq!(&buf, b"pong");
            }
        });

        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(value) => break value,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(10)),
                Err(err) => panic!("accept: {err}"),
            }
        };

        let mut buf = [0u8; 4];
        stream.read_exact(&mut buf).expect("read ping");
        assert_eq!(&buf, b"ping");
        stream.write_all(b"pong").expect("write pong");
        client.join().expect("client thread");
    }
}
