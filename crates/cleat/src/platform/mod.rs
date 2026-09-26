pub mod daemon;
pub mod ipc;
pub mod process;
pub mod pty;
pub mod signals;
pub mod terminal;

#[cfg(unix)]
mod unix;

/// How a session's child ended, as far as its host can honestly tell. A host
/// that adopted a child from another host may observe the exit without its
/// status; it records `Unknown` rather than fabricating a code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildExit {
    Code(i32),
    Unknown,
}
