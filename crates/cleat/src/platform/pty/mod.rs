#[cfg(not(unix))]
mod unsupported;

#[cfg(not(unix))]
pub use unsupported::*;

#[cfg(unix)]
pub use super::unix::*;
