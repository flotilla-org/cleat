use cleat::{runtime::RuntimeLayout, server::SessionService, vt::VtEngineKind};

fn launch(service: &SessionService, id: &str) {
    let command = if cfg!(windows) { "cmd.exe /D /C ping -n 120 127.0.0.1 >NUL" } else { "sleep 120" };
    service.create(Some(id.into()), Some(VtEngineKind::Passthrough), None, Some(command.into()), true).unwrap();
}

fn cli(root: &std::path::Path, arguments: &[&str]) -> serde_json::Value {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_cleat"))
        .arg("--runtime-root")
        .arg(root)
        .env_remove("CLEAT_DAEMON")
        .env_remove("CLEAT_SESSION")
        .args(arguments)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn alias_reads_across_live_generations_and_creates_on_current() {
    let temp = tempfile::tempdir().unwrap();
    let layout = RuntimeLayout::new(temp.path().to_path_buf());
    let service = SessionService::new(layout.clone());
    launch(&service, "old");
    let old = service.with_daemon("default@1".into()).unwrap();
    let new = service.with_daemon("default@2".into()).unwrap();
    launch(&new, "new");
    layout.set_current_generation(2).unwrap();
    assert_eq!(cli(temp.path(), &["inspect", "old", "--json"])["generation"], 1);
    assert_eq!(cli(temp.path(), &["inspect", "new", "--json"])["generation"], 2);
    assert_eq!(cli(temp.path(), &["inspect", "old", "--json"])["hosting_epoch"], 1);
    assert_eq!(cli(temp.path(), &["--server", "default@1", "inspect", "old", "--json"])["generation"], 1);
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_cleat"))
        .arg("--runtime-root")
        .arg(temp.path())
        .args(["--server", "default", "send-keys", "old", "Enter"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    launch(&service, "current");
    assert!(new.session_dir("current").exists());
    assert!(!old.session_dir("current").exists());
    assert_eq!(cli(temp.path(), &["version", "--daemon", "--json"])["generation"], 2);
    let daemons = cli(temp.path(), &["daemons", "--json"]);
    let ours: Vec<_> = daemons.as_array().unwrap().iter().filter(|d| d["runtime_root"] == temp.path().to_str().unwrap()).collect();
    assert_eq!(ours.len(), 2);
    for daemon in ours {
        assert_eq!(daemon["alive"], true);
        assert_eq!(daemon["drain_state"], "serving");
        assert!(daemon["build"].is_object());
    }
    launch(&old, "duplicate");
    launch(&new, "duplicate");
    assert!(service.for_session("duplicate").unwrap_err().contains("exists in multiple daemons"));
    old.kill("duplicate").unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while old.inspect("duplicate").is_ok() {
        assert!(std::time::Instant::now() < deadline, "session did not exit");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(service.for_session("duplicate").unwrap().session_dir("duplicate"), new.session_dir("duplicate"));
    let (_, attachment) =
        service.for_session("old").unwrap().attach(Some("old".into()), None, None, None, true, Default::default()).unwrap();
    drop(attachment);
    for (daemon, id) in [(&old, "old"), (&new, "new"), (&new, "current"), (&new, "duplicate")] {
        daemon.kill(id).unwrap();
    }
}

#[test]
fn live_legacy_registration_keeps_its_directory_until_it_dies() {
    let temp = tempfile::tempdir().unwrap();
    let donor = SessionService::new(RuntimeLayout::new(temp.path().join("donor")));
    launch(&donor, "registration");
    // Borrow a known-live cleat registration to bootstrap the legacy path.
    // Thereafter the actual legacy daemon writes its own pid and serves it.
    let root = temp.path().join("legacy");
    let layout = RuntimeLayout::new(root.clone());
    layout.ensure_daemon_dirs().unwrap();
    let donor_layout = RuntimeLayout::new(temp.path().join("donor"));
    std::fs::copy(donor_layout.daemon_pid_path(), layout.daemon_pid_path()).unwrap();
    let legacy = SessionService::new(layout.clone());
    launch(&legacy, "legacy-session");
    assert_eq!(layout.generation(), None);
    assert!(!root.join("default").is_symlink());
    assert!(!root.join("default@1").exists());
    assert!(legacy.inspect("legacy-session").unwrap().generation.is_none());
    legacy.kill("legacy-session").unwrap();
    donor.kill("registration").unwrap();
}

#[test]
fn dead_current_advances_and_leaves_recordings_reachable() {
    let temp = tempfile::tempdir().unwrap();
    let layout = RuntimeLayout::new(temp.path().to_path_buf());
    let service = SessionService::new(layout.clone());
    launch(&service, "recorded");
    let first = layout.resolved().unwrap();
    service.kill("recorded").unwrap();
    terminate_daemon(&first);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while cleat::platform::daemon::is_session_daemon_alive(temp.path(), first.daemon_name()) {
        assert!(std::time::Instant::now() < deadline, "daemon did not die");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    launch(&service, "fresh");
    assert_eq!(layout.generation(), Some(2));
    assert!(first.session_dir("recorded").join("session.cast").exists());
    assert_eq!(service.for_session("recorded").unwrap().session_dir("recorded"), first.session_dir("recorded"));
    let dead = service.with_daemon(first.daemon_name().into()).unwrap();
    assert!(dead.inspect("recorded").unwrap_err().contains("generation default@1 is dead"));
    let recreation = service.for_recreation("recorded").unwrap();
    assert_eq!(recreation.session_dir("recorded"), layout.session_dir("recorded"));
    launch(&recreation, "recorded");
    assert_eq!(recreation.inspect("recorded").unwrap().generation, Some(2));
    assert_eq!(recreation.inspect("recorded").unwrap().hosting_epoch, 1);
    assert!(!first.session_dir("recorded").exists());
    recreation.kill("recorded").unwrap();
    service.kill("fresh").unwrap();
}

#[cfg(not(windows))]
fn terminate_daemon(layout: &RuntimeLayout) {
    cleat::platform::daemon::terminate_session_daemon_if_expected(layout.root(), layout.daemon_name());
}

#[cfg(windows)]
fn terminate_daemon(layout: &RuntimeLayout) {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE},
    };
    let pid = std::fs::read_to_string(layout.daemon_pid_path()).unwrap().trim().parse::<u32>().unwrap();
    // SAFETY: the registration belongs to this test's private runtime root;
    // the checked handle is closed exactly once.
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        assert!(!handle.is_null());
        let terminated = TerminateProcess(handle, 0);
        CloseHandle(handle);
        assert_ne!(terminated, 0);
    }
}

#[test]
fn recreation_rejects_path_components_before_mutating_the_layout() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("unused");
    let service = SessionService::new(RuntimeLayout::new(root.clone()));
    for id in ["..", "../outside", "a/b"] {
        assert!(service.for_recreation(id).is_err());
    }
    assert!(!root.exists());
}
