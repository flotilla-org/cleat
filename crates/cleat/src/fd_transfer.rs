use std::{
    io::{IoSlice, IoSliceMut, Read, Write},
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
        unix::net::UnixStream,
    },
};

use nix::{
    fcntl::{fcntl, FcntlArg, FdFlag},
    sys::socket::{recvmsg, sendmsg, ControlMessage, ControlMessageOwned, MsgFlags},
};
use serde::{Deserialize, Serialize};

use crate::runtime::SessionMetadata;

const TRANSFER_MARKER: u8 = 1;
const TRANSFER_ACK: u8 = 1;
const TRANSFER_NACK: u8 = 2;
const MAX_TRANSFER_FDS: usize = 16;
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FdRole(String);

impl FdRole {
    pub fn new(role: impl Into<String>) -> Self {
        Self(role.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn pty_master() -> Self {
        Self::new("pty_master")
    }

    pub fn child_status() -> Self {
        Self::new("child_status")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FdTransferOperation {
    Sibling,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FdManifestEntry {
    pub index: usize,
    pub role: FdRole,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FdTransferManifest {
    pub version: u16,
    pub operation: FdTransferOperation,
    pub fds: Vec<FdManifestEntry>,
    pub session: SessionMetadata,
    pub source_session: String,
    pub target_daemon: String,
    pub child_pid: u32,
}

#[derive(Debug)]
pub struct ReceivedTransfer {
    pub manifest: FdTransferManifest,
    pub fds: Vec<OwnedFd>,
}

pub fn send(stream: &mut UnixStream, manifest: &FdTransferManifest, fds: &[BorrowedFd<'_>]) -> Result<(), String> {
    validate_fd_manifest(&manifest.fds, fds.len())?;
    let json = serde_json::to_vec(manifest).map_err(|err| format!("serialize FD transfer manifest: {err}"))?;
    if json.len() > MAX_MANIFEST_BYTES {
        return Err(format!("FD transfer manifest exceeds {MAX_MANIFEST_BYTES} bytes"));
    }

    let marker = [TRANSFER_MARKER];
    let iov = [IoSlice::new(&marker)];
    let raw_fds: Vec<_> = fds.iter().map(AsRawFd::as_raw_fd).collect();
    let sent = sendmsg::<()>(stream.as_raw_fd(), &iov, &[ControlMessage::ScmRights(&raw_fds)], MsgFlags::empty(), None)
        .map_err(|err| format!("send FD transfer descriptors: {err}"))?;
    if sent != marker.len() {
        return Err(format!("short FD transfer descriptor send: wrote {sent} of {} bytes", marker.len()));
    }

    let length = u32::try_from(json.len()).map_err(|_| "FD transfer manifest length does not fit u32".to_string())?;
    stream.write_all(&length.to_be_bytes()).map_err(|err| format!("write FD transfer manifest length: {err}"))?;
    stream.write_all(&json).map_err(|err| format!("write FD transfer manifest: {err}"))
}

pub fn receive(stream: &mut UnixStream) -> Result<ReceivedTransfer, String> {
    let mut marker = [0u8; 1];
    let mut cmsg_space = nix::cmsg_space!([std::os::fd::RawFd; MAX_TRANSFER_FDS]);
    let (received_bytes, raw_fds) = {
        let mut iov = [IoSliceMut::new(&mut marker)];
        let message = recvmsg::<()>(stream.as_raw_fd(), &mut iov, Some(&mut cmsg_space), MsgFlags::empty())
            .map_err(|err| format!("receive FD transfer descriptors: {err}"))?;
        let mut raw_fds = Vec::new();
        for control in message.cmsgs().map_err(|err| format!("read FD transfer control messages: {err}"))? {
            if let ControlMessageOwned::ScmRights(received) = control {
                raw_fds.extend(received);
            }
        }
        (message.bytes, raw_fds)
    };
    let mut fds = Vec::new();
    for raw_fd in raw_fds {
        // SAFETY: SCM_RIGHTS returned a new descriptor owned by this process.
        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        fcntl(fd.as_fd(), FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))
            .map_err(|err| format!("set close-on-exec on transferred descriptor: {err}"))?;
        fds.push(fd);
    }
    if received_bytes != marker.len() || marker[0] != TRANSFER_MARKER {
        return Err("invalid FD transfer marker".to_string());
    }

    let mut length = [0u8; 4];
    stream.read_exact(&mut length).map_err(|err| format!("read FD transfer manifest length: {err}"))?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_MANIFEST_BYTES {
        return Err(format!("FD transfer manifest exceeds {MAX_MANIFEST_BYTES} bytes"));
    }
    let mut json = vec![0; length];
    stream.read_exact(&mut json).map_err(|err| format!("read FD transfer manifest: {err}"))?;
    let manifest: FdTransferManifest = serde_json::from_slice(&json).map_err(|err| format!("parse FD transfer manifest: {err}"))?;
    validate_fd_manifest(&manifest.fds, fds.len())?;
    Ok(ReceivedTransfer { manifest, fds })
}

pub fn send_ack(stream: &mut UnixStream) -> Result<(), String> {
    stream.write_all(&[TRANSFER_ACK]).map_err(|err| format!("write FD transfer acknowledgement: {err}"))
}

pub fn send_nack(stream: &mut UnixStream, message: &str) -> Result<(), String> {
    let message = message.as_bytes();
    if message.len() > MAX_MANIFEST_BYTES {
        return Err(format!("FD transfer rejection exceeds {MAX_MANIFEST_BYTES} bytes"));
    }
    let length = u32::try_from(message.len()).map_err(|_| "FD transfer rejection length does not fit u32".to_string())?;
    stream.write_all(&[TRANSFER_NACK]).map_err(|err| format!("write FD transfer rejection marker: {err}"))?;
    stream.write_all(&length.to_be_bytes()).map_err(|err| format!("write FD transfer rejection length: {err}"))?;
    stream.write_all(message).map_err(|err| format!("write FD transfer rejection: {err}"))
}

pub fn receive_ack(stream: &mut UnixStream) -> Result<(), String> {
    let mut ack = [0u8; 1];
    stream.read_exact(&mut ack).map_err(|err| format!("read FD transfer acknowledgement: {err}"))?;
    match ack[0] {
        TRANSFER_ACK => Ok(()),
        TRANSFER_NACK => {
            let mut length = [0u8; 4];
            stream.read_exact(&mut length).map_err(|err| format!("read FD transfer rejection length: {err}"))?;
            let length = u32::from_be_bytes(length) as usize;
            if length > MAX_MANIFEST_BYTES {
                return Err(format!("FD transfer rejection exceeds {MAX_MANIFEST_BYTES} bytes"));
            }
            let mut message = vec![0; length];
            stream.read_exact(&mut message).map_err(|err| format!("read FD transfer rejection: {err}"))?;
            Err(format!("FD transfer rejected: {}", String::from_utf8_lossy(&message)))
        }
        _ => Err("invalid FD transfer acknowledgement".to_string()),
    }
}

fn validate_fd_manifest(entries: &[FdManifestEntry], fd_count: usize) -> Result<(), String> {
    if fd_count == 0 {
        return Err("FD transfer requires at least one descriptor".to_string());
    }
    if fd_count > MAX_TRANSFER_FDS {
        return Err(format!("FD transfer supports at most {MAX_TRANSFER_FDS} descriptors"));
    }
    if entries.len() != fd_count {
        return Err(format!("FD transfer manifest describes {} descriptors but carried {fd_count}", entries.len()));
    }
    let mut indexes: Vec<_> = entries.iter().map(|entry| entry.index).collect();
    indexes.sort_unstable();
    if indexes != (0..fd_count).collect::<Vec<_>>() {
        return Err("FD transfer manifest indexes must be unique and contiguous from zero".to_string());
    }
    if entries.iter().any(|entry| entry.role.as_str().is_empty()) {
        return Err("FD transfer manifest roles must not be empty".to_string());
    }
    let roles: std::collections::HashSet<_> = entries.iter().map(|entry| &entry.role).collect();
    if roles.len() != entries.len() {
        return Err("FD transfer manifest roles must be unique".to_string());
    }
    Ok(())
}
