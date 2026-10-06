#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use cleat::{
    runtime::{ChildEnvironmentPolicy, RuntimeLayout},
    server::SessionService,
    session::SessionStartOptions,
    vt::VtEngineKind,
};

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for daemon/child");
        std::thread::sleep(Duration::from_millis(10));
    }
}

// Scenario: a contaminated daemon launches both policies through HTTP and the real
// PTY spawn. The shell entrypoint records its environment before any login startup.
#[test]
fn declared_environment_is_applied_before_shell_startup_and_inheritance_remains_default() {
    let temp = tempfile::tempdir().unwrap();
    let layout = RuntimeLayout::new(temp.path().join("runtime"));
    let shell = temp.path().join("shell");
    fs::write(&shell, "#!/bin/sh\n/usr/bin/env > \"$CAPTURE\"\nexec /bin/sh \"$@\"\n").unwrap();
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o755)).unwrap();
    let contaminated = ["NO_COLOR", "CLAUDECODE", "CLAUDE_CODE_MESSAGING_SOCKET", "CLAUDE_CODE_MESSAGING_TOKEN", "CLEAT_ENV_SENTINEL"];
    let mut command = Command::new(env!("CARGO_BIN_EXE_cleat"));
    command
        .args(["--runtime-root", layout.root().to_str().unwrap(), "--server", "default", "serve"])
        .env("SHELL", &shell)
        .env("CLEAT_SESSION", "stale-session")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    for name in contaminated {
        command.env(name, "daemon-only");
    }
    let _daemon = Daemon(command.spawn().unwrap());
    wait_for(|| layout.socket_path().exists());
    let service = SessionService::new(layout.clone());
    // Coverage: both enum variants, duplicate entries, empty values, intentional
    // agent variables, terminal overrides and fresh reserved coordinates.
    for policy in [ChildEnvironmentPolicy::Declared, ChildEnvironmentPolicy::Inherit] {
        let id = if policy == ChildEnvironmentPolicy::Declared { "declared" } else { "inherit" };
        let capture = temp.path().join(id);
        let options = SessionStartOptions {
            environment_policy: policy,
            environment: vec![
                ("CAPTURE".into(), capture.to_str().unwrap().into()),
                ("CLAUDECODE".into(), "adapter-owned".into()),
                ("EMPTY".into(), "".into()),
                ("DUPLICATE".into(), "first".into()),
                ("DUPLICATE".into(), "last".into()),
                ("TERM".into(), "custom".into()),
                ("TERM_PROGRAM".into(), "".into()),
                ("COLORTERM".into(), "explicit".into()),
            ],
            ..Default::default()
        };
        let session = cleat::session::ensure_session_started(
            &layout,
            Some(id.into()),
            Some(VtEngineKind::Passthrough),
            None,
            Some("sleep 30".into()),
            options,
        )
        .unwrap();
        assert_eq!(session.environment_policy, policy);
        wait_for(|| fs::read_to_string(&capture).is_ok_and(|s| s.contains("CLEAT_SESSION=")));
        let contents = fs::read_to_string(&capture).unwrap();
        let entries: Vec<_> = contents.lines().collect();
        for name in contaminated {
            if name != "CLAUDECODE" {
                assert_eq!(
                    entries.contains(&format!("{name}=daemon-only").as_str()),
                    policy == ChildEnvironmentPolicy::Inherit,
                    "{contents}"
                );
            }
        }
        for entry in ["CLAUDECODE=adapter-owned", "EMPTY=", "DUPLICATE=last", "TERM=custom", "TERM_PROGRAM=", "COLORTERM=explicit"] {
            assert!(entries.contains(&entry), "missing {entry}: {contents}");
        }
        assert!(!entries.contains(&"DUPLICATE=first"));
        assert!(entries.contains(&format!("CLEAT_SESSION={id}").as_str()));
        assert!(!entries.contains(&"CLEAT_SESSION=stale-session"));
        service.kill(id).unwrap();
    }
    if cfg!(feature = "ghostty-vt") {
        // Glue: the CLI's flag reaches the same HTTP/native spawn path.
        let capture = temp.path().join("cli-capture");
        let output = Command::new(env!("CARGO_BIN_EXE_cleat"))
            .args([
                "--runtime-root",
                layout.root().to_str().unwrap(),
                "--server",
                "default",
                "launch",
                "cli-declared",
                "--env-clear",
                "--env",
                &format!("CAPTURE={}", capture.display()),
                "--cmd",
                "sleep 30",
                "--json",
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        wait_for(|| fs::read_to_string(&capture).is_ok_and(|s| s.contains("CLEAT_SESSION=")));
        let contents = fs::read_to_string(&capture).unwrap();
        for name in contaminated {
            assert!(!contents.lines().any(|line| line.starts_with(&format!("{name}="))), "{contents}");
        }
        assert!(contents.lines().any(|line| line == "TERM_PROGRAM=ghostty"));
        assert!(contents.lines().any(|line| line == "COLORTERM=truecolor"));
        service.kill("cli-declared").unwrap();
    }
}

// Stored JSON is the contract: omit newly added fields exactly as older records did.
#[test]
fn older_records_default_to_inheritance_and_declared_records_round_trip() {
    let layout = RuntimeLayout::new("/tmp/unused".into());
    for policy in [ChildEnvironmentPolicy::Inherit, ChildEnvironmentPolicy::Declared] {
        let mut metadata = layout.session_metadata("example".into(), VtEngineKind::Passthrough, None, None);
        metadata.environment_policy = policy;
        let mut value = serde_json::to_value(&metadata).unwrap();
        assert_eq!(serde_json::from_value::<cleat::runtime::SessionMetadata>(value.clone()).unwrap(), metadata);
        value.as_object_mut().unwrap().remove("environment_policy");
        value.as_object_mut().unwrap().remove("environment");
        let old: cleat::runtime::SessionMetadata = serde_json::from_value(value).unwrap();
        assert_eq!(old.environment_policy, ChildEnvironmentPolicy::Inherit);
        assert!(old.environment.is_empty());
    }
}
