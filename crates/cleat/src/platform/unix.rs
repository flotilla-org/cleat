use std::{
    env,
    ffi::{CString, OsStr, OsString},
    io,
    os::{
        fd::{AsRawFd, BorrowedFd, IntoRawFd, RawFd},
        unix::ffi::OsStrExt,
    },
    path::{Path, PathBuf},
};

#[cfg(target_os = "linux")]
use nix::sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags};
#[cfg(target_os = "macos")]
use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue};
use nix::{
    errno::Errno,
    fcntl::{fcntl, FcntlArg, OFlag},
    poll::{poll, PollFd, PollFlags, PollTimeout},
    pty::{forkpty, ForkptyResult, Winsize},
    sys::{
        signal::{killpg, Signal},
        wait::{waitpid, WaitPidFlag, WaitStatus},
    },
    unistd::{read as nix_read, tcgetpgrp, write as nix_write, Pid},
};

use crate::{
    platform::ipc::{SessionListener, SessionStream},
    protocol::SignalTarget,
    runtime::SessionMetadata,
};

const STRIP_ENV_VARS: &[&str] = &["SSH_TTY", "SSH_CONNECTION", "SSH_CLIENT"];

pub struct PtyChild {
    master_fd: RawFd,
    pid: Pid,
}

impl PtyChild {
    pub fn spawn(session: &SessionMetadata) -> Result<Self, String> {
        let exec_spec = ChildExecSpec::new(session)?;
        let winsize = Winsize { ws_row: session.initial_size.rows, ws_col: session.initial_size.cols, ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: `forkpty` creates a child attached to a new PTY; parent receives the owned master fd.
        let result = unsafe { forkpty(&winsize, None) }.map_err(|err| format!("forkpty failed: {err}"))?;
        match result {
            ForkptyResult::Parent { master, child } => Ok(Self { master_fd: master.into_raw_fd(), pid: child }),
            ForkptyResult::Child => {
                exec_child_or_exit(&exec_spec);
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

    pub fn resize(&self, cols: u16, rows: u16, width_px: u32, height_px: u32) -> Result<(), String> {
        resize_pty(self.master_fd, cols, rows, width_px, height_px)
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
    listener: &SessionListener,
    client: Option<&SessionStream>,
    client_needs_write: bool,
    pty_child: &PtyChild,
    timeout_ms: i32,
) -> Result<PollResult, String> {
    #[cfg(target_os = "macos")]
    {
        poll_session_ready_kqueue(listener, client, client_needs_write, pty_child, timeout_ms)
    }
    #[cfg(target_os = "linux")]
    {
        poll_session_ready_epoll(listener, client, client_needs_write, pty_child, timeout_ms)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        poll_session_ready_poll(listener, client, client_needs_write, pty_child, timeout_ms)
    }
}

#[cfg(target_os = "macos")]
fn poll_session_ready_kqueue(
    listener: &SessionListener,
    client: Option<&SessionStream>,
    client_needs_write: bool,
    pty_child: &PtyChild,
    timeout_ms: i32,
) -> Result<PollResult, String> {
    const TOKEN_LISTENER_READ: isize = 1;
    const TOKEN_PTY_READ: isize = 2;
    const TOKEN_CLIENT_READ: isize = 3;
    const TOKEN_CLIENT_WRITE: isize = 4;

    let kqueue = Kqueue::new().map_err(|err| format!("create daemon kqueue: {err}"))?;
    let mut changes = vec![read_kevent(listener.as_raw_fd(), TOKEN_LISTENER_READ), read_kevent(pty_child.master_fd(), TOKEN_PTY_READ)];
    if let Some(client) = client {
        changes.push(read_kevent(client.as_raw_fd(), TOKEN_CLIENT_READ));
        if client_needs_write {
            changes.push(write_kevent(client.as_raw_fd(), TOKEN_CLIENT_WRITE));
        }
    }

    let mut events = [KEvent::new(0, EventFilter::EVFILT_READ, EvFlags::empty(), FilterFlag::empty(), 0, 0); 4];
    let timeout = kqueue_timeout(timeout_ms);
    let event_count = loop {
        match kqueue.kevent(&changes, &mut events, timeout) {
            Ok(event_count) => break event_count,
            Err(Errno::EINTR) => continue,
            Err(err) => return Err(format!("kqueue daemon fds: {err}")),
        }
    };
    let mut result = PollResult { listener_readable: false, client_readable: false, client_writable: false, pty_readable: false };
    for event in events.iter().take(event_count) {
        if event.flags().contains(EvFlags::EV_ERROR) {
            if event.data() != 0 {
                return Err(format!("kqueue daemon fd registration: {}", Errno::from_raw(event.data() as i32)));
            }
            continue;
        }
        match event.udata() {
            TOKEN_LISTENER_READ => result.listener_readable = true,
            TOKEN_PTY_READ => result.pty_readable = true,
            TOKEN_CLIENT_READ => result.client_readable = true,
            TOKEN_CLIENT_WRITE => result.client_writable = true,
            _ => {}
        }
    }
    Ok(result)
}

#[cfg(target_os = "macos")]
fn read_kevent(fd: RawFd, token: isize) -> KEvent {
    KEvent::new(fd as _, EventFilter::EVFILT_READ, EvFlags::EV_ADD, FilterFlag::empty(), 0, token)
}

#[cfg(target_os = "macos")]
fn write_kevent(fd: RawFd, token: isize) -> KEvent {
    KEvent::new(fd as _, EventFilter::EVFILT_WRITE, EvFlags::EV_ADD, FilterFlag::empty(), 0, token)
}

#[cfg(target_os = "macos")]
fn kqueue_timeout(timeout_ms: i32) -> Option<libc::timespec> {
    if timeout_ms < 0 {
        None
    } else {
        Some(libc::timespec { tv_sec: (timeout_ms / 1000) as libc::time_t, tv_nsec: ((timeout_ms % 1000) * 1_000_000) as _ })
    }
}

#[cfg(target_os = "linux")]
fn poll_session_ready_epoll(
    listener: &SessionListener,
    client: Option<&SessionStream>,
    client_needs_write: bool,
    pty_child: &PtyChild,
    timeout_ms: i32,
) -> Result<PollResult, String> {
    const TOKEN_LISTENER: u64 = 1;
    const TOKEN_PTY: u64 = 2;
    const TOKEN_CLIENT: u64 = 3;

    let epoll = Epoll::new(EpollCreateFlags::EPOLL_CLOEXEC).map_err(|err| format!("create daemon epoll: {err}"))?;
    epoll
        .add(borrow_raw(listener.as_raw_fd()), EpollEvent::new(EpollFlags::EPOLLIN, TOKEN_LISTENER))
        .map_err(|err| format!("register listener with epoll: {err}"))?;
    epoll
        .add(borrow_raw(pty_child.master_fd()), EpollEvent::new(EpollFlags::EPOLLIN, TOKEN_PTY))
        .map_err(|err| format!("register pty with epoll: {err}"))?;
    if let Some(client) = client {
        let mut flags = EpollFlags::EPOLLIN;
        if client_needs_write {
            flags |= EpollFlags::EPOLLOUT;
        }
        epoll
            .add(borrow_raw(client.as_raw_fd()), EpollEvent::new(flags, TOKEN_CLIENT))
            .map_err(|err| format!("register client with epoll: {err}"))?;
    }

    let mut events = [EpollEvent::empty(); 3];
    let timeout = PollTimeout::try_from(timeout_ms).map_err(|err| format!("invalid epoll timeout: {err}"))?;
    let event_count = loop {
        match epoll.wait(&mut events, timeout) {
            Ok(event_count) => break event_count,
            Err(Errno::EINTR) => continue,
            Err(err) => return Err(format!("epoll daemon fds: {err}")),
        }
    };
    let mut result = PollResult { listener_readable: false, client_readable: false, client_writable: false, pty_readable: false };
    for event in events.iter().take(event_count) {
        let flags = event.events();
        match event.data() {
            TOKEN_LISTENER => result.listener_readable = flags.contains(EpollFlags::EPOLLIN),
            TOKEN_PTY => result.pty_readable = flags.contains(EpollFlags::EPOLLIN),
            TOKEN_CLIENT => {
                result.client_readable = flags.contains(EpollFlags::EPOLLIN);
                result.client_writable = client_needs_write && flags.contains(EpollFlags::EPOLLOUT);
            }
            _ => {}
        }
    }
    Ok(result)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn poll_session_ready_poll(
    listener: &SessionListener,
    client: Option<&SessionStream>,
    client_needs_write: bool,
    pty_child: &PtyChild,
    timeout_ms: i32,
) -> Result<PollResult, String> {
    let listener_fd = listener.as_raw_fd();
    let client_fd = client.map(AsRawFd::as_raw_fd);
    let pty_fd = pty_child.master_fd();
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

struct ChildExecSpec {
    program: CString,
    _argv: Vec<CString>,
    _envp: Vec<CString>,
    cwd: Option<CString>,
    argv_ptrs: Vec<*const libc::c_char>,
    envp_ptrs: Vec<*const libc::c_char>,
}

impl ChildExecSpec {
    fn new(session: &SessionMetadata) -> Result<Self, String> {
        let shell = env::var_os("SHELL").unwrap_or_else(|| OsString::from("/bin/sh"));
        let program = cstring_from_os(resolve_shell_program(&shell).as_os_str(), "shell path contains interior nul")?;
        let mut argv = vec![cstring_from_os(&shell, "shell contains interior nul")?];
        if let Some(cmd) = &session.cmd {
            argv.push(CString::new("-lc").map_err(|_| "invalid -lc".to_string())?);
            argv.push(CString::new(cmd.as_str()).map_err(|_| "cmd contains interior nul".to_string())?);
        }
        let envp = filtered_envp_from(env::vars_os())?;
        let cwd = session.cwd.as_ref().map(|cwd| cstring_from_os(cwd.as_os_str(), "cwd contains interior nul")).transpose()?;
        let argv_ptrs = null_terminated_ptrs(&argv);
        let envp_ptrs = null_terminated_ptrs(&envp);
        Ok(Self { program, _argv: argv, _envp: envp, cwd, argv_ptrs, envp_ptrs })
    }
}

fn exec_child_or_exit(spec: &ChildExecSpec) -> ! {
    // The process may have been forked from a multithreaded parent. Keep this
    // path async-signal-safe: raw chdir/execve/write/_exit only.
    unsafe {
        if let Some(cwd) = &spec.cwd {
            let _ = libc::chdir(cwd.as_ptr());
        }
        libc::execve(spec.program.as_ptr(), spec.argv_ptrs.as_ptr(), spec.envp_ptrs.as_ptr());
        static EXEC_FAILED: &[u8] = b"cleat: execve failed\n";
        let _ = libc::write(libc::STDERR_FILENO, EXEC_FAILED.as_ptr().cast(), EXEC_FAILED.len());
        libc::_exit(127);
    }
}

fn resolve_shell_program(shell: &OsStr) -> PathBuf {
    if shell.as_bytes().contains(&b'/') {
        return PathBuf::from(shell);
    }

    let path = env::var_os("PATH").unwrap_or_else(|| OsString::from("/bin:/usr/bin"));
    for mut dir in env::split_paths(&path) {
        dir.push(shell);
        if is_executable_file(&dir) {
            return dir;
        }
    }
    PathBuf::from(shell)
}

fn is_executable_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    let Ok(path_c) = cstring_from_os(path.as_os_str(), "path contains interior nul") else {
        return false;
    };
    // SAFETY: access reads a valid nul-terminated path prepared in this process.
    unsafe { libc::access(path_c.as_ptr(), libc::X_OK) == 0 }
}

fn filtered_envp_from(env: impl IntoIterator<Item = (OsString, OsString)>) -> Result<Vec<CString>, String> {
    let mut envp = Vec::new();
    for (key, value) in env {
        if STRIP_ENV_VARS.iter().any(|strip| key == OsStr::new(strip)) {
            continue;
        }
        let mut entry = Vec::with_capacity(key.as_bytes().len() + 1 + value.as_bytes().len());
        entry.extend_from_slice(key.as_bytes());
        entry.push(b'=');
        entry.extend_from_slice(value.as_bytes());
        envp.push(CString::new(entry).map_err(|_| "environment contains interior nul".to_string())?);
    }
    Ok(envp)
}

fn cstring_from_os(value: &OsStr, error: &str) -> Result<CString, String> {
    CString::new(value.as_bytes()).map_err(|_| error.to_string())
}

fn null_terminated_ptrs(values: &[CString]) -> Vec<*const libc::c_char> {
    let mut ptrs = Vec::with_capacity(values.len() + 1);
    ptrs.extend(values.iter().map(|value| value.as_ptr()));
    ptrs.push(std::ptr::null());
    ptrs
}

fn borrow_raw<'fd>(fd: RawFd) -> BorrowedFd<'fd> {
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

fn resize_pty(fd: RawFd, cols: u16, rows: u16, width_px: u32, height_px: u32) -> Result<(), String> {
    let winsize = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: width_px.min(u16::MAX as u32) as u16,
        ws_ypixel: height_px.min(u16::MAX as u32) as u16,
    };
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

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn has_pollin(fd: &PollFd<'_>) -> bool {
    fd.revents().map(|flags| flags.contains(PollFlags::POLLIN)).unwrap_or(false)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
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
    use std::{
        ffi::OsString,
        time::{Duration, Instant},
    };

    use nix::{sys::wait::WaitStatus, unistd::Pid};

    use crate::{
        runtime::{SessionMetadata, TerminalSize},
        vt::{TerminalColors, VtEngineKind},
    };

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

    #[test]
    fn child_exec_env_filters_ssh_connection_variables_before_fork() {
        let envp = super::filtered_envp_from([
            (OsString::from("PATH"), OsString::from("/bin")),
            (OsString::from("SSH_TTY"), OsString::from("/dev/ttys001")),
            (OsString::from("SSH_CONNECTION"), OsString::from("client server")),
            (OsString::from("SSH_CLIENT"), OsString::from("client")),
            (OsString::from("CLEAT_KEEP"), OsString::from("1")),
        ])
        .expect("filtered envp");
        let entries: Vec<_> = envp.iter().map(|entry| entry.to_str().expect("utf8 env")).collect();

        assert!(entries.contains(&"PATH=/bin"));
        assert!(entries.contains(&"CLEAT_KEEP=1"));
        assert!(!entries.iter().any(|entry| entry.starts_with("SSH_TTY=")));
        assert!(!entries.iter().any(|entry| entry.starts_with("SSH_CONNECTION=")));
        assert!(!entries.iter().any(|entry| entry.starts_with("SSH_CLIENT=")));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn session_readiness_reports_pty_output() {
        let temp = tempfile::tempdir().expect("tempdir");
        let listener = super::SessionListener::bind(temp.path().join("socket")).expect("bind listener");
        let session = SessionMetadata {
            id: "pty-ready".to_string(),
            vt_engine: VtEngineKind::Passthrough,
            cwd: None,
            cmd: Some("sleep 0.2; printf READY; sleep 0.2".to_string()),
            record: false,
            initial_size: TerminalSize::default(),
            colors: TerminalColors::default(),
        };
        let pty_child = super::PtyChild::spawn(&session).expect("spawn pty child");
        pty_child.set_nonblocking().expect("set pty nonblocking");
        let deadline = Instant::now() + Duration::from_secs(5);

        loop {
            let result = super::poll_session_ready(&listener, None, false, &pty_child, 100).expect("poll session ready");
            if result.pty_readable {
                reap_pty_child(&pty_child, deadline);
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for PTY readiness");
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn reap_pty_child(pty_child: &super::PtyChild, deadline: Instant) {
        loop {
            if pty_child.exited().expect("check child exit").is_some() {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for PTY child exit");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
