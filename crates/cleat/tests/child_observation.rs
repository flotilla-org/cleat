#![cfg(any(target_os = "linux", target_os = "macos"))]
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::process::ExitStatusExt,
    process::{Command, ExitStatus, Stdio},
    time::Duration,
};

use cleat::child_observation::{forward_status, receive_status, ChildObserver};

#[test]
fn observes_exit_without_reaping() {
    let mut child = Command::new("sh").args(["-c", "read line; exit 23"]).stdin(Stdio::piped()).spawn().unwrap();
    let observer = ChildObserver::new(child.id()).unwrap();
    assert_eq!(observer.wait(Duration::from_millis(10)).unwrap_err().kind(), std::io::ErrorKind::TimedOut);
    child.stdin.take().unwrap().write_all(b"exit\n").unwrap();
    let status = observer.wait(Duration::from_secs(5)).unwrap().expect("own child status");
    assert_eq!(status.code(), Some(23));
    assert_eq!(child.wait().unwrap().code(), Some(23));
}

#[test]
fn non_child_status_is_real_or_explicitly_unknown() {
    // The shell owns the target; a FIFO keeps it alive until registration.
    let dir = tempfile::tempdir().unwrap();
    let fifo = dir.path().join("release");
    nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR).unwrap();
    let mut parent = Command::new("sh")
        .args(["-c", "sh -c 'read line < \"$1\"; exit 37' sh \"$1\" & echo $!; wait", "sh"])
        .arg(&fifo)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(parent.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let observer = ChildObserver::new(line.trim().parse().unwrap()).unwrap();
    std::fs::OpenOptions::new().write(true).open(&fifo).unwrap().write_all(b"exit\n").unwrap();
    if let Some(status) = observer.wait(Duration::from_secs(5)).unwrap() {
        assert_eq!(status.code(), Some(37));
    }
    parent.wait().unwrap();
}

#[test]
fn forwarding_preserves_exit_signals_and_source_death_is_unknown() {
    for raw in [23 << 8, libc::SIGTERM, libc::SIGABRT | 0x80] {
        let mut bytes = vec![];
        forward_status(&mut bytes, ExitStatus::from_raw(raw)).unwrap();
        assert_eq!(receive_status(&mut bytes.as_slice()).unwrap().unwrap().into_raw(), raw);
    }
    assert!(receive_status(&mut &b""[..]).unwrap().is_none());
    assert!(receive_status(&mut &b"\0\0"[..]).unwrap().is_none());
}
