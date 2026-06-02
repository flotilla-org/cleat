#[cfg(windows)]
mod windows;

#[cfg(all(not(unix), not(windows)))]
mod unsupported;

#[cfg(all(not(unix), not(windows)))]
pub use unsupported::*;

#[cfg(unix)]
pub use super::unix::*;

#[cfg(windows)]
pub use windows::*;
