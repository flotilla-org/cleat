#[cfg(windows)]
mod windows;

#[cfg(all(not(unix), not(windows)))]
mod unsupported;

#[cfg(all(not(unix), not(windows)))]
pub use unsupported::*;
#[cfg(windows)]
pub use windows::*;

#[cfg(unix)]
pub use super::unix::*;
