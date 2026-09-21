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
