use std::{
    ffi::c_void,
    io,
    mem::{size_of, zeroed},
    path::PathBuf,
    ptr::{null, null_mut},
    thread,
    time::Duration,
};

use windows_sys::Win32::{
    Foundation::{CloseHandle, GetLastError, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
    Security::SECURITY_ATTRIBUTES,
    Storage::FileSystem::{ReadFile, WriteFile},
    System::{
        Console::{ClosePseudoConsole, CreatePseudoConsole, GenerateConsoleCtrlEvent, ResizePseudoConsole, COORD, CTRL_C_EVENT, HPCON},
        Pipes::CreatePipe,
        Threading::{
            CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess, InitializeProcThreadAttributeList, TerminateProcess,
            UpdateProcThreadAttribute, WaitForSingleObject, CREATE_NEW_PROCESS_GROUP, EXTENDED_STARTUPINFO_PRESENT,
            LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, STARTF_USESTDHANDLES, STARTUPINFOEXW,
        },
    },
};

use crate::{
    platform::ipc::{handle_has_available_bytes, SessionListener, SessionStream},
    protocol::SignalTarget,
    runtime::SessionMetadata,
};

pub struct PtyChild {
    conpty: HPCON,
    process: HANDLE,
    thread: HANDLE,
    process_id: u32,
    input_read: HANDLE,
    input_write: HANDLE,
    output_read: HANDLE,
    output_write: HANDLE,
}

impl PtyChild {
    pub fn spawn(session: &SessionMetadata) -> Result<Self, String> {
        let pipes = Pipes::new()?;
        let conpty = create_pseudo_console(80, 24, pipes.input_read, pipes.output_write)?;
        let process = spawn_with_conpty(&windows_shell_command(session), conpty, session.cwd.as_ref())?;

        Ok(Self {
            conpty,
            process: process.hProcess,
            thread: process.hThread,
            process_id: process.dwProcessId,
            input_read: pipes.input_read,
            input_write: pipes.input_write,
            output_read: pipes.output_read,
            output_write: pipes.output_write,
        })
    }

    pub fn master_fd(&self) -> i32 {
        -1
    }

    pub fn set_nonblocking(&self) -> Result<(), String> {
        Ok(())
    }

    pub fn read_output(&self, buf: &mut [u8]) -> Result<usize, io::Error> {
        let mut read = 0;
        let ok = unsafe { ReadFile(self.output_read, buf.as_mut_ptr(), buf.len() as u32, &mut read, null_mut()) };
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(read as usize)
        }
    }

    pub fn write_all(&self, bytes: &[u8]) -> Result<(), String> {
        write_handle_all(self.input_write, bytes)
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), String> {
        let result = unsafe { ResizePseudoConsole(self.conpty, COORD { X: cols as i16, Y: rows as i16 }) };
        if result < 0 {
            Err(format!("ResizePseudoConsole failed with HRESULT 0x{result:08x}"))
        } else {
            Ok(())
        }
    }

    pub fn exited(&self) -> Result<Option<WindowsExitStatus>, String> {
        match unsafe { WaitForSingleObject(self.process, 0) } {
            WAIT_OBJECT_0 => {
                let mut code = 0;
                let ok = unsafe { GetExitCodeProcess(self.process, &mut code) };
                if ok == 0 {
                    Err(last_error("GetExitCodeProcess"))
                } else {
                    Ok(Some(WindowsExitStatus { code }))
                }
            }
            WAIT_TIMEOUT => Ok(None),
            status => Err(format!("unexpected WaitForSingleObject status {status}")),
        }
    }

    pub fn leader_pid(&self) -> u32 {
        self.process_id
    }

    pub fn foreground_pgid(&self) -> Option<u32> {
        None
    }

    pub fn leader_cwd(&self) -> Option<PathBuf> {
        None
    }

    pub fn foreground_cwd(&self) -> Option<PathBuf> {
        None
    }

    pub fn dispatch_signal(&self, signal: i32, _target: SignalTarget) -> Result<(), String> {
        match signal {
            2 => {
                let ok = unsafe { GenerateConsoleCtrlEvent(CTRL_C_EVENT, self.process_id) };
                if ok == 0 {
                    Err(last_error("GenerateConsoleCtrlEvent"))
                } else {
                    Ok(())
                }
            }
            9 | 15 => {
                let ok = unsafe { TerminateProcess(self.process, signal as u32) };
                if ok == 0 {
                    Err(last_error("TerminateProcess"))
                } else {
                    Ok(())
                }
            }
            _ => Err(format!("signal {signal} is not supported on Windows")),
        }
    }

    pub(crate) fn output_available(&self) -> Result<bool, String> {
        handle_has_available_bytes(self.output_read)
    }
}

impl Drop for PtyChild {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.input_write);
            CloseHandle(self.input_read);
            CloseHandle(self.output_read);
            CloseHandle(self.output_write);
            CloseHandle(self.thread);
            CloseHandle(self.process);
            ClosePseudoConsole(self.conpty);
        }
    }
}

pub struct WindowsExitStatus {
    code: u32,
}

pub struct PollResult {
    pub listener_readable: bool,
    pub client_readable: bool,
    pub client_writable: bool,
    pub pty_readable: bool,
}

pub fn poll_session_ready(
    _listener: &SessionListener,
    client: Option<&SessionStream>,
    client_needs_write: bool,
    pty_child: &PtyChild,
    timeout_ms: i32,
) -> Result<PollResult, String> {
    let client_readable = client.map(SessionStream::has_available_bytes).transpose()?.unwrap_or(false);
    let pty_readable = pty_child.output_available()?;
    let listener_readable = true;

    if !client_readable && !pty_readable && timeout_ms > 0 {
        thread::sleep(Duration::from_millis(timeout_ms as u64));
    }

    Ok(PollResult { listener_readable, client_readable, client_writable: client_needs_write, pty_readable })
}

pub fn exit_code_from_wait_status(status: &WindowsExitStatus) -> i32 {
    status.code as i32
}

struct Pipes {
    input_read: HANDLE,
    input_write: HANDLE,
    output_read: HANDLE,
    output_write: HANDLE,
}

impl Pipes {
    fn new() -> Result<Self, String> {
        let attrs =
            SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: null_mut(), bInheritHandle: 0 };

        let mut input_read = null_mut();
        let mut input_write = null_mut();
        let mut output_read = null_mut();
        let mut output_write = null_mut();

        unsafe {
            if CreatePipe(&mut input_read, &mut input_write, &attrs, 0) == 0 {
                return Err(last_error("CreatePipe input"));
            }
            if CreatePipe(&mut output_read, &mut output_write, &attrs, 0) == 0 {
                CloseHandle(input_read);
                CloseHandle(input_write);
                return Err(last_error("CreatePipe output"));
            }
        }

        Ok(Self { input_read, input_write, output_read, output_write })
    }
}

fn create_pseudo_console(cols: u16, rows: u16, input: HANDLE, output: HANDLE) -> Result<HPCON, String> {
    let mut conpty = 0;
    let result = unsafe { CreatePseudoConsole(COORD { X: cols as i16, Y: rows as i16 }, input, output, 0, &mut conpty) };

    if result < 0 {
        Err(format!("CreatePseudoConsole failed with HRESULT 0x{result:08x}"))
    } else {
        Ok(conpty)
    }
}

fn spawn_with_conpty(command_line: &str, conpty: HPCON, cwd: Option<&PathBuf>) -> Result<PROCESS_INFORMATION, String> {
    let mut attr_size = 0;
    unsafe {
        InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut attr_size);
    }

    let mut attr_storage = vec![0u8; attr_size];
    let attr_list = attr_storage.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;

    let initialized = unsafe { InitializeProcThreadAttributeList(attr_list, 1, 0, &mut attr_size) };
    if initialized == 0 {
        return Err(last_error("InitializeProcThreadAttributeList"));
    }

    let updated = unsafe {
        UpdateProcThreadAttribute(
            attr_list,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            conpty as *const c_void,
            size_of::<HPCON>(),
            null_mut(),
            null(),
        )
    };
    if updated == 0 {
        unsafe {
            DeleteProcThreadAttributeList(attr_list);
        }
        return Err(last_error("UpdateProcThreadAttribute"));
    }

    let mut startup_info: STARTUPINFOEXW = unsafe { zeroed() };
    startup_info.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup_info.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup_info.StartupInfo.hStdInput = null_mut();
    startup_info.StartupInfo.hStdOutput = null_mut();
    startup_info.StartupInfo.hStdError = null_mut();
    startup_info.lpAttributeList = attr_list;

    let mut process_info: PROCESS_INFORMATION = unsafe { zeroed() };
    let mut command_line = wide_null(command_line);
    let cwd = cwd.map(|path| wide_null(&path.to_string_lossy()));
    let cwd_ptr = cwd.as_ref().map(|value| value.as_ptr()).unwrap_or_else(null);

    let created = unsafe {
        CreateProcessW(
            null(),
            command_line.as_mut_ptr(),
            null(),
            null(),
            0,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_NEW_PROCESS_GROUP,
            null(),
            cwd_ptr,
            &startup_info as *const STARTUPINFOEXW as *const _,
            &mut process_info,
        )
    };

    unsafe {
        DeleteProcThreadAttributeList(attr_list);
    }

    if created == 0 {
        Err(last_error("CreateProcessW"))
    } else {
        Ok(process_info)
    }
}

fn windows_shell_command(session: &SessionMetadata) -> String {
    match &session.cmd {
        Some(cmd) => format!("powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -Command {cmd}"),
        None => "powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass".to_string(),
    }
}

fn write_handle_all(handle: HANDLE, mut bytes: &[u8]) -> Result<(), String> {
    while !bytes.is_empty() {
        let mut written = 0;
        let ok = unsafe { WriteFile(handle, bytes.as_ptr(), bytes.len().min(u32::MAX as usize) as u32, &mut written, null_mut()) };
        if ok == 0 {
            return Err(last_error("WriteFile"));
        }
        if written == 0 {
            return Err("WriteFile wrote zero bytes".to_string());
        }
        bytes = &bytes[written as usize..];
    }
    Ok(())
}

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn last_error(operation: &str) -> String {
    let code = unsafe { GetLastError() };
    format!("{operation} failed with Windows error {code}")
}
