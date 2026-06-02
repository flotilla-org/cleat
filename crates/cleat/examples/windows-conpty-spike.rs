#[cfg(windows)]
mod spike {
    use std::error::Error;
    use std::ffi::c_void;
    use std::fs::File;
    use std::io::{Read, Write};
    use std::mem::{size_of, zeroed};
    use std::os::windows::io::FromRawHandle;
    use std::ptr::{null, null_mut};
    use std::thread;

    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::System::Console::{ClosePseudoConsole, CreatePseudoConsole, COORD, HPCON};
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess, InitializeProcThreadAttributeList, UpdateProcThreadAttribute,
        WaitForSingleObject, EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
        PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    };

    const READY_MARKER: &str = "cleat-conpty-ready";

    pub fn run() -> Result<(), Box<dyn Error>> {
        run_probe("command-line", "cmd.exe /D /Q /C echo cleat-conpty-ready && exit /b 7", None)?;
        run_probe("interactive", "cmd.exe /D /Q /K", Some(b"echo cleat-conpty-ready\r\nexit /b 7\r\n"))?;

        Ok(())
    }

    fn run_probe(name: &str, command_line: &str, input_bytes: Option<&[u8]>) -> Result<(), Box<dyn Error>> {
        let pipes = Pipes::new()?;
        let conpty = PseudoConsole::new(80, 24, pipes.input_read, pipes.output_write)?;

        let child = ChildProcess::spawn_with_conpty(command_line, conpty.handle)?;

        let mut input = input_bytes.map(|_| unsafe { File::from_raw_handle(pipes.input_write) });
        let mut output = unsafe { File::from_raw_handle(pipes.output_read) };

        let reader = thread::spawn(move || {
            let mut transcript = Vec::new();
            output.read_to_end(&mut transcript).map(|_| transcript)
        });

        if let Some(bytes) = input_bytes {
            thread::sleep(std::time::Duration::from_millis(250));
            let input = input.as_mut().expect("input file is created when input bytes exist");
            input.write_all(bytes)?;
            input.flush()?;
        }

        let wait_status = unsafe { WaitForSingleObject(child.process, 5_000) };
        drop(input);
        unsafe {
            if input_bytes.is_none() {
                CloseHandle(pipes.input_write);
            }
            CloseHandle(pipes.input_read);
            CloseHandle(pipes.output_write);
        }
        if wait_status == WAIT_TIMEOUT {
            return Err(format!("{name} probe timed out waiting for ConPTY child process").into());
        }
        if wait_status != WAIT_OBJECT_0 {
            return Err(format!("{name} probe got unexpected WaitForSingleObject status {wait_status}").into());
        }

        let mut exit_code = 0;
        let got_exit_code = unsafe { GetExitCodeProcess(child.process, &mut exit_code) };
        if got_exit_code == 0 {
            return Err(last_error("GetExitCodeProcess").into());
        }

        drop(conpty);
        drop(child);

        let transcript =
            reader.join().map_err(|_| "reader thread panicked")?.map_err(|err| format!("failed to read ConPTY output: {err}"))?;
        let transcript = String::from_utf8_lossy(&transcript);

        println!("=== {name} ===");
        println!("{transcript}");
        println!("exit_code={exit_code}");

        let normalized_transcript = transcript.replace('\0', "");
        if !normalized_transcript.contains(READY_MARKER) {
            return Err(format!(
                "{name} probe ConPTY transcript did not contain {READY_MARKER:?}: {:?}",
                normalized_transcript.escape_debug().to_string()
            )
            .into());
        }
        if exit_code != 7 {
            return Err(format!("expected exit code 7, got {exit_code}").into());
        }

        Ok(())
    }

    struct Pipes {
        input_read: HANDLE,
        input_write: HANDLE,
        output_read: HANDLE,
        output_write: HANDLE,
    }

    impl Pipes {
        fn new() -> Result<Self, String> {
            let mut attrs = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: null_mut(),
                bInheritHandle: 0,
            };

            let mut input_read = null_mut();
            let mut input_write = null_mut();
            let mut output_read = null_mut();
            let mut output_write = null_mut();

            unsafe {
                if CreatePipe(&mut input_read, &mut input_write, &mut attrs, 0) == 0 {
                    return Err(last_error("CreatePipe input"));
                }
                if CreatePipe(&mut output_read, &mut output_write, &mut attrs, 0) == 0 {
                    CloseHandle(input_read);
                    CloseHandle(input_write);
                    return Err(last_error("CreatePipe output"));
                }
            }

            Ok(Self { input_read, input_write, output_read, output_write })
        }
    }

    struct PseudoConsole {
        handle: HPCON,
    }

    impl PseudoConsole {
        fn new(width: i16, height: i16, input: HANDLE, output: HANDLE) -> Result<Self, String> {
            let mut handle = 0;
            let result = unsafe { CreatePseudoConsole(COORD { X: width, Y: height }, input, output, 0, &mut handle) };

            if result < 0 {
                return Err(format!("CreatePseudoConsole failed with HRESULT 0x{result:08x}"));
            }

            Ok(Self { handle })
        }
    }

    impl Drop for PseudoConsole {
        fn drop(&mut self) {
            unsafe {
                ClosePseudoConsole(self.handle);
            }
        }
    }

    struct ChildProcess {
        process: HANDLE,
        thread: HANDLE,
    }

    impl ChildProcess {
        fn spawn_with_conpty(command_line: &str, conpty: HPCON) -> Result<Self, String> {
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

            let created = unsafe {
                CreateProcessW(
                    null(),
                    command_line.as_mut_ptr(),
                    null(),
                    null(),
                    0,
                    EXTENDED_STARTUPINFO_PRESENT,
                    null(),
                    null(),
                    &startup_info as *const STARTUPINFOEXW as *const _,
                    &mut process_info,
                )
            };

            unsafe {
                DeleteProcThreadAttributeList(attr_list);
            }

            if created == 0 {
                return Err(last_error("CreateProcessW"));
            }

            Ok(Self { process: process_info.hProcess, thread: process_info.hThread })
        }
    }

    impl Drop for ChildProcess {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.thread);
                CloseHandle(self.process);
            }
        }
    }

    fn wide_null(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }

    fn last_error(operation: &str) -> String {
        let code = unsafe { GetLastError() };
        format!("{operation} failed with Windows error {code}")
    }
}

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    spike::run()
}

#[cfg(not(windows))]
fn main() {
    println!("windows-conpty-spike only runs on Windows");
}
