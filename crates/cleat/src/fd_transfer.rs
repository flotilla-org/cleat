//! Synchronous transfer transport, adapted from ceda0b99 (PR #166).
//! Run on a worker, never on the daemon servicing loop. ACK means validation, not adoption.
use std::{
    io::{IoSlice, Read, Write},
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
        unix::net::UnixStream,
    },
};

use nix::{
    fcntl::{fcntl, FcntlArg, FdFlag},
    sys::socket::{sendmsg, ControlMessage, MsgFlags},
};
use serde::{Deserialize, Serialize};

use crate::transfer_manifest::{FdManifestEntry, FdTransferManifest, VersionHeader, MANIFEST_VERSION};

const TRANSFER_MARKER: u8 = 1;
const TRANSFER_ACK: u8 = 1;
const TRANSFER_NACK: u8 = 2;
const MAX_TRANSFER_FDS: usize = 16;
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
// Darwin installs all rights before copying the control data to userspace, and
// retains the original cmsg_len on truncation. Reserve its kernel maximum (XNU
// UIPC_MAX_CMSG_FD = 512) so even an oversized transfer can be fully closed.
#[cfg(target_os = "macos")]
const RECEIVE_CONTROL_WORDS: usize = 513;
#[cfg(not(target_os = "macos"))]
const RECEIVE_CONTROL_WORDS: usize = MAX_TRANSFER_FDS + 1;

#[derive(Debug)]
pub struct ReceivedTransfer {
    pub manifest: FdTransferManifest,
    fds: Vec<OwnedFd>,
}

impl ReceivedTransfer {
    pub fn fds(&self) -> &[OwnedFd] {
        &self.fds
    }

    /// Release the validated descriptors to the caller. This is local ownership
    /// only; it does not implement the later session adoption/commit protocol.
    pub fn commit(self) -> (FdTransferManifest, Vec<OwnedFd>) {
        (self.manifest, self.fds)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rejection {
    pub sender_version: u16,
    pub receiver_version: u16,
    pub reason: String,
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
    // Embedded callers may retain the default SIGPIPE disposition. A closed
    // peer must return an error rather than terminate the hosting process.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let flags = MsgFlags::from_bits_retain(libc::MSG_NOSIGNAL);
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let flags = MsgFlags::empty();
    let sent = sendmsg::<()>(stream.as_raw_fd(), &iov, &[ControlMessage::ScmRights(&raw_fds)], flags, None)
        .map_err(|err| format!("send FD transfer descriptors: {err}"))?;
    if sent != marker.len() {
        return Err(format!("short FD transfer descriptor send: wrote {sent} of {} bytes", marker.len()));
    }

    let length = u32::try_from(json.len()).map_err(|_| "FD transfer manifest length does not fit u32".to_string())?;
    stream.write_all(&length.to_be_bytes()).map_err(|err| format!("write FD transfer manifest length: {err}"))?;
    stream.write_all(&json).map_err(|err| format!("write FD transfer manifest: {err}"))?;
    receive_ack(stream)
}

pub fn receive(stream: &mut UnixStream) -> Result<ReceivedTransfer, String> {
    let (marker, fds) = receive_descriptors(stream)?;
    if marker != TRANSFER_MARKER {
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
    // Decode only the version envelope first: a future schema need not contain
    // any of today's other fields to receive an explicit version rejection.
    let header: VersionHeader = serde_json::from_slice(&json).map_err(|err| format!("parse transfer version: {err}"))?;
    let validated: Result<FdTransferManifest, String> = (|| {
        header.validate()?;
        let manifest: FdTransferManifest = serde_json::from_slice(&json).map_err(|err| format!("parse FD transfer manifest: {err}"))?;
        validate_fd_manifest(&manifest.fds, fds.len())?;
        if manifest.hosting_epoch == 0 {
            return Err("hosting epoch must be positive".into());
        }
        Ok(manifest)
    })();
    match validated {
        Ok(manifest) => {
            send_ack(stream)?;
            Ok(ReceivedTransfer { manifest, fds })
        }
        Err(reason) => {
            send_nack(stream, &Rejection { sender_version: header.version, receiver_version: MANIFEST_VERSION, reason: reason.clone() })?;
            Err(reason)
        }
    }
}

/// Own every installed descriptor before any fallible validation, including a
/// truncated control message. nix's cmsgs() refuses MSG_CTRUNC before exposing
/// the descriptors, so use the bounded libc control-message traversal here.
fn receive_descriptors(stream: &UnixStream) -> Result<(u8, Vec<OwnedFd>), String> {
    let mut marker = [0u8];
    // cmsghdr elements supply the alignment required by CMSG_FIRSTHDR.
    let mut control: [libc::cmsghdr; RECEIVE_CONTROL_WORDS] = unsafe { std::mem::zeroed() };
    let mut iov = libc::iovec { iov_base: marker.as_mut_ptr().cast(), iov_len: 1 };
    // SAFETY: all pointers below reference live, suitably aligned buffers.
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = std::mem::size_of_val(&control) as _;
    #[cfg(target_os = "linux")]
    let flags = libc::MSG_CMSG_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let flags = 0;
    // SAFETY: message describes the live buffers above; stream is borrowed.
    let count = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut message, flags) };
    if count < 0 {
        return Err(format!("receive descriptors: {}", std::io::Error::last_os_error()));
    }
    let mut fds = Vec::new();
    // SAFETY: the kernel initialized the bounded ancillary buffer. Only the
    // complete SCM_RIGHTS entries are read; even on truncation these are owned.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&message);
        while !cmsg.is_null() {
            let offset = cmsg.cast::<u8>().offset_from(control.as_ptr().cast::<u8>()) as usize;
            // msg_controllen is usize on Linux, socklen_t (u32) on Darwin.
            #[allow(clippy::unnecessary_cast)]
            let available = (message.msg_controllen as usize).min(std::mem::size_of_val(&control)).saturating_sub(offset);
            let declared = (*cmsg).cmsg_len as usize;
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let bytes = declared.min(available).saturating_sub(libc::CMSG_LEN(0) as usize);
                let data = libc::CMSG_DATA(cmsg).cast::<libc::c_int>();
                for i in 0..bytes / std::mem::size_of::<libc::c_int>() {
                    fds.push(OwnedFd::from_raw_fd(data.add(i).read_unaligned()));
                }
            }
            if declared > available || declared < libc::CMSG_LEN(0) as usize {
                break;
            }
            cmsg = libc::CMSG_NXTHDR(&message, cmsg);
        }
    }
    if message.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err("truncated FD transfer descriptors".into());
    }
    for fd in &fds {
        fcntl(fd.as_fd(), FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))
            .map_err(|err| format!("set close-on-exec on transferred descriptor: {err}"))?;
    }
    if count != 1 {
        return Err("missing FD transfer marker".into());
    }
    Ok((marker[0], fds))
}

fn send_ack(stream: &mut UnixStream) -> Result<(), String> {
    stream.write_all(&[TRANSFER_ACK]).map_err(|err| format!("write FD transfer acknowledgement: {err}"))
}

fn send_nack(stream: &mut UnixStream, rejection: &Rejection) -> Result<(), String> {
    let message = serde_json::to_vec(rejection).map_err(|err| err.to_string())?;
    if message.len() > MAX_MANIFEST_BYTES {
        return Err(format!("FD transfer rejection exceeds {MAX_MANIFEST_BYTES} bytes"));
    }
    let length = u32::try_from(message.len()).map_err(|_| "FD transfer rejection length does not fit u32".to_string())?;
    stream.write_all(&[TRANSFER_NACK]).map_err(|err| format!("write FD transfer rejection marker: {err}"))?;
    stream.write_all(&length.to_be_bytes()).map_err(|err| format!("write FD transfer rejection length: {err}"))?;
    stream.write_all(&message).map_err(|err| format!("write FD transfer rejection: {err}"))
}

fn receive_ack(stream: &mut UnixStream) -> Result<(), String> {
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
            let rejection: Rejection = serde_json::from_slice(&message).map_err(|err| format!("parse rejection: {err}"))?;
            Err(format!(
                "FD transfer rejected (sender version {}, receiver version {}): {}",
                rejection.sender_version, rejection.receiver_version, rejection.reason
            ))
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
