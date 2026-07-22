#![cfg(unix)]

use std::{
    io::{Read, Write},
    os::{fd::AsFd, unix::net::UnixStream},
};

use cleat::{
    fd_transfer::{self, FdManifestEntry, FdTransferManifest},
    runtime::{SessionMetadata, TerminalSize},
    vt::{TerminalColors, VtEngineKind},
};

fn manifest() -> FdTransferManifest {
    FdTransferManifest {
        version: 1,
        operation: "sibling".to_string(),
        fds: vec![FdManifestEntry { index: 0, role: "pty_master".to_string() }],
        session: SessionMetadata {
            id: "helper".to_string(),
            vt_engine: VtEngineKind::Passthrough,
            cwd: Some("/workspace".into()),
            cmd: Some("cargo test".to_string()),
            tags: Vec::new(),
            record: false,
            initial_size: TerminalSize::default(),
            colors: TerminalColors::default(),
        },
        source_session: "parent".to_string(),
        target_daemon: "helper".to_string(),
        child_pid: 42,
    }
}

#[test]
fn transfers_manifest_and_working_fd_over_unix_socket() {
    let (mut sender, mut receiver) = UnixStream::pair().expect("socketpair");
    let (read_end, write_end) = nix::unistd::pipe().expect("pipe");
    let expected = manifest();
    let sent = expected.clone();

    let send = std::thread::spawn(move || fd_transfer::send(&mut sender, &sent, &[read_end.as_fd()]));
    let received = fd_transfer::receive(&mut receiver).expect("receive transfer");
    send.join().expect("join sender").expect("send transfer");

    assert_eq!(received.manifest, expected);
    assert_eq!(received.fds.len(), 1);

    std::fs::File::from(write_end).write_all(b"fd arrived").expect("write through original pipe end");
    let mut bytes = [0; 10];
    std::fs::File::from(received.fds.into_iter().next().expect("received fd")).read_exact(&mut bytes).expect("read through transferred fd");
    assert_eq!(&bytes, b"fd arrived");
}
