#![cfg(unix)]
// Adapted from ceda0b99, crates/cleat/tests/fd_transfer.rs (PR #166).
use std::{
    fs::File,
    io::{IoSlice, Read, Write},
    os::{
        fd::{AsFd, AsRawFd, OwnedFd},
        unix::net::UnixStream,
    },
    time::Duration,
};

use cleat::{
    fd_transfer,
    recording::ReplaySnapshot,
    runtime::{SessionMetadata, TerminalSize},
    transfer_manifest::{FdManifestEntry, FdRole, FdTransferManifest},
    vt::{TerminalColors, VtEngineKind},
};
use nix::sys::socket::{sendmsg, ControlMessage, MsgFlags};

fn manifest() -> FdTransferManifest {
    FdTransferManifest {
        version: 1,
        min_supported_version: 1,
        fds: [FdRole::pty_master(), FdRole::recording(), FdRole::pidfd(), FdRole::child_status(), FdRole::new("future_optional")]
            .into_iter()
            .enumerate()
            .map(|(index, role)| FdManifestEntry { index, role })
            .collect(),
        session: SessionMetadata {
            id: "session".into(),
            vt_engine: VtEngineKind::Passthrough,
            cwd: Some("/workspace".into()),
            cmd: Some("cargo test".into()),
            tags: vec!["transfer".into()],
            environment: vec![],
            record: true,
            initial_size: TerminalSize::default(),
            colors: TerminalColors::default(),
        },
        size: TerminalSize { cols: 100, rows: 30 },
        cell_pixel_size: (8, 16),
        child_pid: 42,
        hosting_epoch: 3,
        replay_snapshot: ReplaySnapshot { engine: "passthrough".into(), cols: 100, rows: 30, state: "\x1b[Hscreen".into() },
    }
}
fn pair() -> (UnixStream, UnixStream) {
    let pair = UnixStream::pair().unwrap();
    for stream in [&pair.0, &pair.1] {
        stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        stream.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
    }
    pair
}
fn is_open(fd: i32) -> bool {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fstat initializes stat on success; we only inspect its return code.
    unsafe { libc::fstat(fd, stat.as_mut_ptr()) == 0 }
}

#[test]
fn round_trip_all_roles_cloexec_and_shared_flags() {
    let (mut sender, mut receiver) = pair();
    let expected = manifest();
    let sent = expected.clone();
    let (read_end, write_end) = nix::unistd::pipe().unwrap();
    nix::fcntl::fcntl(&read_end, nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK)).unwrap();
    let send = std::thread::spawn(move || {
        fd_transfer::send(&mut sender, &sent, &vec![read_end.as_fd(); sent.fds.len()]).unwrap();
        assert!(is_open(read_end.as_raw_fd()));
    });
    let received = fd_transfer::receive(&mut receiver).unwrap();
    send.join().unwrap();
    assert_eq!(received.manifest, expected);
    for fd in received.fds() {
        assert_ne!(nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_GETFD).unwrap() & libc::FD_CLOEXEC, 0);
        assert_ne!(nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_GETFL).unwrap() & libc::O_NONBLOCK, 0);
    }
    File::from(write_end).write_all(b"arrived").unwrap();
    let (_, fds) = received.commit();
    let mut data = [0; 7];
    File::from(fds.into_iter().next().unwrap()).read_exact(&mut data).unwrap();
    assert_eq!(&data, b"arrived");
}

#[test]
fn version_nack_leaves_sender_descriptors_valid() {
    for (version, minimum) in [(2, 1), (1, 2), (0, 0)] {
        let (mut sender, mut receiver) = pair();
        let mut sent = manifest();
        sent.version = version;
        sent.min_supported_version = minimum;
        let send = std::thread::spawn(move || {
            let file = File::open("/dev/null").unwrap();
            let error = fd_transfer::send(&mut sender, &sent, &vec![file.as_fd(); sent.fds.len()]).unwrap_err();
            assert!(error.contains(&format!("sender version {version}, receiver version 1")), "{error}");
            assert!(is_open(file.as_raw_fd()));
        });
        assert!(fd_transfer::receive(&mut receiver).is_err());
        send.join().unwrap();
    }
}

fn raw_send(stream: &mut UnixStream, descriptors: &[OwnedFd], marker: u8, json: &[u8]) {
    let raw: Vec<_> = descriptors.iter().map(AsRawFd::as_raw_fd).collect();
    sendmsg::<()>(stream.as_raw_fd(), &[IoSlice::new(&[marker])], &[ControlMessage::ScmRights(&raw)], MsgFlags::empty(), None).unwrap();
    stream.write_all(&(json.len() as u32).to_be_bytes()).unwrap();
    stream.write_all(json).unwrap();
}

#[test]
fn future_schema_gets_structured_nack_before_body_decode() {
    let (mut sender, mut receiver) = pair();
    let fds = vec![File::open("/dev/null").unwrap().into()];
    raw_send(&mut sender, &fds, 1, br#"{"version":99,"min_supported_version":99}"#);
    assert!(fd_transfer::receive(&mut receiver).unwrap_err().contains("unsupported"));
    let mut prefix = [0; 5];
    sender.read_exact(&mut prefix).unwrap();
    assert_eq!(prefix[0], 2);
    let mut json = vec![0; u32::from_be_bytes(prefix[1..].try_into().unwrap()) as usize];
    sender.read_exact(&mut json).unwrap();
    let nack: fd_transfer::Rejection = serde_json::from_slice(&json).unwrap();
    assert_eq!((nack.sender_version, nack.receiver_version), (99, 1));
}

#[test]
fn cleanup_on_all_non_commit_paths() {
    // Run descriptor-number assertions in an isolated process so concurrently
    // running Rust tests cannot reuse a just-closed descriptor.
    if std::env::var_os("CLEAT_FD_CLEANUP_TEST").is_none() {
        assert!(std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "cleanup_on_all_non_commit_paths", "--nocapture"])
            .env("CLEAT_FD_CLEANUP_TEST", "1")
            .status()
            .unwrap()
            .success());
        return;
    }
    let original: Vec<OwnedFd> = (0..100).map(|_| File::open("/dev/null").unwrap().into()).collect();
    let mut invalid = manifest();
    invalid.fds[1].index = 0;
    let mut empty_role = manifest();
    empty_role.fds[0].role = FdRole::new("");
    let mut duplicate_role = manifest();
    duplicate_role.fds[1].role = FdRole::pty_master();
    let mut invalid_epoch = manifest();
    invalid_epoch.hosting_epoch = 0;
    for (case, (count, marker, json)) in [
        (5, 1, serde_json::to_vec(&manifest()).unwrap()), // guard drop
        (5, 0, serde_json::to_vec(&manifest()).unwrap()), // bad marker
        (5, 1, b"broken json".to_vec()),
        (5, 1, serde_json::to_vec(&invalid).unwrap()),
        (5, 1, serde_json::to_vec(&empty_role).unwrap()),
        (5, 1, serde_json::to_vec(&duplicate_role).unwrap()),
        (5, 1, serde_json::to_vec(&invalid_epoch).unwrap()),
        (4, 1, serde_json::to_vec(&manifest()).unwrap()),   // count mismatch
        (100, 1, serde_json::to_vec(&manifest()).unwrap()), // ancillary truncation (Linux), oversized rights (Darwin)
    ]
    .into_iter()
    .enumerate()
    {
        let (mut sender, mut receiver) = pair();
        let before: Vec<_> = (0..1024).map(is_open).collect();
        raw_send(&mut sender, &original[..count], marker, &json);
        let result = fd_transfer::receive(&mut receiver);
        assert_eq!(result.is_ok(), case == 0, "case {case}: {result:?}");
        if let Ok(received) = &result {
            assert_eq!(marker, 1);
            for fd in received.fds() {
                assert!(is_open(fd.as_raw_fd()));
            }
        }
        drop(result);
        assert_eq!(before, (0..1024).map(is_open).collect::<Vec<_>>());
    }
    // EOF/timeout after rights but before complete manifest, and failed ACK.
    for mode in ["eof", "timeout", "ack_failure", "oversize"] {
        let (mut sender, mut receiver) = pair();
        receiver.set_read_timeout(Some(Duration::from_millis(10))).unwrap();
        let mut before: Vec<_> = (0..1024).map(is_open).collect();
        if mode == "ack_failure" {
            raw_send(&mut sender, &original[..5], 1, &serde_json::to_vec(&manifest()).unwrap());
            // SHUT_RD does not reliably reject the peer's next write on
            // Darwin. Close the peer entirely to force a failed ACK.
            before[sender.as_raw_fd() as usize] = false;
            drop(sender);
        } else {
            let raw: Vec<_> = original[..5].iter().map(AsRawFd::as_raw_fd).collect();
            sendmsg::<()>(sender.as_raw_fd(), &[IoSlice::new(&[1])], &[ControlMessage::ScmRights(&raw)], MsgFlags::empty(), None).unwrap();
            if mode == "eof" {
                sender.shutdown(std::net::Shutdown::Write).unwrap();
            }
            if mode == "oversize" {
                sender.write_all(&u32::MAX.to_be_bytes()).unwrap();
            }
        }
        assert!(fd_transfer::receive(&mut receiver).is_err(), "mode {mode}");
        assert_eq!(before, (0..1024).map(is_open).collect::<Vec<_>>());
    }
}

#[test]
fn recording_append_and_child_status_descriptors_remain_usable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.cast");
    let recording = std::fs::OpenOptions::new().create(true).append(true).open(&path).unwrap();
    let (status_reader, mut status_writer) = UnixStream::pair().unwrap();
    let pty = nix::pty::openpty(None, None).unwrap();
    #[cfg(target_os = "linux")]
    let process = cleat::child_observation::pidfd_open(std::process::id()).unwrap();
    #[cfg(not(target_os = "linux"))]
    let process: OwnedFd = File::open("/dev/null").unwrap().into();
    let mut sent = manifest();
    sent.fds.pop();
    sent.child_pid = std::process::id();
    let (mut sender, mut receiver) = pair();
    let send = std::thread::spawn(move || {
        fd_transfer::send(&mut sender, &sent, &[pty.master.as_fd(), recording.as_fd(), process.as_fd(), status_reader.as_fd()]).unwrap();
    });
    let (_, mut fds) = fd_transfer::receive(&mut receiver).unwrap().commit();
    send.join().unwrap();
    let mut status = File::from(fds.pop().unwrap());
    use std::os::unix::process::ExitStatusExt;
    cleat::child_observation::forward_status(&mut status_writer, std::process::ExitStatus::from_raw(19 << 8)).unwrap();
    assert_eq!(cleat::child_observation::receive_status(&mut status).unwrap().unwrap().code(), Some(19));
    let _pidfd = fds.pop().unwrap();
    let mut recording = File::from(fds.pop().unwrap());
    assert_ne!(nix::fcntl::fcntl(&recording, nix::fcntl::FcntlArg::F_GETFL).unwrap() & libc::O_APPEND, 0);
    recording.write_all(b"transferred\n").unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), "transferred\n");
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn disconnected_peer_does_not_signal_embedded_host() {
    if std::env::var_os("CLEAT_TRANSFER_SIGPIPE_TEST").is_none() {
        assert!(std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "disconnected_peer_does_not_signal_embedded_host", "--nocapture"])
            .env("CLEAT_TRANSFER_SIGPIPE_TEST", "1")
            .status()
            .unwrap()
            .success());
        return;
    }
    // SAFETY: isolated subprocess; restore the C host's default disposition.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let (mut sender, receiver) = pair();
    drop(receiver);
    let file = File::open("/dev/null").unwrap();
    assert!(fd_transfer::send(&mut sender, &manifest(), &[file.as_fd(); 5]).is_err());
    assert!(is_open(file.as_raw_fd()));
}
