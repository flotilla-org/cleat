#[cfg(unix)]
mod unix;
#[cfg(not(unix))]
mod unsupported;

#[cfg(unix)]
pub use unix::*;
#[cfg(not(unix))]
pub use unsupported::*;
