use std::{
    fs,
    io::{Read, Write},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use cleat::runtime::RuntimeLayout;

#[test]
fn no_create_preserves_session_files_when_daemon_is_unavailable() {
    let temp = tempfile::tempdir().expect("runtime root");
    let layout = RuntimeLayout::new(temp.path().to_owned());
    let session = layout.session_dir("existing");
    fs::create_dir_all(&session).expect("session directory");
    let marker = session.join("owned-state");
    fs::write(&marker, "must survive a failed attachment").expect("session marker");
    // Any attempted auto-start must fail rather than leave a test daemon behind.
    let output = Command::new(env!("CARGO_BIN_EXE_cleat"))
        .env("CARGO_BIN_EXE_cleat", temp.path().join("must-not-start"))
        .arg("--runtime-root")
        .arg(temp.path())
        .args(["attach", "existing", "--no-create", "--no-record"])
        .output()
        .expect("attach command");
    assert!(!output.status.success());
    assert_eq!(fs::read_to_string(marker).expect("attachment must preserve session files"), "must survive a failed attachment");
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("connect"), "preserve the connection failure: {error}");
    assert!(!layout.daemon_dir().join("daemon.pid").exists());
}

#[test]
fn no_create_missing_target_does_not_create_runtime_state() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("absent");
    let output = Command::new(env!("CARGO_BIN_EXE_cleat"))
        .arg("--runtime-root")
        .arg(&root)
        .args(["attach", "missing", "--no-create", "--no-record"])
        .output()
        .expect("attach command");
    assert!(!output.status.success());
    assert!(!root.exists(), "failed attachment must not create runtime state");
}

#[test]
fn no_create_preserves_state_on_denied_or_invalid_inspect_response() {
    for response in [
        "HTTP/1.1 403 Forbidden\r\nContent-Length: 18\r\nConnection: close\r\n\r\n{\"error\":\"denied\"}",
        "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nnot-json",
    ] {
        let temp = tempfile::tempdir().expect("runtime root");
        let layout = RuntimeLayout::new(temp.path().to_owned());
        let session = layout.session_dir("existing");
        fs::create_dir_all(&session).expect("session directory");
        let marker = session.join("owned-state");
        fs::write(&marker, "retained").expect("marker");
        let listener = cleat::platform::ipc::bind_session_listener(&layout.socket_path()).expect("bind endpoint");
        cleat::platform::ipc::set_listener_nonblocking(&listener, true).expect("nonblocking listener");
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept inspect: {error}"),
                }
            };
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).expect("read inspect");
                request.push(byte[0]);
            }
            assert!(request.starts_with(b"GET /sessions/existing HTTP/1.1"));
            stream.write_all(response.as_bytes()).expect("write response");
        });
        let output = Command::new(env!("CARGO_BIN_EXE_cleat"))
            .arg("--runtime-root")
            .arg(temp.path())
            .args(["attach", "existing", "--no-create", "--no-record"])
            .output()
            .expect("attach command");
        server.join().expect("server");
        assert!(!output.status.success());
        assert_eq!(fs::read_to_string(marker).expect("preserved marker"), "retained");
        assert!(!layout.daemon_dir().join("daemon.pid").exists());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("denied") || error.contains("parse HTTP response"), "{error}");
    }
}
