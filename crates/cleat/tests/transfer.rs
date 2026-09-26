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
