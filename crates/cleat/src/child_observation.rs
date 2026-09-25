//! Best-effort exit observation. Register before the source releases its child.
//! These blocking helpers belong on workers. Observation never reaps a child.
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::{
    io::{self, Read, Write},
    os::unix::process::ExitStatusExt,
    process::ExitStatus,
    time::Duration,
};

/// Forward a wait status on the manifest's child_status stream (four big-endian
/// bytes). EOF before a frame means the source died without forwarding status.
pub fn forward_status(writer: &mut impl Write, status: ExitStatus) -> io::Result<()> {
    writer.write_all(&status.into_raw().to_be_bytes())
}
pub fn receive_status(reader: &mut impl Read) -> io::Result<Option<ExitStatus>> {
    let mut bytes = [0; 4];
    match reader.read_exact(&mut bytes) {
        Ok(()) => Ok(Some(ExitStatus::from_raw(i32::from_be_bytes(bytes)))),
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => Ok(None),
        Err(err) => Err(err),
    }
}

#[cfg(target_os = "linux")]
pub fn pidfd_open(pid: u32) -> io::Result<OwnedFd> {
    if pid == 0 || pid > i32::MAX as u32 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid child pid"));
    }
    // SAFETY: pidfd_open takes scalar arguments and returns a new owned fd.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

pub struct ChildObserver {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fd: OwnedFd,
}
impl ChildObserver {
    pub fn new(pid: u32) -> io::Result<Self> {
        if pid == 0 || pid > i32::MAX as u32 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid child pid"));
        }
        #[cfg(target_os = "linux")]
        {
            Ok(Self { fd: pidfd_open(pid)? })
        }
        #[cfg(target_os = "macos")]
        {
            // SAFETY: kqueue has no arguments and returns a new owned fd.
            let raw = unsafe { libc::kqueue() };
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            // SAFETY: fcntl sets a descriptor-local flag on our owned fd.
            if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                return Err(io::Error::last_os_error());
            }
            let mut event = libc::kevent {
                ident: pid as _,
                filter: libc::EVFILT_PROC,
                flags: libc::EV_ADD | libc::EV_ONESHOT,
                fflags: libc::NOTE_EXIT | libc::NOTE_EXITSTATUS,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            // SAFETY: event is a live kevent and no output array is requested.
            let register =
                |event: &libc::kevent| unsafe { libc::kevent(fd.as_raw_fd(), event, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
            if register(&event) < 0 {
                let err = io::Error::last_os_error();
                if !matches!(err.raw_os_error(), Some(libc::EACCES | libc::EPERM)) {
                    return Err(err);
                }
                // Non-children may permit exit notification but deny status.
                event.fflags = libc::NOTE_EXIT;
                if register(&event) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(Self { fd })
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            Err(io::Error::new(io::ErrorKind::Unsupported, "child observation unavailable"))
        }
    }

    /// Adopt a transferred pidfd, preserving process identity across PID reuse.
    #[cfg(target_os = "linux")]
    pub fn from_pidfd(fd: OwnedFd) -> Self {
        Self { fd }
    }

    /// Wait for exit. None means exit was observed but status is unavailable;
    /// timeout is a distinct error. WNOWAIT leaves the source's reaper intact.
    pub fn wait(&self, timeout: Duration) -> io::Result<Option<ExitStatus>> {
        #[cfg(target_os = "linux")]
        {
            let mut poll = libc::pollfd { fd: self.fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
            // SAFETY: poll points to one initialized pollfd.
            match unsafe { libc::poll(&mut poll, 1, millis) } {
                -1 => return Err(io::Error::last_os_error()),
                0 => return Err(io::Error::new(io::ErrorKind::TimedOut, "child still running")),
                _ => {}
            }
            if poll.revents & libc::POLLNVAL != 0 {
                return Err(io::Error::from_raw_os_error(libc::EBADF));
            }
            // SAFETY: waitid writes a siginfo_t; WNOWAIT does not consume status.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let result =
                unsafe { libc::waitid(libc::P_PIDFD, self.fd.as_raw_fd() as _, &mut info, libc::WEXITED | libc::WNOHANG | libc::WNOWAIT) };
            if result < 0 {
                let err = io::Error::last_os_error();
                if matches!(err.raw_os_error(), Some(libc::ECHILD | libc::EINVAL | libc::ENOSYS)) {
                    return Ok(None);
                }
                return Err(err);
            }
            // SAFETY: waitid initialized the SIGCHLD union fields.
            if unsafe { info.si_pid() } == 0 {
                return Ok(None);
            }
            let status = unsafe { info.si_status() };
            let raw = match info.si_code {
                libc::CLD_EXITED => status << 8,
                libc::CLD_KILLED => status,
                libc::CLD_DUMPED => status | 0x80,
                _ => return Ok(None),
            };
            Ok(Some(ExitStatus::from_raw(raw)))
        }
        #[cfg(target_os = "macos")]
        {
            let duration = libc::timespec { tv_sec: timeout.as_secs().min(i64::MAX as u64) as _, tv_nsec: timeout.subsec_nanos() as _ };
            // SAFETY: kevent initializes the single output event.
            let mut event: libc::kevent = unsafe { std::mem::zeroed() };
            let result = unsafe { libc::kevent(self.fd.as_raw_fd(), std::ptr::null(), 0, &mut event, 1, &duration) };
            if result < 0 {
                return Err(io::Error::last_os_error());
            }
            if result == 0 {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "child still running"));
            }
            if event.flags & libc::EV_ERROR != 0 {
                return Err(io::Error::from_raw_os_error(event.data as i32));
            }
            Ok((event.fflags & libc::NOTE_EXITSTATUS != 0).then(|| ExitStatus::from_raw(event.data as i32)))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = timeout;
            Ok(None)
        }
    }
}
