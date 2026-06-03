use std::{
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    io::{self, Read, Write},
    mem::zeroed,
    path::Path,
    ptr::{null, null_mut},
    sync::Mutex,
    time::{Duration, Instant},
};

use windows_sys::Win32::{
    Foundation::{
        CloseHandle, DuplicateHandle, GetLastError, DUPLICATE_SAME_ACCESS, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING,
        ERROR_NO_DATA, ERROR_OPERATION_ABORTED, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_PIPE_LISTENING, GENERIC_READ, GENERIC_WRITE,
        HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Storage::FileSystem::{CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, PIPE_ACCESS_DUPLEX},
    System::{
        Pipes::{ConnectNamedPipe, CreateNamedPipeW, PeekNamedPipe, WaitNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT},
        Threading::{CreateEventW, GetCurrentProcess, ResetEvent, SetEvent, WaitForSingleObject},
        IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
    },
};

#[derive(Debug)]
pub struct SessionStream {
    handle: HANDLE,
}

pub struct SessionListener {
    pipe_name: Vec<u16>,
    pending: Mutex<Option<PendingPipeInstance>>,
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
        let instance = pending.as_mut().ok_or_else(|| io::Error::other("session listener has no pending pipe instance"))?;
        if !instance.poll_connected()? {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "no named-pipe client is waiting"));
        }

        let instance = pending.take().expect("pending checked");
        *pending = Some(PendingPipeInstance::new(&self.pipe_name)?);
        Ok((SessionStream { handle: instance.into_handle() }, ()))
    }
}

impl Drop for SessionListener {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.pending.lock() {
            if let Some(handle) = pending.take() {
                drop(handle);
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

    pub(crate) fn overlapped_reader(&self, capacity: usize) -> io::Result<OverlappedRead> {
        OverlappedRead::new(self.handle, capacity)
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
        overlapped_read_blocking(self.handle, buf)
    }
}

impl Write for SessionStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        overlapped_write_blocking(self.handle, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn connect_session_stream(socket_path: &Path) -> Result<SessionStream, String> {
    try_connect_session_stream(socket_path).map_err(|err| format!("connect {}: {err}", socket_path.display()))
}

pub fn try_connect_session_stream(socket_path: &Path) -> io::Result<SessionStream> {
    let pipe_name = pipe_name_from_marker_file(socket_path)?;
    let handle = open_pipe(&pipe_name)?;
    Ok(SessionStream { handle })
}

pub fn bind_session_listener(socket_path: &Path) -> Result<SessionListener, String> {
    let pipe_name = pipe_name_for_socket_path(socket_path);
    let handle = PendingPipeInstance::new(&pipe_name).map_err(|err| format!("create named pipe {}: {err}", display_wide(&pipe_name)))?;
    fs::write(socket_path, display_wide(&pipe_name)).map_err(|err| format!("write named-pipe marker {}: {err}", socket_path.display()))?;

    Ok(SessionListener { pipe_name, pending: Mutex::new(Some(handle)) })
}

pub fn set_listener_nonblocking(_listener: &SessionListener, _nonblocking: bool) -> Result<(), String> {
    // Windows uses overlapped pipe operations for nonblocking readiness. PIPE_NOWAIT
    // is intentionally not used; Microsoft documents it as a LAN Manager
    // compatibility mode, not an async I/O mechanism.
    Ok(())
}

pub fn set_stream_nonblocking(_stream: &SessionStream, _nonblocking: bool) -> Result<(), String> {
    Ok(())
}

pub fn set_stream_read_timeout(_stream: &SessionStream, _timeout: Option<Duration>) -> Result<(), String> {
    Ok(())
}

pub fn shutdown_stream(stream: &SessionStream) {
    unsafe {
        CancelIoEx(stream.handle, null_mut());
    }
}

struct PendingPipeInstance {
    handle: HANDLE,
    connect: Option<PendingConnect>,
}

impl PendingPipeInstance {
    fn new(pipe_name: &[u16]) -> io::Result<Self> {
        Ok(Self { handle: create_pipe_instance(pipe_name)?, connect: None })
    }

    fn poll_connected(&mut self) -> io::Result<bool> {
        if self.connect.is_none() {
            let mut connect = PendingConnect::new()?;
            let connected = unsafe { ConnectNamedPipe(self.handle, connect.overlapped_mut()) };
            if connected != 0 {
                return Ok(true);
            }
            match unsafe { GetLastError() } {
                ERROR_PIPE_CONNECTED => {
                    unsafe {
                        SetEvent(connect.event);
                    }
                    return Ok(true);
                }
                ERROR_IO_PENDING => {
                    self.connect = Some(connect);
                    return Ok(false);
                }
                ERROR_NO_DATA | ERROR_PIPE_LISTING_ALIAS => return Ok(false),
                err => return Err(io_error_from_code(err)),
            }
        }

        let connect = self.connect.as_mut().expect("connect initialized");
        match unsafe { WaitForSingleObject(connect.event, 0) } {
            WAIT_TIMEOUT => Ok(false),
            WAIT_OBJECT_0 => {
                let mut transferred = 0;
                let ok = unsafe { GetOverlappedResult(self.handle, connect.overlapped_mut(), &mut transferred, 0) };
                if ok == 0 {
                    let err = unsafe { GetLastError() };
                    if err == ERROR_PIPE_CONNECTED {
                        self.connect = None;
                        Ok(true)
                    } else {
                        Err(io_error_from_code(err))
                    }
                } else {
                    self.connect = None;
                    Ok(true)
                }
            }
            status => Err(io::Error::other(format!("WaitForSingleObject connect returned {status}"))),
        }
    }

    fn into_handle(mut self) -> HANDLE {
        self.connect = None;
        let handle = self.handle;
        self.handle = null_mut();
        handle
    }
}

impl Drop for PendingPipeInstance {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                CancelIoEx(self.handle, null_mut());
                CloseHandle(self.handle);
            }
        }
    }
}

struct PendingConnect {
    event: HANDLE,
    overlapped: Box<OVERLAPPED>,
}

impl PendingConnect {
    fn new() -> io::Result<Self> {
        let event = unsafe { CreateEventW(null(), 1, 0, null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut overlapped = Box::new(unsafe { zeroed::<OVERLAPPED>() });
        overlapped.hEvent = event;
        Ok(Self { event, overlapped })
    }

    fn overlapped_mut(&mut self) -> *mut OVERLAPPED {
        &mut *self.overlapped
    }
}

impl Drop for PendingConnect {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.event);
        }
    }
}

pub(crate) struct OverlappedRead {
    handle: HANDLE,
    event: HANDLE,
    overlapped: Box<OVERLAPPED>,
    buffer: Vec<u8>,
    pending: bool,
}

impl OverlappedRead {
    fn new(handle: HANDLE, capacity: usize) -> io::Result<Self> {
        let event = unsafe { CreateEventW(null(), 1, 0, null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut overlapped = Box::new(unsafe { zeroed::<OVERLAPPED>() });
        overlapped.hEvent = event;
        Ok(Self { handle, event, overlapped, buffer: vec![0; capacity], pending: false })
    }

    pub(crate) fn poll(&mut self) -> io::Result<Option<Vec<u8>>> {
        self.poll_timeout(Duration::ZERO)
    }

    pub(crate) fn poll_timeout(&mut self, timeout: Duration) -> io::Result<Option<Vec<u8>>> {
        if !self.pending {
            unsafe {
                ResetEvent(self.event);
            }
            *self.overlapped = unsafe { zeroed::<OVERLAPPED>() };
            self.overlapped.hEvent = self.event;
            let mut read = 0;
            let ok = unsafe {
                ReadFile(
                    self.handle,
                    self.buffer.as_mut_ptr(),
                    self.buffer.len().min(u32::MAX as usize) as u32,
                    &mut read,
                    &mut *self.overlapped,
                )
            };
            if ok != 0 {
                return Ok(Some(self.buffer[..read as usize].to_vec()));
            }
            match unsafe { GetLastError() } {
                ERROR_IO_PENDING => self.pending = true,
                ERROR_BROKEN_PIPE | ERROR_OPERATION_ABORTED | ERROR_NO_DATA => return Ok(Some(Vec::new())),
                err => return Err(io_error_from_code(err)),
            }
        }

        match unsafe { WaitForSingleObject(self.event, wait_timeout_ms(timeout)) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => {
                let mut transferred = 0;
                let ok = unsafe { GetOverlappedResult(self.handle, &mut *self.overlapped, &mut transferred, 0) };
                self.pending = false;
                if ok == 0 {
                    let err = unsafe { GetLastError() };
                    match err {
                        ERROR_BROKEN_PIPE | ERROR_OPERATION_ABORTED | ERROR_NO_DATA => Ok(Some(Vec::new())),
                        _ => Err(io_error_from_code(err)),
                    }
                } else {
                    Ok(Some(self.buffer[..transferred as usize].to_vec()))
                }
            }
            status => Err(io::Error::other(format!("WaitForSingleObject read returned {status}"))),
        }
    }
}

impl Drop for OverlappedRead {
    fn drop(&mut self) {
        unsafe {
            if self.pending {
                CancelIoEx(self.handle, &mut *self.overlapped);
            }
            CloseHandle(self.event);
        }
    }
}

// windows-sys imports Win32 constants as static values, which cannot be used
// directly in Rust match patterns.
const ERROR_PIPE_LISTING_ALIAS: u32 = ERROR_PIPE_LISTENING;

fn overlapped_read_blocking(handle: HANDLE, buf: &mut [u8]) -> io::Result<usize> {
    let event = unsafe { CreateEventW(null(), 1, 0, null()) };
    if event.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut overlapped: OVERLAPPED = unsafe { zeroed() };
    overlapped.hEvent = event;
    let mut read = 0;
    let ok = unsafe { ReadFile(handle, buf.as_mut_ptr(), buf.len().min(u32::MAX as usize) as u32, &mut read, &mut overlapped) };
    let result = if ok != 0 {
        Ok(read as usize)
    } else {
        match unsafe { GetLastError() } {
            ERROR_IO_PENDING => wait_overlapped(handle, &mut overlapped),
            ERROR_BROKEN_PIPE | ERROR_OPERATION_ABORTED => Ok(0),
            err => Err(io_error_from_code(err)),
        }
    };
    unsafe {
        CloseHandle(event);
    }
    result
}

fn overlapped_write_blocking(handle: HANDLE, buf: &[u8]) -> io::Result<usize> {
    let event = unsafe { CreateEventW(null(), 1, 0, null()) };
    if event.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut overlapped: OVERLAPPED = unsafe { zeroed() };
    overlapped.hEvent = event;
    let mut written = 0;
    let ok = unsafe { WriteFile(handle, buf.as_ptr(), buf.len().min(u32::MAX as usize) as u32, &mut written, &mut overlapped) };
    let result = if ok != 0 {
        Ok(written as usize)
    } else {
        match unsafe { GetLastError() } {
            ERROR_IO_PENDING => wait_overlapped(handle, &mut overlapped),
            ERROR_BROKEN_PIPE | ERROR_OPERATION_ABORTED => Ok(0),
            err => Err(io_error_from_code(err)),
        }
    };
    unsafe {
        CloseHandle(event);
    }
    result
}

fn wait_overlapped(handle: HANDLE, overlapped: &mut OVERLAPPED) -> io::Result<usize> {
    match unsafe { WaitForSingleObject(overlapped.hEvent, u32::MAX) } {
        WAIT_OBJECT_0 => {
            let mut transferred = 0;
            let ok = unsafe { GetOverlappedResult(handle, overlapped, &mut transferred, 0) };
            if ok == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(transferred as usize)
            }
        }
        status => Err(io::Error::other(format!("WaitForSingleObject overlapped returned {status}"))),
    }
}

fn create_pipe_instance(pipe_name: &[u16]) -> io::Result<HANDLE> {
    let pipe_mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT;
    let handle = unsafe {
        CreateNamedPipeW(pipe_name.as_ptr(), PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED, pipe_mode, 255, 64 * 1024, 64 * 1024, 0, null())
    };
    if handle == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        Ok(handle)
    }
}

fn open_pipe(pipe_name: &[u16]) -> io::Result<HANDLE> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let handle = unsafe {
            CreateFileW(pipe_name.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, null(), OPEN_EXISTING, FILE_FLAG_OVERLAPPED, null_mut())
        };
        if handle != INVALID_HANDLE_VALUE {
            return Ok(handle);
        }

        let error = unsafe { GetLastError() };
        match error {
            ERROR_PIPE_BUSY => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "timed out waiting for named pipe"));
                }
                let remaining_ms = deadline.saturating_duration_since(now).as_millis().min(5_000) as u32;
                let waited = unsafe { WaitNamedPipeW(pipe_name.as_ptr(), remaining_ms) };
                if waited == 0 {
                    let err = io::Error::last_os_error();
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, format!("timed out waiting for named pipe: {err}")));
                    }
                }
            }
            ERROR_FILE_NOT_FOUND => return Err(io_error_from_code(error)),
            _ => return Err(io_error_from_code(error)),
        }
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

fn pipe_name_from_marker_file(socket_path: &Path) -> io::Result<Vec<u16>> {
    let pipe_name = fs::read_to_string(socket_path)?;
    let pipe_name = pipe_name.trim_end_matches(['\r', '\n']);
    if pipe_name.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("empty named-pipe marker {}", socket_path.display())));
    }
    Ok(pipe_name.encode_utf16().chain(Some(0)).collect())
}

fn display_wide(value: &[u16]) -> String {
    String::from_utf16_lossy(value.strip_suffix(&[0]).unwrap_or(value))
}

fn wait_timeout_ms(timeout: Duration) -> u32 {
    timeout.as_millis().min(u32::MAX as u128) as u32
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
