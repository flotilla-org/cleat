//! Daemon-to-daemon Transfer (#254). Every daemon here is started by the CLI
//! under a private runtime root, so the environment each command passes is
//! the environment its auto-started daemon inherits.
#![cfg(unix)]

use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{Duration, Instant},
};

use cleat::{
    asciicast::EventCode,
    cast_reader::read_all_events_since,
    protocol::{InspectResult, TransferResult},
};

struct Root {
    temp: tempfile::TempDir,
    env: Vec<(String, String)>,
}

impl Root {
    fn new() -> Self {
        let mut env = Vec::new();
        // Without a functional VT the replay-probe engine stands in: it
        // produces replay snapshots, so transfer works on no-VT builds too.
        if !cfg!(feature = "ghostty-vt") {
            env.push(("CLEAT_TEST_VT_ENGINE".to_string(), "replay-probe".to_string()));
        }
        Self { temp: tempfile::tempdir().unwrap(), env }
    }

    fn path(&self) -> &Path {
        self.temp.path()
    }

    fn command(&self, args: &[&str], extra_env: &[(&str, &str)]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cleat"));
        command.arg("--runtime-root").arg(self.path()).env_remove("CLEAT_DAEMON").env_remove("CLEAT_SESSION").args(args);
        for (key, value) in &self.env {
            command.env(key, value);
        }
        for (key, value) in extra_env {
            command.env(key, value);
        }
        command
    }

    fn run_with(&self, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
        self.command(args, extra_env).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        self.ok_with(args, &[])
    }

    fn ok_with(&self, args: &[&str], extra_env: &[(&str, &str)]) -> String {
        let output = self.run_with(args, extra_env);
        assert!(output.status.success(), "cleat {args:?} failed: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn err_with(&self, args: &[&str], extra_env: &[(&str, &str)]) -> String {
        let output = self.run_with(args, extra_env);
        assert!(!output.status.success(), "cleat {args:?} unexpectedly succeeded: {}", String::from_utf8_lossy(&output.stdout));
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    fn launch_shell(&self, id: &str) {
        let mut args = vec!["launch", id, "--cmd", "sh"];
        if !cfg!(feature = "ghostty-vt") {
            args.extend(["--vt", "passthrough"]);
        }
        self.ok(&args);
    }

    fn inspect(&self, args: &[&str]) -> InspectResult {
        let mut all = args.to_vec();
        all.push("--json");
        serde_json::from_str(&self.ok(&all)).unwrap()
    }

    fn transfer(&self, id: &str, to: &str, extra: &[&str], extra_env: &[(&str, &str)]) -> Result<TransferResult, String> {
        let mut args = vec!["transfer", id, "--to", to, "--json"];
        args.extend_from_slice(extra);
        let output = self.run_with(&args, extra_env);
        if output.status.success() {
            Ok(serde_json::from_slice(&output.stdout).unwrap())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).into_owned())
        }
    }

    fn session_dir(&self, daemon: &str, id: &str) -> PathBuf {
        self.path().join(daemon).join("sessions").join(id)
    }

    fn cast(&self, daemon: &str, id: &str) -> PathBuf {
        self.session_dir(daemon, id).join("session.cast")
    }

    fn listed(&self, daemon: &str) -> String {
        self.ok(&["--server", daemon, "list"])
    }

    fn daemon_pid(&self, daemon: &str) -> i32 {
        std::fs::read_to_string(self.path().join(daemon).join("daemon.pid")).unwrap().trim().parse().unwrap()
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        // Stop every daemon this test started; sessions die with them.
        let Ok(entries) = std::fs::read_dir(self.path()) else { return };
        for entry in entries.flatten() {
            if let Ok(pid) = std::fs::read_to_string(entry.path().join("daemon.pid")) {
                if let Ok(pid) = pid.trim().parse::<i32>() {
                    if pid != std::process::id() as i32 {
                        let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGTERM);
                    }
                }
            }
        }
    }
}

fn wait_until(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn recorded_output(cast: &Path) -> String {
    read_all_events_since(cast, 0)
        .map(|events| events.into_iter().filter(|event| event.code == EventCode::Output).map(|event| event.data).collect())
        .unwrap_or_default()
}

fn markers(cast: &Path) -> Vec<serde_json::Value> {
    read_all_events_since(cast, 0)
        .unwrap()
        .into_iter()
        .filter(|event| event.code == EventCode::Marker)
        .filter_map(|event| serde_json::from_str(&event.data).ok())
        .collect()
}

fn wait_for_output(cast: &Path, text: &str) {
    wait_until(&format!("{text:?} in {}", cast.display()), Duration::from_secs(10), || recorded_output(cast).contains(text));
}

#[test]
fn transfer_moves_a_live_shell_between_daemons() {
    let root = Root::new();
    root.launch_shell("moving");
    root.ok(&["send", "moving", "echo before-$((40+2))"]);
    wait_for_output(&root.cast("default@1", "moving"), "before-42");
    let before = root.inspect(&["inspect", "moving"]);
    assert_eq!(before.hosting_epoch, 1);

    let result = root.transfer("moving", "other", &[], &[]).unwrap();
    assert_eq!(result.address, "daemon:other@1");
    assert_eq!(result.hosting_epoch, 2);
    assert!(result.dropped_clients.is_empty());

    // The id resolves to its new host even through the old daemon's name.
    let after = root.inspect(&["inspect", "moving"]);
    assert_eq!(after.hosting_epoch, 2);
    assert_eq!(after.generation, Some(1));
    assert_eq!(after.process.leader_pid, before.process.leader_pid, "the shell keeps running");
    assert_eq!(root.inspect(&["--server", "other", "inspect", "moving"]).hosting_epoch, 2);
    assert!(!root.listed("default").contains("moving"), "the source no longer lists it");
    assert!(root.listed("other").contains("moving"));
    assert!(!root.session_dir("default@1", "moving").exists());

    // Typing continues on the new host, into the same recording.
    root.ok(&["send", "moving", "echo after-$((50+5))"]);
    let cast = root.cast("other@1", "moving");
    wait_for_output(&cast, "after-55");
    let output = recorded_output(&cast);
    assert!(output.find("before-42").unwrap() < output.find("after-55").unwrap());
    let header_lines = std::fs::read_to_string(&cast).unwrap().lines().filter(|line| line.starts_with('{')).count();
    assert_eq!(header_lines, 1, "one recording across the move");
    assert_eq!(markers(&cast), vec![serde_json::json!({"event": "transferred", "epoch": 2, "address": "daemon:other@1"})]);
    if cfg!(feature = "ghostty-vt") {
        let screen = root.ok(&["capture", "moving"]);
        assert!(screen.contains("before-42") && screen.contains("after-55"), "{screen}");
    }
    root.ok(&["kill", "moving"]);
}

/// An attached packet watcher, driven directly over the daemon protocol.
struct Watcher {
    client: cleat::packet::PacketClient<std::os::unix::net::UnixStream>,
}

impl Watcher {
    const CHANNEL: u32 = 1;

    fn attach(root: &Root, id: &str) -> Self {
        let service =
            cleat::server::SessionService::new(cleat::runtime::RuntimeLayout::new(root.path().to_path_buf())).for_session(id).unwrap();
        let (mut client, _) = service.connect_packets(id).unwrap();
        client.get_ref().set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        client.open_channel(Self::CHANNEL, id, cleat::packet::ChannelRole::Watcher).unwrap();
        let mut watcher = Self { client };
        watcher.render();
        watcher
    }

    /// Read until the next render on the channel, acknowledging it.
    fn render(&mut self) -> cleat::packet::RenderPacket {
        let render = self.client.read_render(Self::CHANNEL).expect("channel stays open");
        self.client.ack(Self::CHANNEL, render.update.render_generation).unwrap();
        render
    }

    /// Read until the channel closes, returning the redirect that preceded it.
    fn redirected(&mut self) -> cleat::packet::SessionRedirect {
        let mut redirect = None;
        loop {
            let frame = self.client.read_frame().expect("redirect before close");
            match (frame.channel, frame.msg_type) {
                (cleat::packet::CHANNEL_CONTROL, cleat::packet::MSG_CONTROL_REDIRECT) => {
                    let message = frame.decode::<cleat::packet::ChannelRedirect>().unwrap();
                    assert_eq!(message.channel, Self::CHANNEL);
                    redirect = Some(message.redirect);
                }
                (cleat::packet::CHANNEL_CONTROL, cleat::packet::MSG_CONTROL_ERROR) => {
                    let error = frame.decode::<cleat::packet::ControlError>().unwrap();
                    if error.channel == Self::CHANNEL {
                        assert!(error.message.contains("moved to"), "{}", error.message);
                        return redirect.expect("redirect precedes the close");
                    }
                }
                (channel, cleat::packet::MSG_SESSION_RENDER) if channel == Self::CHANNEL => {
                    let render = frame.decode::<cleat::packet::RenderPacket>().unwrap();
                    self.client.ack(Self::CHANNEL, render.update.render_generation).unwrap();
                }
                _ => {}
            }
        }
    }
}

/// The session is still hosted where it was, unchanged, and still live.
fn assert_left_in_place(root: &Root, id: &str, before: &InspectResult, probe: &str) {
    let now = root.inspect(&["--server", "default@1", "inspect", id]);
    assert_eq!(now.hosting_epoch, 1);
    assert_eq!(now.process.leader_pid, before.process.leader_pid);
    assert!(root.listed("default").contains(id));
    let cast = root.cast("default@1", id);
    assert!(markers(&cast).is_empty(), "no transferred marker: {:?}", markers(&cast));
    root.ok(&["send", id, &format!("echo {probe}-$((1+1))")]);
    wait_for_output(&cast, &format!("{probe}-2"));
}

fn launch_with(root: &Root, id: &str, env: &[(&str, &str)]) {
    let mut launch = vec!["launch", id, "--cmd", "sh"];
    if !cfg!(feature = "ghostty-vt") {
        launch.extend(["--vt", "passthrough"]);
    }
    root.ok_with(&launch, env);
}

#[test]
fn a_manifest_version_nack_leaves_the_session_and_its_clients_undisturbed() {
    let root = Root::new();
    // The source daemon sends a manifest version outside the target's window.
    launch_with(&root, "kept", &[("CLEAT_TEST_TRANSFER_MANIFEST_VERSION", "99")]);
    let before = root.inspect(&["inspect", "kept"]);
    let mut watcher = Watcher::attach(&root, "kept");

    let err = root.transfer("kept", "other", &[], &[]).unwrap_err();
    assert!(err.contains("unsupported manifest version 99"), "{err}");
    assert_left_in_place(&root, "kept", &before, "after-version-nack");
    watcher.render();
    assert!(root.listed("other").is_empty());
}

#[test]
fn an_adoption_nack_leaves_the_session_and_its_clients_undisturbed() {
    let root = Root::new();
    launch_with(&root, "kept", &[]);
    let before = root.inspect(&["inspect", "kept"]);
    let mut watcher = Watcher::attach(&root, "kept");

    let err = root.transfer("kept", "picky", &[], &[("CLEAT_TEST_TRANSFER_REFUSE_ADOPTION", "not today")]).unwrap_err();
    assert!(err.contains("refused adoption: not today"), "{err}");
    assert_left_in_place(&root, "kept", &before, "after-adoption-nack");
    watcher.render();
    assert!(root.listed("picky").is_empty());
}

/// A daemon that answers the transfer probe, then accepts every further
/// connection and never says another word.
struct StallingTarget {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl StallingTarget {
    fn start(root: &Root, name: &str) -> Self {
        let dir = root.path().join(format!("{name}@1"));
        std::fs::create_dir_all(dir.join("sessions")).unwrap();
        // No registration: a daemon without a pid file counts as starting up,
        // so the CLI routes to this socket instead of starting a real one.
        std::os::unix::fs::symlink(format!("{name}@1"), root.path().join(name)).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(dir.join("socket")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let mut held = Vec::new();
            while !stopping.load(std::sync::atomic::Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        stream.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
                        let mut request = Vec::new();
                        let mut byte = [0u8; 1];
                        while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
                            request.push(byte[0]);
                        }
                        if request.starts_with(b"GET / ") {
                            let body = serde_json::json!({
                                "service": "cleat-session",
                                "packet_protocol": {
                                    "version": cleat::packet::PROTOCOL_VERSION,
                                    "min_supported_version": cleat::packet::PROTOCOL_VERSION,
                                },
                            })
                            .to_string();
                            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                        } else {
                            held.push(stream);
                        }
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        });
        Self { stop, thread: Some(thread) }
    }
}

impl Drop for StallingTarget {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[test]
fn a_stalled_target_never_blocks_the_servicing_loop_and_leaves_the_session() {
    let root = Root::new();
    root.launch_shell("stuck");
    root.launch_shell("bystander");
    let before = root.inspect(&["inspect", "stuck"]);
    let _target = StallingTarget::start(&root, "stall");

    let started = Instant::now();
    let transfer = root
        .command(&["transfer", "stuck", "--to", "stall", "--timeout", "3s"], &[])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // While the handshake stalls, the source keeps serving every session.
    std::thread::sleep(Duration::from_millis(300));
    let mut probes = 0;
    while started.elapsed() < Duration::from_millis(2500) {
        let begun = Instant::now();
        root.ok(&["inspect", "stuck", "--json"]);
        root.ok(&["send", "bystander", "echo alive-$((2+3))"]);
        assert!(begun.elapsed() < Duration::from_secs(1), "servicing loop stalled for {:?}", begun.elapsed());
        probes += 1;
    }
    assert!(probes >= 3);
    wait_for_output(&root.cast("default@1", "bystander"), "alive-5");
    // Control operations wait while the session is frozen.
    let err = root.err_with(&["tag", "stuck", "+frozen"], &[]);
    assert!(err.contains("transferring"), "{err}");

    let output = transfer.wait_with_output().unwrap();
    assert!(!output.status.success());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("timed out"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_left_in_place(&root, "stuck", &before, "after-stall");
    root.ok(&["tag", "stuck", "+thawed"]);
}

#[test]
fn stale_holders_are_refused_and_never_reach_the_pty() {
    let root = Root::new();
    root.launch_shell("fenced");
    let source_socket = root.path().join("default@1").join("socket");
    root.transfer("fenced", "other", &[], &[]).unwrap();

    let err = root.err_with(&["--hosting-epoch", "1", "send-keys", "-l", "fenced", "stalemarker"], &[]);
    assert!(err.contains("stale holder") && err.contains("epoch 2"), "{err}");

    // The old host keeps answering with the redirect during its grace window.
    let raw = |epoch: Option<u64>| {
        use std::io::{Read, Write};
        let mut stream = std::os::unix::net::UnixStream::connect(&source_socket).unwrap();
        let body = br#"{"bytes":[115,116,97,108,101]}"#;
        let epoch = epoch.map(|epoch| format!("x-cleat-hosting-epoch: {epoch}\r\n")).unwrap_or_default();
        write!(stream, "POST /sessions/fenced/keys HTTP/1.1\r\nHost: cleat\r\nContent-Length: {}\r\n{epoch}\r\n", body.len()).unwrap();
        stream.write_all(body).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    };
    let response = raw(Some(1));
    assert!(response.starts_with("HTTP/1.1 421"), "{response}");
    assert!(response.contains(r#""stale_holder":true"#) && response.contains("daemon:other@1"), "{response}");
    let response = raw(None);
    assert!(response.starts_with("HTTP/1.1 421") && response.contains(r#""stale_holder":false"#), "{response}");

    root.ok(&["--hosting-epoch", "2", "send", "fenced", "echo current-$((3+4))"]);
    let cast = root.cast("other@1", "fenced");
    wait_for_output(&cast, "current-7");
    let recorded: String = read_all_events_since(&cast, 0).unwrap().into_iter().map(|event| event.data).collect();
    assert!(!recorded.contains("stalemarker") && !recorded.contains("stale"), "stale input reached the PTY: {recorded}");
}

fn wait_for_exit(cast: &Path) -> cleat::asciicast::Event {
    let mut exit = None;
    wait_until("the session's exit", Duration::from_secs(10), || {
        exit = read_all_events_since(cast, 0).ok().and_then(|events| {
            events.into_iter().rev().find(|event| {
                event.code == EventCode::Exit || (event.code == EventCode::Marker && event.data.contains(r#""event":"exit""#))
            })
        });
        exit.is_some()
    });
    exit.unwrap()
}

#[test]
fn child_exit_after_the_move_records_the_forwarded_status() {
    let root = Root::new();
    root.launch_shell("exiting");
    root.transfer("exiting", "other", &[], &[]).unwrap();
    root.ok(&["send", "exiting", "exit 7"]);
    let exit = wait_for_exit(&root.cast("other@1", "exiting"));
    assert_eq!((exit.code, exit.data.as_str()), (EventCode::Exit, "7"));
}

#[test]
fn child_exit_after_the_source_died_records_status_unknown() {
    let root = Root::new();
    root.launch_shell("orphaned");
    root.transfer("orphaned", "other", &[], &[]).unwrap();
    let source = root.daemon_pid("default@1");
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(source), nix::sys::signal::Signal::SIGKILL).unwrap();
    wait_until("the source daemon to die", Duration::from_secs(5), || {
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(source), None).is_err()
            || std::fs::read_to_string(format!("/proc/{source}/stat")).is_ok_and(|stat| stat.contains(") Z "))
    });
    root.ok(&["send", "orphaned", "exit 7"]);
    let exit = wait_for_exit(&root.cast("other@1", "orphaned"));
    assert_eq!(exit.code, EventCode::Marker);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&exit.data).unwrap(), serde_json::json!({"event": "exit", "status": "unknown"}));
}

#[test]
fn the_compatibility_gate_refuses_then_drops_incompatible_clients() {
    let root = Root::new();
    root.launch_shell("gated");
    let before = root.inspect(&["inspect", "gated"]);
    let mut watcher = Watcher::attach(&root, "gated");
    let newer = [("CLEAT_TEST_PACKET_PROTOCOL_VERSION", "99")];

    let err = root.transfer("gated", "newer", &[], &newer).unwrap_err();
    assert!(err.contains("1 attached client(s) cannot follow") && err.contains("watcher") && err.contains("--drop-incompatible"), "{err}");
    assert_left_in_place(&root, "gated", &before, "after-gate");
    watcher.render();

    let result = root.transfer("gated", "newer", &["--drop-incompatible"], &newer).unwrap();
    assert_eq!(result.dropped_clients.len(), 1, "{result:?}");
    let redirect = watcher.redirected();
    assert_eq!(redirect.address, "daemon:newer@1");
    assert_eq!(redirect.hosting_epoch, 2);
    let reason = redirect.incompatibility(cleat::packet::PROTOCOL_VERSION).expect("the client cannot follow");
    assert!(reason.contains("speaking protocol 99"), "{reason}");
}
