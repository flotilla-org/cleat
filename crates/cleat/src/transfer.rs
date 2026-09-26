//! Daemon-to-daemon Transfer of a live session (Unix; issue #254).
//!
//! The releasing daemon opens a fresh connection to the target daemon's
//! socket, upgrades it to `cleat-transfer/1`, and runs the exchange below on a
//! worker thread, never on its servicing loop (ADR 0004):
//!
//! 1. `fd_transfer::send`/`receive`: descriptors and manifest, answered by the
//!    transport's validation ACK or NACK. The ACK is validation only.
//! 2. The target validates adoption (roles, descriptor kinds, id, epoch,
//!    engine), builds the session runtime without reading the PTY, and replies
//!    READY — or REFUSED with a reason.
//! 3. On READY the source commits (hosting epoch, `transferred` marker,
//!    directory move, redirects) and sends COMMIT carrying the PTY output it
//!    read after its snapshot. On REFUSED, a transport failure, or its
//!    deadline, it unfreezes and keeps everything; nothing is destroyed.
//! 4. The target replays that output, starts reading the PTY, publishes the
//!    session, and replies COMMITTED.
//!
//! Framing after the upgrade is byte-oriented: a one-byte code, then (where a
//! payload follows) a big-endian u32 length and the payload.

use std::{
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsFd, BorrowedFd, OwnedFd},
        unix::{fs::FileTypeExt, net::UnixStream},
    },
    path::PathBuf,
    sync::mpsc::{Receiver, RecvTimeoutError, Sender},
    time::{Duration, Instant},
};

use http::{Method, StatusCode};
use serde::Deserialize;

use crate::{
    fd_transfer::{self, ReceivedTransfer, Rejection},
    http_uds,
    transfer_manifest::{FdRole, FdTransferManifest, MANIFEST_VERSION},
};

/// Default bound on everything before READY: target probe, descriptor
/// transport, and the target's adoption.
pub(crate) const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a released session's old host keeps answering with a redirect.
pub(crate) const REDIRECT_GRACE: Duration = Duration::from_secs(30);
/// How long an adopter holds a READY session for the source's commit.
pub(crate) const COMMIT_WAIT: Duration = Duration::from_secs(10);
/// How long a committed source waits for COMMITTED before reporting anyway.
const COMMITTED_WAIT: Duration = Duration::from_secs(5);
/// How long a worker waits for its servicing loop's next decision.
const DECISION_WAIT: Duration = Duration::from_secs(30);

const ADOPTION_READY: u8 = 1;
const ADOPTION_REFUSED: u8 = 2;
const COMMIT: u8 = 1;
const ABORT: u8 = 2;
const COMMITTED: u8 = 1;
const MAX_REASON_BYTES: usize = 64 * 1024;
const MAX_TAIL_BYTES: usize = 32 * 1024 * 1024;

/// What the target advertises before anything is released.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TargetProtocol {
    pub version: u16,
    pub min_supported_version: u16,
}

impl TargetProtocol {
    pub fn accepts(&self, version: u16) -> bool {
        (self.min_supported_version..=self.version).contains(&version)
    }
}

/// Source worker → servicing loop.
pub(crate) enum SourceEvent {
    Probed(Result<TargetProtocol, String>),
    /// READY (`Ok`), or why the target did not get there.
    Handshake(Result<(), HandshakeFailure>),
    Committed(Result<(), String>),
}

pub(crate) enum HandshakeFailure {
    /// The target answered with a NACK (transport or adoption).
    Refused(String),
    /// Transport or protocol failure; the target's state is unknown but it
    /// never adopts without a COMMIT.
    Failed(String),
}

impl HandshakeFailure {
    pub fn message(&self) -> &str {
        match self {
            Self::Refused(message) | Self::Failed(message) => message,
        }
    }
}

/// Servicing loop → source worker.
pub(crate) enum SourceDecision {
    Proceed { manifest: Box<FdTransferManifest>, fds: Vec<OwnedFd> },
    Commit { tail: Vec<u8> },
    Abort,
}

/// The releasing side of the exchange. Every blocking step has a socket
/// timeout; the servicing loop enforces the overall deadline independently
/// and simply stops listening when it passes.
pub(crate) fn run_source_worker(socket: PathBuf, deadline: Instant, events: Sender<SourceEvent>, decisions: Receiver<SourceDecision>) {
    let probed = probe_target(&socket, deadline);
    let failed = probed.is_err();
    if events.send(SourceEvent::Probed(probed)).is_err() || failed {
        return;
    }
    let (manifest, fds) = match decisions.recv_timeout(remaining(deadline)) {
        Ok(SourceDecision::Proceed { manifest, fds }) => (manifest, fds),
        _ => return,
    };
    let mut stream = match handshake(&socket, deadline, &manifest, &fds) {
        Ok(stream) => {
            // The target owns duplicates now; ours close here.
            drop(fds);
            if events.send(SourceEvent::Handshake(Ok(()))).is_err() {
                return;
            }
            stream
        }
        Err(failure) => {
            let _ = events.send(SourceEvent::Handshake(Err(failure)));
            return;
        }
    };
    match decisions.recv_timeout(DECISION_WAIT) {
        Ok(SourceDecision::Commit { tail }) => {
            let result = (|| {
                stream.set_write_timeout(Some(COMMITTED_WAIT)).map_err(|err| format!("set transfer write timeout: {err}"))?;
                stream.set_read_timeout(Some(COMMITTED_WAIT)).map_err(|err| format!("set transfer read timeout: {err}"))?;
                write_frame(&mut stream, COMMIT, &tail).map_err(|err| format!("send transfer commit: {err}"))?;
                read_committed(&mut stream)
            })();
            let _ = events.send(SourceEvent::Committed(result));
        }
        Ok(SourceDecision::Abort) | Ok(SourceDecision::Proceed { .. }) | Err(RecvTimeoutError::Timeout) => {
            let _ = stream.set_write_timeout(Some(Duration::from_millis(250)));
            let _ = write_frame(&mut stream, ABORT, &[]);
        }
        // The servicing loop gave up (deadline); closing tells the target.
        Err(RecvTimeoutError::Disconnected) => {}
    }
}

fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(1))
}

/// The servicing loop owns the deadline and reports it; socket timeouts are
/// only a backstop that frees the worker shortly after.
const SOCKET_TIMEOUT_SLACK: Duration = Duration::from_secs(1);

fn connect(socket: &PathBuf, deadline: Instant) -> Result<UnixStream, String> {
    let stream = UnixStream::connect(socket).map_err(|err| format!("connect to target daemon {}: {err}", socket.display()))?;
    let timeout = remaining(deadline) + SOCKET_TIMEOUT_SLACK;
    stream.set_read_timeout(Some(timeout)).map_err(|err| format!("set transfer read timeout: {err}"))?;
    stream.set_write_timeout(Some(timeout)).map_err(|err| format!("set transfer write timeout: {err}"))?;
    Ok(stream)
}

fn probe_target(socket: &PathBuf, deadline: Instant) -> Result<TargetProtocol, String> {
    #[derive(Deserialize)]
    struct Health {
        packet_protocol: Option<ProtocolRange>,
    }
    #[derive(Deserialize)]
    struct ProtocolRange {
        version: u16,
        min_supported_version: u16,
    }
    let mut stream = connect(socket, deadline)?;
    http_uds::write_request(&mut stream, Method::GET, "/", &[]).map_err(|err| format!("probe target daemon: {err}"))?;
    let response = http_uds::read_response(&mut stream).map_err(|err| format!("probe target daemon: {err}"))?;
    if response.status != StatusCode::OK {
        return Err(format!("probe target daemon: HTTP {}", response.status));
    }
    let health: Health = serde_json::from_slice(&response.body).map_err(|err| format!("parse target daemon status: {err}"))?;
    let range = health.packet_protocol.ok_or("target daemon does not support session transfer")?;
    Ok(TargetProtocol { version: range.version, min_supported_version: range.min_supported_version })
}

fn handshake(socket: &PathBuf, deadline: Instant, manifest: &FdTransferManifest, fds: &[OwnedFd]) -> Result<UnixStream, HandshakeFailure> {
    let mut stream = connect(socket, deadline).map_err(HandshakeFailure::Failed)?;
    http_uds::write_transfer_upgrade_request(&mut stream).map_err(|err| HandshakeFailure::Failed(format!("request transfer: {err}")))?;
    let response =
        http_uds::read_response_head(&mut stream).map_err(|err| HandshakeFailure::Failed(format!("read transfer upgrade: {err}")))?;
    if response.status != StatusCode::SWITCHING_PROTOCOLS {
        return Err(HandshakeFailure::Failed(format!("target daemon refused the transfer upgrade: HTTP {}", response.status)));
    }
    let borrowed: Vec<BorrowedFd<'_>> = fds.iter().map(AsFd::as_fd).collect();
    fd_transfer::send(&mut stream, manifest, &borrowed).map_err(|err| {
        if err.starts_with("FD transfer rejected") {
            HandshakeFailure::Refused(err)
        } else {
            HandshakeFailure::Failed(err)
        }
    })?;
    match read_adoption_reply(&mut stream) {
        Ok(Ok(())) => Ok(stream),
        Ok(Err(rejection)) => Err(HandshakeFailure::Refused(format!("target daemon refused adoption: {}", rejection.reason))),
        Err(err) => Err(HandshakeFailure::Failed(err)),
    }
}

fn write_frame(stream: &mut impl Write, code: u8, payload: &[u8]) -> std::io::Result<()> {
    let length = u32::try_from(payload.len()).map_err(|_| std::io::Error::other("transfer frame too large"))?;
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.push(code);
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(payload);
    stream.write_all(&frame)
}

fn read_frame(stream: &mut impl Read, limit: usize) -> Result<(u8, Vec<u8>), String> {
    let mut head = [0u8; 5];
    stream.read_exact(&mut head).map_err(|err| format!("read transfer frame: {err}"))?;
    let length = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if length > limit {
        return Err(format!("transfer frame of {length} bytes exceeds {limit}"));
    }
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload).map_err(|err| format!("read transfer frame: {err}"))?;
    Ok((head[0], payload))
}

pub(crate) fn write_ready(stream: &mut impl Write) -> std::io::Result<()> {
    write_frame(stream, ADOPTION_READY, &[])
}

pub(crate) fn write_refusal(stream: &mut impl Write, reason: &str) -> std::io::Result<()> {
    let rejection = Rejection { sender_version: MANIFEST_VERSION, receiver_version: MANIFEST_VERSION, reason: reason.to_string() };
    write_frame(stream, ADOPTION_REFUSED, &serde_json::to_vec(&rejection).map_err(std::io::Error::other)?)
}

fn read_adoption_reply(stream: &mut impl Read) -> Result<Result<(), Rejection>, String> {
    match read_frame(stream, MAX_REASON_BYTES)? {
        (ADOPTION_READY, _) => Ok(Ok(())),
        (ADOPTION_REFUSED, payload) => serde_json::from_slice(&payload).map(Err).map_err(|err| format!("parse adoption refusal: {err}")),
        (code, _) => Err(format!("invalid adoption reply {code}")),
    }
}

/// `Some(tail)` on COMMIT, `None` on ABORT.
pub(crate) fn read_commit(stream: &mut impl Read) -> Result<Option<Vec<u8>>, String> {
    match read_frame(stream, MAX_TAIL_BYTES)? {
        (COMMIT, tail) => Ok(Some(tail)),
        (ABORT, _) => Ok(None),
        (code, _) => Err(format!("invalid transfer commit {code}")),
    }
}

pub(crate) fn write_committed(stream: &mut impl Write) -> std::io::Result<()> {
    write_frame(stream, COMMITTED, &[])
}

fn read_committed(stream: &mut impl Read) -> Result<(), String> {
    match read_frame(stream, MAX_REASON_BYTES)? {
        (COMMITTED, _) => Ok(()),
        (code, _) => Err(format!("invalid transfer acknowledgement {code}")),
    }
}

/// A validated transport offer waiting for the servicing loop's adoption
/// decision.
pub(crate) struct AdoptionOffer {
    pub received: ReceivedTransfer,
    pub stream: UnixStream,
}

/// The adopting side's receive, on a worker. Transport-level rejections are
/// answered by `fd_transfer::receive` itself.
pub(crate) fn run_adoption_receiver(mut stream: UnixStream, offers: Sender<AdoptionOffer>) {
    let _ = stream.set_read_timeout(Some(DEFAULT_HANDSHAKE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(DEFAULT_HANDSHAKE_TIMEOUT));
    match fd_transfer::receive(&mut stream) {
        Ok(received) => {
            let _ = offers.send(AdoptionOffer { received, stream });
        }
        Err(err) => eprintln!("transfer receive failed: {err}"),
    }
}

/// Waits for the source's COMMIT or ABORT after READY, on a worker.
pub(crate) fn run_commit_receiver(mut stream: UnixStream, session_id: String, outcomes: Sender<(String, Result<Option<Vec<u8>>, String>)>) {
    let _ = stream.set_read_timeout(Some(COMMIT_WAIT));
    let outcome = read_commit(&mut stream);
    let _ = outcomes.send((session_id, outcome));
}

/// The descriptors an adopter takes over, checked against their roles.
pub(crate) struct AdoptionDescriptors {
    pub pty_master: OwnedFd,
    pub recording: Option<File>,
    pub pidfd: Option<OwnedFd>,
    pub child_status: Option<UnixStream>,
}

/// Validate required roles and descriptor kinds (transfer-manifest entry 5).
/// Unknown roles are owned and closed with the transfer.
pub(crate) fn classify_descriptors(manifest: &FdTransferManifest, fds: Vec<OwnedFd>) -> Result<AdoptionDescriptors, String> {
    let mut slots: Vec<Option<OwnedFd>> = fds.into_iter().map(Some).collect();
    let mut take = |role: FdRole| -> Option<OwnedFd> {
        let entry = manifest.fds.iter().find(|entry| entry.role == role)?;
        slots.get_mut(entry.index).and_then(Option::take)
    };
    let pty_master = take(FdRole::pty_master()).ok_or("transfer manifest has no pty_master descriptor")?;
    let recording = take(FdRole::recording());
    let pidfd = take(FdRole::pidfd());
    let child_status = take(FdRole::child_status());

    let pty_type = std::fs::File::from(pty_master.try_clone().map_err(|err| format!("inspect pty_master: {err}"))?)
        .metadata()
        .map_err(|err| format!("inspect pty_master: {err}"))?
        .file_type();
    if !pty_type.is_char_device() {
        return Err("pty_master descriptor is not a terminal device".to_string());
    }
    let recording = recording
        .map(|fd| {
            let file = File::from(fd);
            let metadata = file.metadata().map_err(|err| format!("inspect recording: {err}"))?;
            if !metadata.is_file() {
                return Err("recording descriptor is not a regular file".to_string());
            }
            let flags =
                nix::fcntl::fcntl(file.as_fd(), nix::fcntl::FcntlArg::F_GETFL).map_err(|err| format!("inspect recording: {err}"))?;
            if nix::fcntl::OFlag::from_bits_truncate(flags) & nix::fcntl::OFlag::O_APPEND != nix::fcntl::OFlag::O_APPEND {
                return Err("recording descriptor is not opened for append".to_string());
            }
            Ok(file)
        })
        .transpose()?;
    let child_status = child_status
        .map(|fd| {
            let file = File::from(fd);
            let kind = file.metadata().map_err(|err| format!("inspect child_status: {err}"))?.file_type();
            if !kind.is_socket() {
                return Err("child_status descriptor is not a socket".to_string());
            }
            Ok(UnixStream::from(OwnedFd::from(file)))
        })
        .transpose()?;
    Ok(AdoptionDescriptors { pty_master, recording, pidfd, child_status })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adoption_and_commit_frames_round_trip() {
        let mut buffer = Vec::new();
        write_ready(&mut buffer).unwrap();
        write_refusal(&mut buffer, "id collides").unwrap();
        write_frame(&mut buffer, COMMIT, b"tail bytes").unwrap();
        write_frame(&mut buffer, ABORT, &[]).unwrap();
        write_committed(&mut buffer).unwrap();
        let mut reader = buffer.as_slice();
        assert!(read_adoption_reply(&mut reader).unwrap().is_ok());
        assert_eq!(read_adoption_reply(&mut reader).unwrap().unwrap_err().reason, "id collides");
        assert_eq!(read_commit(&mut reader).unwrap(), Some(b"tail bytes".to_vec()));
        assert_eq!(read_commit(&mut reader).unwrap(), None);
        read_committed(&mut reader).unwrap();
    }

    #[test]
    fn oversized_frames_are_rejected_before_allocation() {
        let mut frame = vec![COMMIT];
        frame.extend_from_slice(&u32::MAX.to_be_bytes());
        assert!(read_commit(&mut frame.as_slice()).unwrap_err().contains("exceeds"));
    }
}
