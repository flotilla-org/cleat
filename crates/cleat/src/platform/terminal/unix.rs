use std::{
    os::fd::{AsRawFd, BorrowedFd, RawFd},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use nix::{
    errno::Errno,
    poll::{poll, PollFd, PollFlags, PollTimeout},
    sys::termios::{self, SetArg},
    unistd::isatty,
};

static ATTACH_SIGNAL_EXIT: AtomicBool = AtomicBool::new(false);

pub struct ForegroundTerminal {
    fd: RawFd,
    original: Option<termios::Termios>,
}

impl ForegroundTerminal {
    pub fn enter() -> Result<Self, String> {
        let fd = std::io::stdin().as_raw_fd();
        // SAFETY: stdin remains open for the lifetime of the guard; we only borrow its fd.
        let borrowed_fd = unsafe { BorrowedFd::borrow_raw(fd) };
        if !isatty(borrowed_fd).map_err(|err| format!("detect terminal stdin: {err}"))? {
            return Ok(Self { fd, original: None });
        }

        let original = termios::tcgetattr(borrowed_fd).map_err(|err| format!("read terminal attrs: {err}"))?;
        let mut raw = original.clone();
        termios::cfmakeraw(&mut raw);
        termios::tcsetattr(borrowed_fd, SetArg::TCSAFLUSH, &raw).map_err(|err| format!("set terminal raw mode: {err}"))?;

        Ok(Self { fd, original: Some(original) })
    }

    pub fn read_input(&mut self, timeout: Duration, buf: &mut [u8]) -> std::io::Result<Option<usize>> {
        if !poll_fd_readable(self.fd, timeout).map_err(std::io::Error::other)? {
            return Ok(None);
        }
        std::io::Read::read(&mut std::io::stdin().lock(), buf).map(Some)
    }
}

impl Drop for ForegroundTerminal {
    fn drop(&mut self) {
        if let Some(original) = self.original.as_ref() {
            // SAFETY: stdin remains open for the lifetime of the guard; we only borrow its fd.
            let borrowed_fd = unsafe { BorrowedFd::borrow_raw(self.fd) };
            let _ = termios::tcsetattr(borrowed_fd, SetArg::TCSAFLUSH, original);
        }
    }
}

pub struct AttachSignalHandlers {
    previous: Vec<(libc::c_int, libc::sigaction)>,
}

impl AttachSignalHandlers {
    pub fn install() -> Result<Self, String> {
        ATTACH_SIGNAL_EXIT.store(false, Ordering::SeqCst);
        let mut previous = Vec::new();
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            let mut old = unsafe { std::mem::zeroed::<libc::sigaction>() };
            let mut new = unsafe { std::mem::zeroed::<libc::sigaction>() };
            new.sa_sigaction = attach_signal_handler as *const () as usize;
            new.sa_flags = 0;
            unsafe {
                libc::sigemptyset(&mut new.sa_mask);
            }
            let rc = unsafe { libc::sigaction(signal, &new, &mut old) };
            if rc != 0 {
                return Err(format!("install signal handler {signal}: {}", std::io::Error::last_os_error()));
            }
            previous.push((signal, old));
        }
        Ok(Self { previous })
    }
}

impl Drop for AttachSignalHandlers {
    fn drop(&mut self) {
        ATTACH_SIGNAL_EXIT.store(false, Ordering::SeqCst);
        for (signal, action) in self.previous.drain(..).rev() {
            unsafe {
                libc::sigaction(signal, &action, std::ptr::null_mut());
            }
        }
    }
}

extern "C" fn attach_signal_handler(_signal: libc::c_int) {
    ATTACH_SIGNAL_EXIT.store(true, Ordering::SeqCst);
}

pub fn attach_signal_exit_requested() -> bool {
    ATTACH_SIGNAL_EXIT.load(Ordering::SeqCst)
}

pub fn stdout_is_tty() -> Result<bool, String> {
    let fd = std::io::stdout().as_raw_fd();
    // SAFETY: stdout remains open for the duration of this check; we only borrow its fd.
    let borrowed_fd = unsafe { BorrowedFd::borrow_raw(fd) };
    isatty(borrowed_fd).map_err(|err| format!("detect terminal stdout: {err}"))
}

pub fn os_terminal_size() -> Option<(u16, u16)> {
    let fd = std::io::stdout().as_raw_fd();
    let mut winsize = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut winsize) };
    if rc == 0 && winsize.ws_col > 0 && winsize.ws_row > 0 {
        Some((winsize.ws_col, winsize.ws_row))
    } else {
        None
    }
}

fn poll_fd_readable(fd: RawFd, timeout: Duration) -> Result<bool, String> {
    // SAFETY: the fd remains open for the duration of the poll call; we only borrow it temporarily.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let mut fds = [PollFd::new(borrowed, PollFlags::POLLIN)];
    let timeout_ms = i32::try_from(timeout.as_millis()).map_err(|_| format!("poll timeout too large: {}ms", timeout.as_millis()))?;
    match poll(&mut fds, PollTimeout::try_from(timeout_ms).map_err(|err| format!("invalid poll timeout: {err}"))?) {
        Ok(_) => {}
        Err(Errno::EINTR) => return Ok(false),
        Err(err) => return Err(format!("poll readable fd: {err}")),
    }
    Ok(fds[0].revents().map(|flags| flags.contains(PollFlags::POLLIN)).unwrap_or(false))
}
