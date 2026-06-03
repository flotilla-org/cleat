use std::time::Duration;

pub struct ForegroundTerminal;

impl ForegroundTerminal {
    pub fn enter() -> Result<Self, String> {
        Ok(Self)
    }

    pub fn read_input(&mut self, _timeout: Duration, _buf: &mut [u8]) -> std::io::Result<Option<usize>> {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "foreground attach input is not supported on this platform"))
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

pub fn os_terminal_size() -> Option<(u16, u16)> {
    None
}
