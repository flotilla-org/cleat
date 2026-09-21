use std::{ffi::OsStr, io, os::windows::ffi::OsStrExt, path::Path, ptr};

use windows_sys::Win32::{
    Foundation::CloseHandle,
    System::Threading::{CreateProcessW, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS, PROCESS_INFORMATION, STARTUPINFOW},
};

pub(super) fn spawn(exe: &Path, root: &Path, daemon_name: &str) -> io::Result<()> {
    let application: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut command = Vec::new();
    for arg in [
        exe.as_os_str(),
        OsStr::new("--runtime-root"),
        root.as_os_str(),
        OsStr::new("--server"),
        OsStr::new(daemon_name),
        OsStr::new("serve"),
    ] {
        if !command.is_empty() {
            command.push(b' ' as u16);
        }
        append_argument(&mut command, arg)?;
    }
    command.push(0);
    // Command::spawn redirects standard streams but still inherits other
    // inheritable handles on Windows, including the launcher's captured output
    // pipe. The daemon needs its environment, not any of those handles or the
    // launcher's console. No handle inheritance also leaves stdio disconnected.
    // SAFETY: application/command are NUL-terminated UTF-16 buffers; startup and
    // process are valid initialized structures. No borrowed handles are passed.
    unsafe {
        let startup = STARTUPINFOW { cb: size_of::<STARTUPINFOW>() as u32, ..std::mem::zeroed() };
        let mut process: PROCESS_INFORMATION = std::mem::zeroed();
        if CreateProcessW(
            application.as_ptr(),
            command.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            0,
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP,
            ptr::null(),
            ptr::null(),
            &startup,
            &mut process,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        CloseHandle(process.hThread);
        CloseHandle(process.hProcess);
    }
    Ok(())
}

// Quote directly for the Windows argv parser, not cmd.exe. Preserve UTF-16
// paths (including unpaired surrogates) and double backslashes before quotes.
fn append_argument(command: &mut Vec<u16>, arg: &OsStr) -> io::Result<()> {
    command.push(b'"' as u16);
    let mut backslashes = 0;
    for unit in arg.encode_wide() {
        match unit {
            0 => return Err(io::Error::new(io::ErrorKind::InvalidInput, "NUL in daemon argument")),
            92 => backslashes += 1,
            34 => {
                command.extend(std::iter::repeat_n(92, backslashes * 2 + 1));
                command.push(unit);
                backslashes = 0;
            }
            _ => {
                command.extend(std::iter::repeat_n(92, backslashes));
                command.push(unit);
                backslashes = 0;
            }
        }
    }
    command.extend(std::iter::repeat_n(92, backslashes * 2));
    command.push(b'"' as u16);
    Ok(())
}
