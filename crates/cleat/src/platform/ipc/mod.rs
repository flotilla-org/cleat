use std::path::{Path, PathBuf};

#[cfg(unix)]
mod unix;
#[cfg(all(not(unix), not(windows)))]
mod unsupported;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::*;
#[cfg(all(not(unix), not(windows)))]
pub use unsupported::*;
#[cfg(windows)]
pub use windows::*;

const SOCKET_NAME: &str = "socket";

pub fn session_socket_path(root: &Path, id: &str) -> PathBuf {
    root.join(id).join(SOCKET_NAME)
}
