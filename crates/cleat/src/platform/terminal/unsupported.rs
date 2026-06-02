use std::time::Duration;

pub struct TerminalModeGuard;

impl TerminalModeGuard {
    pub fn activate() -> Result<Self, String> {
        Ok(Self)
    }
}

pub struct AttachSignalHandlers;

impl AttachSignalHandlers {
    pub fn install() -> Result<Self, String> {
        Ok(Self)
    }
}

pub fn attach_signal_exit_requested() -> bool {
    false
}

pub fn stdout_is_tty() -> Result<bool, String> {
    Ok(false)
}

pub fn poll_stdin_readable(_timeout: Duration) -> Result<bool, String> {
    Err("foreground attach stdin polling is only supported on Unix".to_string())
}

pub fn os_terminal_size() -> Option<(u16, u16)> {
    None
}
