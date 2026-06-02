use std::{
    ffi::CString,
    io,
    os::fd::{AsRawFd, BorrowedFd, IntoRawFd, RawFd},
    path::PathBuf,
};

use nix::{
    errno::Errno,
    fcntl::{fcntl, FcntlArg, OFlag},
    poll::{poll, PollFd, PollFlags, PollTimeout},
    pty::{forkpty, ForkptyResult},
    sys::{
        signal::{killpg, Signal},
        wait::{waitpid, WaitPidFlag, WaitStatus},
    },
    unistd::{chdir, execvp, read as nix_read, tcgetpgrp, write as nix_write, Pid},
};

use crate::{protocol::SignalTarget, runtime::SessionMetadata};

const STRIP_ENV_VARS: &[&str] = &["SSH_TTY", "SSH_CONNECTION", "SSH_CLIENT"];

pub struct PtyChild {
    master_fd: RawFd,
    pid: Pid,
}

impl PtyChild {
    pub fn spawn(session: &SessionMetadata) -> Result<Self, String> {
        // SAFETY: `forkpty` creates a child attached to a new PTY; parent receives the owned master fd.
        let result = unsafe { forkpty(None, None) }.map_err(|err| format!("forkpty failed: {err}"))?;
        match result {
            ForkptyResult::Parent { master, child } => Ok(Self { master_fd: master.into_raw_fd(), pid: child }),
            ForkptyResult::Child => {
                if let Some(cwd) = &session.cwd {
                    let _ = chdir(cwd);
                }
                for key in STRIP_ENV_VARS {
                    // SAFETY: child process is single-threaded here, before exec, so environment mutation is safe.
                    unsafe {
                        std::env::remove_var(key);
                    }
                }
                let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
                let shell_c = CString::new(shell.clone()).map_err(|_| "shell contains interior nul".to_string())?;
                let mut args = vec![shell_c.clone()];
                if let Some(cmd) = &session.cmd {
                    args.push(CString::new("-lc").map_err(|_| "invalid -lc".to_string())?);
                    args.push(CString::new(cmd.as_str()).map_err(|_| "cmd contains interior nul".to_string())?);
                }
                let _ = execvp(&shell_c, &args);
                std::process::exit(127);
            }
        }
    }

    pub fn master_fd(&self) -> RawFd {
        self.master_fd
    }

    pub fn set_nonblocking(&self) -> Result<(), String> {
        set_nonblocking(self.master_fd)
    }

    pub fn read_output(&self, buf: &mut [u8]) -> Result<usize, io::Error> {
        read_fd(self.master_fd, buf)
    }

    pub fn write_all(&self, bytes: &[u8]) -> Result<(), String> {
        write_fd_all(self.master_fd, bytes)
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), String> {
        resize_pty(self.master_fd, cols, rows)
    }

    pub fn exited(&self) -> Result<Option<WaitStatus>, String> {
        child_exited(self.pid)
    }

    pub fn leader_pid(&self) -> u32 {
        self.pid.as_raw() as u32
    }

    pub fn foreground_pgid(&self) -> Option<u32> {
        tcgetpgrp(borrow_raw(self.master_fd)).ok().map(|pid| pid.as_raw() as u32)
    }

    pub fn leader_cwd(&self) -> Option<PathBuf> {
        resolve_cwd(self.leader_pid())
    }

    pub fn foreground_cwd(&self) -> Option<PathBuf> {
        self.foreground_pgid().and_then(resolve_cwd)
    }

    pub fn dispatch_signal(&self, signal: i32, target: SignalTarget) -> Result<(), String> {
        let signal = Signal::try_from(signal).map_err(|err| format!("invalid signal number: {err}"))?;

        match target {
            SignalTarget::Foreground => {
                let fg_pgid = tcgetpgrp(borrow_raw(self.master_fd)).map_err(|err| format!("tcgetpgrp: {err}"))?;
                killpg(fg_pgid, signal).map_err(|err| format!("killpg: {err}"))
            }
            SignalTarget::Leader => nix::sys::signal::kill(self.pid, signal).map_err(|err| format!("kill: {err}")),
            SignalTarget::Tree => Err("tree signal target is not yet implemented".to_string()),
        }
    }
}

pub struct PollResult {
    pub listener_readable: bool,
    pub client_readable: bool,
    pub client_writable: bool,
    pub pty_readable: bool,
}

pub fn poll_session_ready(
    listener_fd: RawFd,
    client_fd: Option<RawFd>,
    client_needs_write: bool,
    pty_fd: RawFd,
    timeout_ms: i32,
) -> Result<PollResult, String> {
    let listener_borrowed = borrow_raw(listener_fd);
    let pty_borrowed = borrow_raw(pty_fd);
    let mut fds = vec![PollFd::new(listener_borrowed, PollFlags::POLLIN), PollFd::new(pty_borrowed, PollFlags::POLLIN)];
    let client_index = if let Some(fd) = client_fd {
        let client_borrowed = borrow_raw(fd);
        let mut flags = PollFlags::POLLIN;
        if client_needs_write {
            flags |= PollFlags::POLLOUT;
        }
        fds.push(PollFd::new(client_borrowed, flags));
        Some(fds.len() - 1)
    } else {
        None
    };

    poll(&mut fds, PollTimeout::try_from(timeout_ms).map_err(|err| format!("invalid poll timeout: {err}"))?)
        .map_err(|err| format!("poll daemon fds: {err}"))?;

    Ok(PollResult {
        listener_readable: has_pollin(&fds[0]),
        pty_readable: has_pollin(&fds[1]),
        client_readable: client_index.map(|index| has_pollin(&fds[index])).unwrap_or(false),
        client_writable: client_index.map(|index| has_pollout(&fds[index])).unwrap_or(false),
    })
}

pub fn exit_code_from_wait_status(status: &WaitStatus) -> i32 {
    match status {
        WaitStatus::Exited(_, code) => *code,
        WaitStatus::Signaled(_, sig, _) => 128 + *sig as i32,
        _ => 1,
    }
}

pub fn stream_fd(stream: &std::os::unix::net::UnixStream) -> RawFd {
    stream.as_raw_fd()
}

pub fn listener_fd(listener: &std::os::unix::net::UnixListener) -> RawFd {
    listener.as_raw_fd()
}

fn borrow_raw(fd: RawFd) -> BorrowedFd<'static> {
    // SAFETY: callers only borrow fds owned by this process for the duration of immediate syscalls.
    unsafe { BorrowedFd::borrow_raw(fd) }
}

fn set_nonblocking(fd: RawFd) -> Result<(), String> {
    let flags = fcntl(borrow_raw(fd), FcntlArg::F_GETFL).map_err(|err| format!("fcntl F_GETFL failed: {err}"))?;
    let mut oflags = OFlag::from_bits_retain(flags);
    oflags.insert(OFlag::O_NONBLOCK);
    fcntl(borrow_raw(fd), FcntlArg::F_SETFL(oflags)).map_err(|err| format!("fcntl F_SETFL failed: {err}"))?;
    Ok(())
}

fn read_fd(fd: RawFd, buf: &mut [u8]) -> Result<usize, io::Error> {
    nix_read(borrow_raw(fd), buf).map_err(io::Error::from)
}

fn write_fd_all(fd: RawFd, mut bytes: &[u8]) -> Result<(), String> {
    while !bytes.is_empty() {
        match nix_write(borrow_raw(fd), bytes) {
            Ok(written) => bytes = &bytes[written..],
            Err(err) => {
                let err = io::Error::from(err);
                if err.kind() == io::ErrorKind::WouldBlock {
                    wait_for_writable(fd)?;
                    continue;
                }
                return Err(format!("write pty input: {err}"));
            }
        }
    }
    Ok(())
}

fn wait_for_writable(fd: RawFd) -> Result<(), String> {
    let mut fds = [PollFd::new(borrow_raw(fd), PollFlags::POLLOUT)];
    poll(&mut fds, PollTimeout::NONE).map_err(|err| format!("poll writable pty fd: {err}"))?;
    Ok(())
}

fn resize_pty(fd: RawFd, cols: u16, rows: u16) -> Result<(), String> {
    let winsize = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: ioctl updates the window size for a valid PTY master fd using a properly initialized winsize.
    let rc = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &winsize) };
    if rc == 0 {
        Ok(())
    } else {
        Err(format!("resize pty: {}", io::Error::last_os_error()))
    }
}

fn child_exited(child_pid: Pid) -> Result<Option<WaitStatus>, String> {
    match waitpid(child_pid, Some(WaitPidFlag::WNOHANG)) {
        Ok(WaitStatus::StillAlive) => Ok(None),
        Ok(status) => Ok(Some(status)),
        Err(Errno::ECHILD) => Ok(None),
        Err(err) => Err(format!("waitpid failed: {err}")),
    }
}

fn has_pollin(fd: &PollFd<'_>) -> bool {
    fd.revents().map(|flags| flags.contains(PollFlags::POLLIN)).unwrap_or(false)
}

fn has_pollout(fd: &PollFd<'_>) -> bool {
    fd.revents().map(|flags| flags.contains(PollFlags::POLLOUT)).unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn resolve_cwd(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

#[cfg(target_os = "macos")]
fn resolve_cwd(pid: u32) -> Option<PathBuf> {
    use std::mem;

    // SAFETY: proc_pidinfo is called with a valid output buffer and documented arguments.
    unsafe {
        let mut vnode_info: libc::proc_vnodepathinfo = mem::zeroed();
        let size = mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
        let ret =
            libc::proc_pidinfo(pid as libc::c_int, libc::PROC_PIDVNODEPATHINFO, 0, &mut vnode_info as *mut _ as *mut libc::c_void, size);
        if ret <= 0 {
            return None;
        }
        let cstr = std::ffi::CStr::from_ptr(vnode_info.pvi_cdir.vip_path.as_ptr() as *const libc::c_char);
        let path = PathBuf::from(cstr.to_string_lossy().into_owned());
        if path.as_os_str().is_empty() {
            None
        } else {
            Some(path)
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn resolve_cwd(_pid: u32) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use nix::{sys::wait::WaitStatus, unistd::Pid};

    #[test]
    fn exit_code_from_normal_exit() {
        let status = WaitStatus::Exited(Pid::from_raw(1), 42);
        assert_eq!(super::exit_code_from_wait_status(&status), 42);
    }

    #[test]
    fn exit_code_from_zero_exit() {
        let status = WaitStatus::Exited(Pid::from_raw(1), 0);
        assert_eq!(super::exit_code_from_wait_status(&status), 0);
    }

    #[test]
    fn exit_code_from_signal_is_128_plus_signal() {
        let status = WaitStatus::Signaled(Pid::from_raw(1), nix::sys::signal::Signal::SIGTERM, false);
        assert_eq!(super::exit_code_from_wait_status(&status), 128 + libc::SIGTERM);
    }
}
