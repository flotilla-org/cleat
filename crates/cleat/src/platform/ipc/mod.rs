use std::path::{Path, PathBuf};

#[cfg(unix)]
mod unix;
#[cfg(not(unix))]
mod unsupported;

#[cfg(unix)]
pub use unix::*;
#[cfg(not(unix))]
pub use unsupported::*;

const SOCKET_NAME: &str = "socket";

pub fn session_socket_path(root: &Path, id: &str) -> PathBuf {
    root.join(id).join(SOCKET_NAME)
}
