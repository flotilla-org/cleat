#[cfg(unix)]
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::time::Duration;

#[cfg(unix)]
use nix::{
    errno::Errno,
    poll::{poll, PollFd, PollFlags, PollTimeout},
    sys::termios::{self, SetArg},
    unistd::isatty,
};

#[cfg(unix)]
static ATTACH_SIGNAL_EXIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(unix)]
pub struct TerminalModeGuard {
    fd: RawFd,
    original: Option<termios::Termios>,
}

#[cfg(unix)]
impl TerminalModeGuard {
    pub fn activate() -> Result<Self, String> {
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
}

#[cfg(unix)]
impl Drop for TerminalModeGuard {
    fn drop(&mut self) {
        if let Some(original) = self.original.as_ref() {
            // SAFETY: stdin remains open for the lifetime of the guard; we only borrow its fd.
            let borrowed_fd = unsafe { BorrowedFd::borrow_raw(self.fd) };
            let _ = termios::tcsetattr(borrowed_fd, SetArg::TCSAFLUSH, original);
        }
    }
}

#[cfg(not(unix))]
pub struct TerminalModeGuard;

#[cfg(not(unix))]
impl TerminalModeGuard {
    pub fn activate() -> Result<Self, String> {
        Ok(Self)
    }
}

#[cfg(unix)]
pub struct AttachSignalHandlers {
    previous: Vec<(libc::c_int, libc::sigaction)>,
}

#[cfg(unix)]
impl AttachSignalHandlers {
    pub fn install() -> Result<Self, String> {
        use std::sync::atomic::Ordering;

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

#[cfg(unix)]
impl Drop for AttachSignalHandlers {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;

        ATTACH_SIGNAL_EXIT.store(false, Ordering::SeqCst);
        for (signal, action) in self.previous.drain(..).rev() {
            unsafe {
                libc::sigaction(signal, &action, std::ptr::null_mut());
            }
        }
    }
}

#[cfg(unix)]
extern "C" fn attach_signal_handler(_signal: libc::c_int) {
    use std::sync::atomic::Ordering;

    ATTACH_SIGNAL_EXIT.store(true, Ordering::SeqCst);
}

#[cfg(not(unix))]
pub struct AttachSignalHandlers;

#[cfg(not(unix))]
impl AttachSignalHandlers {
    pub fn install() -> Result<Self, String> {
        Ok(Self)
    }
}

#[cfg(unix)]
pub fn attach_signal_exit_requested() -> bool {
    use std::sync::atomic::Ordering;

    ATTACH_SIGNAL_EXIT.load(Ordering::SeqCst)
}

#[cfg(not(unix))]
pub fn attach_signal_exit_requested() -> bool {
    false
}

#[cfg(unix)]
pub fn stdout_is_tty() -> Result<bool, String> {
    let fd = std::io::stdout().as_raw_fd();
    // SAFETY: stdout remains open for the duration of this check; we only borrow its fd.
    let borrowed_fd = unsafe { BorrowedFd::borrow_raw(fd) };
    isatty(borrowed_fd).map_err(|err| format!("detect terminal stdout: {err}"))
}

#[cfg(not(unix))]
pub fn stdout_is_tty() -> Result<bool, String> {
    Ok(false)
}

#[cfg(unix)]
pub fn poll_stdin_readable(timeout: Duration) -> Result<bool, String> {
    let fd = std::io::stdin().as_raw_fd();
    poll_fd_readable(fd, timeout)
}

#[cfg(not(unix))]
pub fn poll_stdin_readable(_timeout: Duration) -> Result<bool, String> {
    Err("foreground attach stdin polling is only supported on Unix".to_string())
}

#[cfg(unix)]
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

pub fn current_terminal_size() -> (u16, u16) {
    #[cfg(unix)]
    {
        let fd = std::io::stdout().as_raw_fd();
        let mut winsize = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
        let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut winsize) };
        if rc == 0 && winsize.ws_col > 0 && winsize.ws_row > 0 {
            return (winsize.ws_col, winsize.ws_row);
        }
    }
    size_from_env(std::env::var("COLUMNS").ok().as_deref(), std::env::var("LINES").ok().as_deref())
}

fn size_from_env(columns: Option<&str>, lines: Option<&str>) -> (u16, u16) {
    let cols = columns.and_then(|value| value.parse::<u16>().ok()).unwrap_or(80);
    let rows = lines.and_then(|value| value.parse::<u16>().ok()).unwrap_or(24);
    (cols, rows)
}

#[cfg(test)]
mod tests {
    #[test]
    fn size_from_env_falls_back_to_defaults_for_missing_or_invalid_values() {
        assert_eq!(super::size_from_env(None, None), (80, 24));
        assert_eq!(super::size_from_env(Some("not-a-number"), Some("also-bad")), (80, 24));
    }

    #[test]
    fn size_from_env_uses_valid_columns_and_lines() {
        assert_eq!(super::size_from_env(Some("132"), Some("43")), (132, 43));
    }
}
