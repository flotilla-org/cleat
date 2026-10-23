//! `cleat proxy`: relay stdio to a local daemon's packet socket.
//!
//! This is the far end of a command transport, e.g. a provider spawning
//! `ssh host cleat proxy --daemon work`: the provider speaks the packet
//! protocol over the command's stdio, and the proxy copies bytes both ways
//! without interpreting them. It never starts, discovers beyond the runtime
//! root, or recovers a daemon. SSH (or whatever runs it) is the
//! authentication; the proxy adds none.
#[cfg(unix)]
use std::io::{Read, Write};

use crate::runtime::RuntimeLayout;

/// Connect to `layout`'s daemon socket and relay until the daemon side
/// closes. An unreachable daemon is an error naming the daemon and socket, so
/// the near end can tell "no daemon" from a dropped connection.
#[cfg(unix)]
pub fn run(layout: &RuntimeLayout) -> Result<(), String> {
    let socket = layout.socket_path();
    let stream = crate::platform::ipc::try_connect_session_stream(&socket)
        .map_err(|err| format!("daemon {} is not reachable at {}: {err}", layout.daemon_name(), socket.display()))?;
    let mut to_daemon = stream.try_clone().map_err(|err| format!("clone daemon socket: {err}"))?;
    let mut from_daemon = stream;
    std::thread::spawn(move || {
        let _ = relay(&mut std::io::stdin().lock(), &mut to_daemon);
        let _ = to_daemon.shutdown(std::net::Shutdown::Write);
    });
    // Stdin closing only half-closes the socket: the daemon still answers what
    // it already received, then closes, which ends this direction. A vanished
    // near end ends it too, through that close or a failed stdout write.
    let _ = relay(&mut from_daemon, &mut std::io::stdout().lock());
    Ok(())
}

#[cfg(not(unix))]
pub fn run(_layout: &RuntimeLayout) -> Result<(), String> {
    Err("cleat proxy is unsupported on this platform".into())
}

/// Copy until EOF, flushing each chunk: the stream is interactive.
#[cfg(unix)]
fn relay(from: &mut impl Read, to: &mut impl Write) -> std::io::Result<()> {
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = match from.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => read,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        to.write_all(&buffer[..read])?;
        to.flush()?;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn relay_copies_every_chunk_until_eof() {
        let input: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let mut output = Vec::new();
        relay(&mut input.as_slice(), &mut output).unwrap();
        assert_eq!(output, input);
    }
}
