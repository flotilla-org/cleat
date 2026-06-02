#[cfg(unix)]
mod unix;
#[cfg(not(unix))]
mod unsupported;

#[cfg(unix)]
pub(crate) use unix::*;
#[cfg(not(unix))]
pub(crate) use unsupported::*;
