use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use windows_sys::Win32::{
    Foundation::{GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::{
        Console::{
            GetConsoleMode, GetConsoleScreenBufferInfo, GetStdHandle, SetConsoleCtrlHandler, SetConsoleMode, CONSOLE_SCREEN_BUFFER_INFO,
            CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
            ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        },
        Threading::WaitForSingleObject,
    },
};

static ATTACH_SIGNAL_EXIT: AtomicBool = AtomicBool::new(false);

pub struct TerminalModeGuard {
    input: Option<(HANDLE, u32)>,
    output: Option<(HANDLE, u32)>,
}

impl TerminalModeGuard {
    pub fn activate() -> Result<Self, String> {
        let input_handle = std_handle(STD_INPUT_HANDLE)?;
        let output_handle = std_handle(STD_OUTPUT_HANDLE)?;

        let input = match console_mode(input_handle)? {
            Some(original) => {
                let raw = (original | ENABLE_VIRTUAL_TERMINAL_INPUT) & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT);
                set_console_mode(input_handle, raw)?;
                Some((input_handle, original))
            }
            None => None,
        };

        let output = match console_mode(output_handle)? {
            Some(original) => {
                set_console_mode(output_handle, original | ENABLE_VIRTUAL_TERMINAL_PROCESSING)?;
                Some((output_handle, original))
            }
            None => None,
        };

        Ok(Self { input, output })
    }
}

impl Drop for TerminalModeGuard {
    fn drop(&mut self) {
        if let Some((handle, mode)) = self.input {
            let _ = set_console_mode(handle, mode);
        }
        if let Some((handle, mode)) = self.output {
            let _ = set_console_mode(handle, mode);
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

pub fn poll_stdin_readable(timeout: Duration) -> Result<bool, String> {
    let handle = std_handle(STD_INPUT_HANDLE)?;
    if console_mode(handle)?.is_none() {
        return Ok(true);
    }

    let timeout_ms = u32::try_from(timeout.as_millis()).map_err(|_| format!("poll timeout too large: {}ms", timeout.as_millis()))?;
    match unsafe { WaitForSingleObject(handle, timeout_ms) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        status => Err(format!("WaitForSingleObject stdin returned {status}")),
    }
}

pub fn os_terminal_size() -> Option<(u16, u16)> {
    let handle = std_handle(STD_OUTPUT_HANDLE).ok()?;
    let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetConsoleScreenBufferInfo(handle, &mut info) };
    if ok == 0 {
        return None;
    }

    let cols = (info.srWindow.Right - info.srWindow.Left + 1).try_into().ok()?;
    let rows = (info.srWindow.Bottom - info.srWindow.Top + 1).try_into().ok()?;
    Some((cols, rows))
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
