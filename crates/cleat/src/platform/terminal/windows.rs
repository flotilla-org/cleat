use std::{
    collections::VecDeque,
    io,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use windows_sys::Win32::{
    Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT},
    Storage::FileSystem::{CreateFileW, ReadFile, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING},
    System::{
        Console::{
            GetConsoleMode, GetConsoleScreenBufferInfo, GetStdHandle, ReadConsoleInputW, SetConsoleCtrlHandler, SetConsoleMode,
            CONSOLE_SCREEN_BUFFER_INFO, CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT,
            ENABLE_MOUSE_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
            ENABLE_WINDOW_INPUT, INPUT_RECORD, KEY_EVENT, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, WINDOW_BUFFER_SIZE_EVENT,
        },
        Threading::WaitForSingleObject,
    },
};

const GENERIC_READ_WRITE: u32 = 0x8000_0000 | 0x4000_0000;
const VK_BACK: u16 = 0x08;
const VK_TAB: u16 = 0x09;
const VK_RETURN: u16 = 0x0d;
const VK_ESCAPE: u16 = 0x1b;
const VK_PRIOR: u16 = 0x21;
const VK_NEXT: u16 = 0x22;
const VK_END: u16 = 0x23;
const VK_HOME: u16 = 0x24;
const VK_LEFT: u16 = 0x25;
const VK_UP: u16 = 0x26;
const VK_RIGHT: u16 = 0x27;
const VK_DOWN: u16 = 0x28;
const VK_INSERT: u16 = 0x2d;
const VK_DELETE: u16 = 0x2e;

static ATTACH_SIGNAL_EXIT: AtomicBool = AtomicBool::new(false);

pub struct ForegroundTerminal {
    input_reader: TerminalInput,
    input: Option<ConsoleHandleMode>,
    output: Option<ConsoleHandleMode>,
}

impl ForegroundTerminal {
    pub fn enter() -> Result<Self, String> {
        let input_handle = interactive_console_input_handle()?;
        let output_handle = interactive_console_output_handle()?;

        let (input_reader, input) = match console_mode(input_handle.handle)? {
            Some(original) => {
                let raw = (original | ENABLE_WINDOW_INPUT)
                    & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT | ENABLE_MOUSE_INPUT);
                set_console_mode(input_handle.handle, raw)?;
                let vt_input = set_console_mode(input_handle.handle, raw | ENABLE_VIRTUAL_TERMINAL_INPUT).is_ok();
                let input_reader = TerminalInput::Console {
                    handle: input_handle.handle,
                    vt_input,
                    pending_high_surrogate: None,
                    pending_bytes: VecDeque::new(),
                };
                (input_reader, Some(ConsoleHandleMode { handle: input_handle, original }))
            }
            None => (TerminalInput::ByteStream { handle: input_handle.handle }, None),
        };

        let output = match console_mode(output_handle.handle)? {
            Some(original) => {
                set_console_mode(output_handle.handle, original | ENABLE_VIRTUAL_TERMINAL_PROCESSING)?;
                Some(ConsoleHandleMode { handle: output_handle, original })
            }
            None => None,
        };

        Ok(Self { input_reader, input, output })
    }

    pub fn read_input(&mut self, timeout: Duration, buf: &mut [u8]) -> io::Result<Option<usize>> {
        if buf.is_empty() {
            return Ok(Some(0));
        }
        self.input_reader.read(timeout, buf)
    }
}

impl Drop for ForegroundTerminal {
    fn drop(&mut self) {
        if let Some(state) = self.input.as_ref() {
            let _ = set_console_mode(state.handle.handle, state.original);
        }
        if let Some(state) = self.output.as_ref() {
            let _ = set_console_mode(state.handle.handle, state.original);
        }
    }
}

pub struct AttachSignalHandlers {
    installed: bool,
}

impl AttachSignalHandlers {
    pub fn install() -> Result<Self, String> {
        ATTACH_SIGNAL_EXIT.store(false, Ordering::SeqCst);
        let ok = unsafe { SetConsoleCtrlHandler(Some(attach_console_handler), 1) };
        if ok == 0 {
            Err(last_error("SetConsoleCtrlHandler"))
        } else {
            Ok(Self { installed: true })
        }
    }
}

impl Drop for AttachSignalHandlers {
    fn drop(&mut self) {
        ATTACH_SIGNAL_EXIT.store(false, Ordering::SeqCst);
        if self.installed {
            unsafe {
                SetConsoleCtrlHandler(Some(attach_console_handler), 0);
            }
        }
    }
}

unsafe extern "system" fn attach_console_handler(ctrl_type: u32) -> i32 {
    match ctrl_type {
        CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT => {
            ATTACH_SIGNAL_EXIT.store(true, Ordering::SeqCst);
            1
        }
        _ => 0,
    }
}

pub fn attach_signal_exit_requested() -> bool {
    ATTACH_SIGNAL_EXIT.load(Ordering::SeqCst)
}

pub fn stdout_is_tty() -> Result<bool, String> {
    let handle = std_handle(STD_OUTPUT_HANDLE)?;
    Ok(console_mode(handle)?.is_some())
}

enum TerminalInput {
    Console { handle: HANDLE, vt_input: bool, pending_high_surrogate: Option<u16>, pending_bytes: VecDeque<u8> },
    ByteStream { handle: HANDLE },
}

impl TerminalInput {
    fn read(&mut self, timeout: Duration, buf: &mut [u8]) -> io::Result<Option<usize>> {
        match self {
            Self::Console { handle, vt_input, pending_high_surrogate, pending_bytes } => {
                read_console_input(*handle, *vt_input, pending_high_surrogate, pending_bytes, timeout, buf)
            }
            Self::ByteStream { handle } => read_byte_stream(*handle, buf).map(Some),
        }
    }
}

fn read_byte_stream(handle: HANDLE, buf: &mut [u8]) -> io::Result<usize> {
    let mut read = 0;
    let ok = unsafe { ReadFile(handle, buf.as_mut_ptr(), buf.len() as u32, &mut read, std::ptr::null_mut()) };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(read as usize)
    }
}

fn read_console_input(
    handle: HANDLE,
    vt_input: bool,
    pending_high_surrogate: &mut Option<u16>,
    pending_bytes: &mut VecDeque<u8>,
    timeout: Duration,
    buf: &mut [u8],
) -> io::Result<Option<usize>> {
    if !pending_bytes.is_empty() {
        return Ok(Some(drain_pending_bytes(pending_bytes, buf)));
    }

    if !wait_for_handle(handle, timeout)? {
        return Ok(None);
    }

    loop {
        let mut records = [INPUT_RECORD::default(); 16];
        let mut read = 0;
        let ok = unsafe { ReadConsoleInputW(handle, records.as_mut_ptr(), records.len() as u32, &mut read) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }

        let mut out = Vec::new();
        for record in records.iter().take(read as usize) {
            match record.EventType as u32 {
                KEY_EVENT => {
                    let key = unsafe { record.Event.KeyEvent };
                    if key.bKeyDown == 0 {
                        continue;
                    }
                    let repeat = key.wRepeatCount.max(1);
                    for _ in 0..repeat {
                        let unicode = unsafe { key.uChar.UnicodeChar };
                        if vt_input && unicode != 0 {
                            append_unicode_char(unicode, pending_high_surrogate, &mut out);
                        } else {
                            append_key_event_bytes(key.wVirtualKeyCode, unicode, pending_high_surrogate, &mut out);
                        }
                    }
                }
                WINDOW_BUFFER_SIZE_EVENT => {}
                _ => {}
            }
            if out.len() >= buf.len() {
                break;
            }
        }

        if !out.is_empty() {
            let n = out.len().min(buf.len());
            buf[..n].copy_from_slice(&out[..n]);
            pending_bytes.extend(out[n..].iter().copied());
            return Ok(Some(n));
        }

        if !wait_for_handle(handle, Duration::ZERO)? {
            return Ok(None);
        }
    }
}

fn drain_pending_bytes(pending: &mut VecDeque<u8>, buf: &mut [u8]) -> usize {
    let n = pending.len().min(buf.len());
    for slot in buf.iter_mut().take(n) {
        *slot = pending.pop_front().expect("pending byte available");
    }
    n
}

fn append_key_event_bytes(virtual_key: u16, unicode: u16, pending_high_surrogate: &mut Option<u16>, out: &mut Vec<u8>) {
    match virtual_key {
        VK_RETURN => out.push(b'\r'),
        VK_BACK => out.push(0x08),
        VK_TAB => out.push(b'\t'),
        VK_ESCAPE => out.push(0x1b),
        VK_UP => out.extend_from_slice(b"\x1b[A"),
        VK_DOWN => out.extend_from_slice(b"\x1b[B"),
        VK_RIGHT => out.extend_from_slice(b"\x1b[C"),
        VK_LEFT => out.extend_from_slice(b"\x1b[D"),
        VK_HOME => out.extend_from_slice(b"\x1b[H"),
        VK_END => out.extend_from_slice(b"\x1b[F"),
        VK_INSERT => out.extend_from_slice(b"\x1b[2~"),
        VK_DELETE => out.extend_from_slice(b"\x1b[3~"),
        VK_PRIOR => out.extend_from_slice(b"\x1b[5~"),
        VK_NEXT => out.extend_from_slice(b"\x1b[6~"),
        _ if unicode != 0 => {
            append_unicode_char(unicode, pending_high_surrogate, out);
        }
        _ => {}
    }
}

fn append_unicode_char(unicode: u16, pending_high_surrogate: &mut Option<u16>, out: &mut Vec<u8>) {
    if unicode == 0 {
        return;
    }

    if (0xd800..=0xdbff).contains(&unicode) {
        *pending_high_surrogate = Some(unicode);
        return;
    }

    let units = if let Some(high) = pending_high_surrogate.take() { vec![high, unicode] } else { vec![unicode] };
    for decoded in char::decode_utf16(units) {
        let ch = decoded.unwrap_or(char::REPLACEMENT_CHARACTER);
        let mut encoded = [0u8; 4];
        out.extend_from_slice(ch.encode_utf8(&mut encoded).as_bytes());
    }
}

fn wait_for_handle(handle: HANDLE, timeout: Duration) -> io::Result<bool> {
    let timeout_ms =
        u32::try_from(timeout.as_millis()).map_err(|_| io::Error::other(format!("poll timeout too large: {}ms", timeout.as_millis())))?;
    match unsafe { WaitForSingleObject(handle, timeout_ms) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        status => Err(io::Error::other(format!("WaitForSingleObject stdin returned {status}"))),
    }
}

pub fn os_terminal_size() -> Option<(u16, u16)> {
    let handle = interactive_console_output_handle().ok()?;
    let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetConsoleScreenBufferInfo(handle.handle, &mut info) };
    if ok == 0 {
        return None;
    }

    let cols = (info.srWindow.Right - info.srWindow.Left + 1).try_into().ok()?;
    let rows = (info.srWindow.Bottom - info.srWindow.Top + 1).try_into().ok()?;
    Some((cols, rows))
}

struct ConsoleHandleMode {
    handle: BorrowedHandle,
    original: u32,
}

struct BorrowedHandle {
    handle: HANDLE,
    close_on_drop: bool,
}

impl BorrowedHandle {
    fn new(handle: HANDLE) -> Self {
        Self { handle, close_on_drop: false }
    }

    fn owned(handle: HANDLE) -> Self {
        Self { handle, close_on_drop: true }
    }
}

impl Drop for BorrowedHandle {
    fn drop(&mut self) {
        if self.close_on_drop {
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }
}

impl std::ops::Deref for BorrowedHandle {
    type Target = HANDLE;

    fn deref(&self) -> &Self::Target {
        &self.handle
    }
}

fn console_input_handle() -> Result<BorrowedHandle, String> {
    open_console_handle("CONIN$")
}

fn console_output_handle() -> Result<BorrowedHandle, String> {
    open_console_handle("CONOUT$")
}

fn interactive_console_input_handle() -> Result<BorrowedHandle, String> {
    let std = std_handle(STD_INPUT_HANDLE)?;
    if console_mode(std)?.is_none() {
        return Ok(BorrowedHandle::new(std));
    }
    console_input_handle().or_else(|_| Ok(BorrowedHandle::new(std)))
}

fn interactive_console_output_handle() -> Result<BorrowedHandle, String> {
    let std = std_handle(STD_OUTPUT_HANDLE)?;
    if console_mode(std)?.is_none() {
        return Ok(BorrowedHandle::new(std));
    }
    console_output_handle().or_else(|_| Ok(BorrowedHandle::new(std)))
}

fn open_console_handle(name: &str) -> Result<BorrowedHandle, String> {
    let wide = wide_null(name);
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        Err(last_error(&format!("CreateFileW {name}")))
    } else {
        Ok(BorrowedHandle::owned(handle))
    }
}

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn std_handle(which: u32) -> Result<HANDLE, String> {
    let handle = unsafe { GetStdHandle(which) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        Err(last_error("GetStdHandle"))
    } else {
        Ok(handle)
    }
}

fn console_mode(handle: HANDLE) -> Result<Option<u32>, String> {
    let mut mode = 0;
    let ok = unsafe { GetConsoleMode(handle, &mut mode) };
    if ok == 0 {
        Ok(None)
    } else {
        Ok(Some(mode))
    }
}

fn set_console_mode(handle: HANDLE, mode: u32) -> Result<(), String> {
    let ok = unsafe { SetConsoleMode(handle, mode) };
    if ok == 0 {
        Err(last_error("SetConsoleMode"))
    } else {
        Ok(())
    }
}

fn last_error(operation: &str) -> String {
    let code = unsafe { GetLastError() };
    format!("{operation} failed with Windows error {code}")
}
