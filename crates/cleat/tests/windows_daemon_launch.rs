#![cfg(windows)]

use std::{process::Command, sync::mpsc, thread, time::Duration};

#[test]
fn fresh_daemon_does_not_hold_launch_output_open() {
    let root = tempfile::Builder::new().prefix("cleat capture [paths] \u{03bb} ").tempdir().expect("runtime root");
    let path = root.path().to_owned();
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let result = Command::new(env!("CARGO_BIN_EXE_cleat"))
            .arg("--runtime-root")
            .arg(path)
            .args(["--server", "capture-test", "launch", "short", "--vt", "passthrough", "--cmd", "cmd.exe /D /C exit 0", "--json"])
            .output();
        let _ = sender.send(result);
    });
    let result = receiver.recv_timeout(Duration::from_secs(5));
    // Release inherited pipe handles even on failure, so the test never hangs.
    let pid: u32 = std::fs::read_to_string(cleat::platform::daemon::daemon_pid_path(root.path(), "capture-test"))
        .expect("owned daemon pid")
        .trim()
        .parse()
        .expect("daemon pid integer");
    // The generic non-Unix termination helper is currently a no-op.
    // SAFETY: the PID came from this test's private runtime directory; the
    // process handle is checked before use and closed exactly once.
    unsafe {
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE},
        };
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        assert!(!handle.is_null(), "open owned daemon");
        let terminated = TerminateProcess(handle, 0);
        CloseHandle(handle);
        assert_ne!(terminated, 0, "terminate owned daemon");
    }
    drop(worker);
    let output = result.expect("launch output must complete while the daemon is alive").expect("launch command");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("launch JSON");
    assert_eq!(value["id"], "short");
}

struct OwnedDaemon {
    root: std::path::PathBuf,
    name: &'static str,
}

impl Drop for OwnedDaemon {
    fn drop(&mut self) {
        let Ok(pid) = std::fs::read_to_string(cleat::platform::daemon::daemon_pid_path(&self.root, self.name)) else { return };
        let Ok(pid) = pid.trim().parse::<u32>() else { return };
        // SAFETY: the private runtime directory identifies this test's daemon;
        // check the process handle and close it exactly once.
        unsafe {
            use windows_sys::Win32::{
                Foundation::CloseHandle,
                System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE},
            };
            let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if !handle.is_null() {
                TerminateProcess(handle, 0);
                CloseHandle(handle);
            }
        }
    }
}

// Scenario: CLI → auto-started contaminated daemon → real ConPTY/CreateProcessW
// launch. cmd /D disables AutoRun, so `set` observes the initial shell environment.
#[test]
fn declared_launch_preserves_windows_baseline_and_excludes_daemon_environment() {
    let temp = tempfile::tempdir().expect("runtime root");
    let root = temp.path().join("runtime");
    let _daemon = OwnedDaemon { root: root.clone(), name: "declared-test" };
    let system_root = std::env::var("SystemRoot").expect("Windows installation directory");
    let path = std::env::var("PATH").expect("Windows host PATH");
    let output = Command::new(env!("CARGO_BIN_EXE_cleat"))
        .arg("--runtime-root")
        .arg(&root)
        .args(["--server", "declared-test", "launch", "declared", "--vt", "passthrough", "--env-clear", "--cwd"])
        .arg(temp.path())
        .args([
            "--env",
            &format!("SystemRoot={system_root}"),
            "--env",
            &format!("PATH={path}"),
            "--env",
            "CLAUDECODE=adapter-owned",
            "--env",
            "EMPTY=",
            "--cmd",
            "set > environment.txt & echo done > ready",
            "--json",
        ])
        .env("CLEAT_ENV_SENTINEL", "daemon-only")
        .env("NO_COLOR", "daemon-only")
        .env("CLAUDECODE", "daemon-only")
        .env("CLAUDE_CODE_MESSAGING_SOCKET", "daemon-only")
        .env("CLAUDE_CODE_MESSAGING_TOKEN", "daemon-only")
        .output()
        .expect("launch child");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let capture = temp.path().join("environment.txt");
    let contents = loop {
        if let Ok(contents) = std::fs::read_to_string(&capture) {
            if temp.path().join("ready").exists() {
                break contents;
            }
        }
        assert!(std::time::Instant::now() < deadline, "child did not capture environment");
        thread::sleep(Duration::from_millis(10));
    };
    let entries: Vec<_> = contents.lines().map(str::to_ascii_lowercase).collect();
    assert!(!contents.contains("daemon-only"), "{contents}");
    assert!(entries.contains(&format!("systemroot={}", system_root.to_ascii_lowercase())));
    assert!(entries.contains(&format!("path={}", path.to_ascii_lowercase())));
    assert!(entries.contains(&"claudecode=adapter-owned".to_owned()));
    assert!(entries.contains(&"cleat_session=declared".to_owned()));
    assert!(entries.contains(&"term=dumb".to_owned()));
    // Empty declarations are checked directly by the native-block contract.
}
