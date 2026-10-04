#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

fn client(verb: &str, socket: &Path, directory: &Path) -> Command {
    let trap = directory.join("spawn-trap");
    // A true process boundary: any attempted daemon start executes this probe.
    fs::write(&trap, "#!/bin/sh\nprintf spawned > \"$CLEAT_TEST_SPAWN_MARKER\"\nexit 1\n").unwrap();
    fs::set_permissions(&trap, fs::Permissions::from_mode(0o700)).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_cleat"));
    command
        .current_dir(directory)
        .args([verb, "--socket"])
        .arg(socket)
        .arg("remote")
        .env("CARGO_BIN_EXE_cleat", trap)
        .env("CLEAT_TEST_SPAWN_MARKER", directory.join("spawned"))
        .env("CLEAT_RUNTIME_DIR", directory.join("absent-runtime"))
        // Inapplicable and deliberately invalid local coordinates must never
        // be resolved when the endpoint is explicit.
        .env("CLEAT_DAEMON", "invalid/daemon")
        .env("CLEAT_SESSION", "invalid/session")
        .env("CLEAT_OUTPUT_DAEMON", "invalid/physical")
        .env_remove("SSH_CONNECTION")
        .env_remove("SSH_CLIENT");
    command
}

fn no_local_state(directory: &Path) {
    assert!(!directory.join("spawned").exists(), "socket mode must never spawn a daemon");
    assert!(!directory.join("absent-runtime").exists(), "socket mode must never create a runtime layout");
}

// Both public commands must fail at the socket boundary without consulting
// invalid ambient coordinates or spawning a replacement daemon.
#[test]
fn unavailable_socket_never_spawns_or_creates_runtime() {
    for verb in ["attach", "packets"] {
        for managed in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let socket = directory.path().join("missing.sock");
            let mut command = client(verb, &socket, directory.path());
            if !managed {
                command.env_remove("CLEAT_DAEMON").env_remove("CLEAT_SESSION").env_remove("CLEAT_OUTPUT_DAEMON");
            }
            let output = command.output().unwrap();
            assert!(!output.status.success());
            no_local_state(directory.path());
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(error.contains("connect") && error.contains("missing.sock"), "{error}");
        }
    }
}

#[cfg(feature = "ghostty-vt")]
mod live {
    use std::{
        io::Write,
        process::{Child, Stdio},
        thread,
        time::{Duration, Instant},
    };

    struct Client(Child);
    impl Client {
        fn wait(&mut self) -> std::process::ExitStatus {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(status) = self.0.try_wait().unwrap() {
                    return status;
                }
                assert!(Instant::now() < deadline, "socket client did not exit promptly");
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
    impl Drop for Client {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    use cleat::{runtime::RuntimeLayout, server::SessionService, vt::VtEngineKind};

    use super::*;

    struct Daemon {
        _root: tempfile::TempDir,
        layout: RuntimeLayout,
        service: SessionService,
        pid: i32,
    }
    impl Daemon {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let layout = RuntimeLayout::new(root.path().to_owned());
            let service = SessionService::new(layout.clone());
            service.create(Some("remote".into()), Some(VtEngineKind::Ghostty), None, Some("cat".into()), false).unwrap();
            let pid = fs::read_to_string(layout.daemon_pid_path()).unwrap().trim().parse().unwrap();
            Self { _root: root, layout, service, pid }
        }
        fn expose(&self, directory: &Path) -> std::path::PathBuf {
            let socket = directory.join("forward.sock");
            std::os::unix::fs::symlink(self.layout.socket_path(), &socket).unwrap();
            socket
        }
        fn stop(&self) {
            // The test owns this isolated daemon; interrupt its transport.
            unsafe { libc::kill(self.pid, libc::SIGKILL) };
        }
    }
    impl Drop for Daemon {
        fn drop(&mut self) {
            let _ = self.service.kill("remote");
            self.stop();
        }
    }

    fn eventually(description: &str, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "daemon/client did not reach {description}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    // A socket exposed outside the daemon's runtime is sufficient for packets.
    // Two renders exercise the ACK that permits subsequent output, and a
    // daemon-start probe proves the client never starts a shadow daemon.
    #[test]
    fn packets_from_empty_directory_receive_and_ack_renders() {
        let daemon = Daemon::new();
        let directory = tempfile::tempdir().unwrap();
        let socket = daemon.expose(directory.path());
        let stdout = directory.path().join("stdout");
        let stderr = directory.path().join("stderr");
        let mut child = Client(
            client("packets", &socket, directory.path())
                .args(["--count", "2"])
                .stdout(fs::File::create(&stdout).unwrap())
                .stderr(fs::File::create(&stderr).unwrap())
                .spawn()
                .unwrap(),
        );
        eventually("channel opened", || !daemon.service.inspect("remote").unwrap().attachments.is_empty());
        daemon.service.send_keys("remote", b"packet-input\n").unwrap();
        assert!(child.wait().success());
        assert_eq!(fs::read_to_string(stdout).unwrap().lines().count(), 2);
        assert!(fs::read_to_string(stderr).unwrap().contains("cycle protection does not cover remote relationships"));
        no_local_state(directory.path());
    }

    // Attach opens a controller, relays input and acknowledges output without
    // local metadata. Daemon death is a clear error, with no reconnect/spawn.
    #[test]
    fn attach_from_empty_directory_relays_input_and_exits_on_disconnect() {
        let daemon = Daemon::new();
        let directory = tempfile::tempdir().unwrap();
        let socket = daemon.expose(directory.path());
        let stderr = directory.path().join("stderr");
        let stdout = directory.path().join("stdout");
        let mut child = Client(
            client("attach", &socket, directory.path())
                .stdin(Stdio::piped())
                .stdout(fs::File::create(&stdout).unwrap())
                .stderr(fs::File::create(&stderr).unwrap())
                .spawn()
                .unwrap(),
        );
        eventually("controller opened", || daemon.service.inspect("remote").unwrap().attachments.iter().any(|a| a.role == "controller"));
        child.0.stdin.as_mut().unwrap().write_all(b"socket-input\n").unwrap();
        eventually("input rendered", || {
            // The renderer emits SGR per cell. Observe text through those CSI
            // sequences rather than asserting on an implementation's escapes.
            let bytes = fs::read(&stdout).unwrap();
            let mut text = Vec::new();
            let mut iter = bytes.into_iter().peekable();
            while let Some(byte) = iter.next() {
                if byte == 0x1b && iter.peek() == Some(&b'[') {
                    iter.next();
                    for code in iter.by_ref() {
                        if (0x40..=0x7e).contains(&code) {
                            break;
                        }
                    }
                } else {
                    text.push(byte);
                }
            }
            String::from_utf8_lossy(&text).contains("socket-input")
        });
        daemon.stop();
        assert!(!child.wait().success());
        let diagnostic = fs::read_to_string(stderr).unwrap();
        assert!(diagnostic.contains("socket disconnected"), "{diagnostic}");
        assert!(diagnostic.contains("fresh connect"), "{diagnostic}");
        assert!(diagnostic.contains("cycle protection does not cover remote relationships"), "{diagnostic}");
        no_local_state(directory.path());
    }

    // A packets client awaiting another render terminates on transport loss;
    // it must not replay/reconnect or create a local replacement daemon.
    #[test]
    fn packets_disconnect_requires_fresh_connect() {
        let daemon = Daemon::new();
        let directory = tempfile::tempdir().unwrap();
        let socket = daemon.expose(directory.path());
        let stderr = directory.path().join("stderr");
        let mut child = Client(
            client("packets", &socket, directory.path())
                .args(["--count", "1000"])
                .stdout(Stdio::null())
                .stderr(fs::File::create(&stderr).unwrap())
                .spawn()
                .unwrap(),
        );
        eventually("channel opened", || !daemon.service.inspect("remote").unwrap().attachments.is_empty());
        daemon.stop();
        assert!(!child.wait().success());
        let diagnostic = fs::read_to_string(stderr).unwrap();
        assert!(diagnostic.contains("socket packets:") && diagnostic.contains("fresh connect"), "{diagnostic}");
        no_local_state(directory.path());
    }

    // A deliberate detach succeeds, unlike unexpected transport loss.
    #[test]
    fn socket_attach_deliberate_detach_succeeds() {
        let daemon = Daemon::new();
        let directory = tempfile::tempdir().unwrap();
        let socket = daemon.expose(directory.path());
        let mut child = Client(
            client("attach", &socket, directory.path()).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap(),
        );
        eventually("controller opened", || !daemon.service.inspect("remote").unwrap().attachments.is_empty());
        child.0.stdin.as_mut().unwrap().write_all(b"\x1dd").unwrap();
        assert!(child.wait().success());
        no_local_state(directory.path());
    }

    // Transfer closes a socket-only attachment. It cannot follow a redirect
    // using daemon-local runtime coordinates or reconnect to another endpoint.
    #[test]
    fn socket_attach_does_not_follow_transfer_redirect() {
        let daemon = Daemon::new();
        let target = daemon.service.with_daemon("target".into()).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let socket = daemon.expose(directory.path());
        let stderr = directory.path().join("stderr");
        let mut child = Client(
            client("attach", &socket, directory.path())
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(fs::File::create(&stderr).unwrap())
                .spawn()
                .unwrap(),
        );
        eventually("controller opened", || !daemon.service.inspect("remote").unwrap().attachments.is_empty());
        daemon.service.transfer("remote", &target, Default::default()).unwrap();
        let status = child.wait();
        let diagnostic = fs::read_to_string(stderr).unwrap();
        // Clean the transferred child and destination daemon before asserting.
        let attachments = target.inspect("remote").unwrap().attachments;
        target.kill("remote").unwrap();
        let target_layout = daemon.layout.clone().with_daemon("target".into()).unwrap();
        let pid: i32 = fs::read_to_string(target_layout.daemon_pid_path()).unwrap().trim().parse().unwrap();
        unsafe { libc::kill(pid, libc::SIGKILL) };
        assert!(!status.success());
        assert!(diagnostic.contains("socket channel closed") && diagnostic.contains("fresh connect"), "{diagnostic}");
        assert!(attachments.is_empty(), "socket-only client must not reconnect at the transfer target");
        no_local_state(directory.path());
    }

    // Missing remote sessions are refused by the daemon directory; attach
    // never creates a session even though local attach defaults to creation.
    #[test]
    fn socket_attach_does_not_create_missing_session() {
        let daemon = Daemon::new();
        // Keep a different live session so the daemon remains available.
        daemon.service.create(Some("other".into()), Some(VtEngineKind::Ghostty), None, Some("cat".into()), false).unwrap();
        daemon.service.kill("remote").unwrap();
        let directory = tempfile::tempdir().unwrap();
        let socket = daemon.expose(directory.path());
        let output = client("attach", &socket, directory.path()).output().unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("not present in packet directory"));
        assert_eq!(daemon.service.list().unwrap().len(), 1);
        no_local_state(directory.path());
        daemon.service.kill("other").unwrap();
    }
}
