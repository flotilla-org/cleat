#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use cleat::{
    provider_ffi::{
        cleat_provider_close, cleat_provider_open, cleat_session_create, cleat_session_destroy, CleatProviderDesc, CleatSessionDesc,
        CleatStr, CLEAT_PROVIDER_ABI_VERSION, CLEAT_PROVIDER_BACKEND_DAEMON, CLEAT_PROVIDER_BACKEND_IN_PROCESS,
        CLEAT_PROVIDER_VT_PASSTHROUGH,
    },
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

fn read_when_done(path: &Path) -> String {
    wait_for(|| fs::read_to_string(path).is_ok_and(|s| s.ends_with("<end>\n")));
    fs::read_to_string(path).unwrap()
}

fn executable(path: &Path, script: &str) {
    fs::write(path, script).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

// Arguments a shell would reinterpret: spaces, both quote kinds, expansions,
// globs, separators, backslashes, a leading dash and the empty string.
const AWKWARD_ARGS: [&str; 9] = ["two words", "\"double\"", "'single'", "$HOME", "*", "a;b|c&&d", "back\\slash", "-lc", ""];

// Scenario (flotilla#4173): a login profile resets PATH, like Debian's
// /etc/profile under `/bin/sh -l`. A shell-command session loses the declared
// PATH to that profile; an argv session runs its program directly with exactly
// the declared environment, found on the declared PATH, with every argument
// delivered unchanged.
#[test]
fn argv_session_runs_without_shell_or_profile() {
    let temp = tempfile::tempdir().unwrap();
    let layout = RuntimeLayout::new(temp.path().join("runtime"));
    let home = temp.path().join("home");
    let tools = temp.path().join("opt-bin");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tools).unwrap();
    // The login shell reads $HOME/.profile, which clobbers PATH.
    fs::write(home.join(".profile"), "PATH=/usr/bin:/bin\nexport PATH\n").unwrap();
    // Only on the declared PATH, never on the daemon's.
    executable(
        &tools.join("probe"),
        "#!/bin/sh\n{ printf 'PATH=%s\\n' \"$PATH\"; printf '[%s]\\n' \"$@\"; echo '<end>'; } > \"$CAPTURE\"\nsleep 30\n",
    );
    let declared_path = format!("{}:/usr/bin:/bin", tools.display());

    let _daemon = Daemon(
        Command::new(env!("CARGO_BIN_EXE_cleat"))
            .args(["--runtime-root", layout.root().to_str().unwrap(), "--server", "default", "serve"])
            .env("SHELL", "/bin/sh")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    wait_for(|| layout.socket_path().exists());
    let service = SessionService::new(layout.clone());
    let start = |id: &str, cmd: Option<String>, argv: Option<Vec<String>>| {
        let capture = temp.path().join(id);
        let options = SessionStartOptions {
            environment_policy: ChildEnvironmentPolicy::Declared,
            environment: vec![
                ("PATH".into(), declared_path.clone()),
                ("HOME".into(), home.to_str().unwrap().into()),
                ("CAPTURE".into(), capture.to_str().unwrap().into()),
            ],
            argv,
            ..Default::default()
        };
        let started = cleat::session::ensure_session_started(&layout, Some(id.into()), Some(VtEngineKind::Passthrough), None, cmd, options);
        (started, capture)
    };

    // Control: the profile really does reset PATH for a shell-command session.
    let (started, capture) = start("shell", Some("{ echo \"PATH=$PATH\"; echo '<end>'; } > \"$CAPTURE\"; sleep 30".into()), None);
    started.unwrap();
    assert_eq!(read_when_done(&capture), "PATH=/usr/bin:/bin\n<end>\n");

    let mut argv = vec!["probe".to_string()];
    argv.extend(AWKWARD_ARGS.map(String::from));
    let (started, capture) = start("direct", None, Some(argv.clone()));
    assert_eq!(started.unwrap().argv.as_ref(), Some(&argv));
    let expected: String = std::iter::once(format!("PATH={declared_path}\n"))
        .chain(AWKWARD_ARGS.map(|arg| format!("[{arg}]\n")))
        .chain(["<end>\n".into()])
        .collect();
    assert_eq!(read_when_done(&capture), expected);
    let listed = service.list().unwrap().into_iter().find(|session| session.id == "direct").unwrap();
    assert_eq!((listed.cmd, listed.argv), (None, Some(argv)));

    // A program missing from the child's PATH, an empty argv or program name,
    // a NUL in an argument and an argv combined with a shell command are refused instead of falling back to a shell.
    let (missing, _) = start("missing", None, Some(vec!["cleat-no-such-program".into()]));
    let (empty, _) = start("empty", None, Some(Vec::new()));
    let (unnamed, _) = start("unnamed", None, Some(vec!["".into(), "arg".into()]));
    let (nul, _) = start("nul", None, Some(vec!["true".into(), "a\0b".into()]));
    let (both, _) = start("both", Some("true".into()), Some(vec!["true".into()]));
    for refused in [missing, empty, unnamed, nul, both] {
        assert!(refused.is_err(), "{refused:?}");
    }
    assert_eq!(service.list().unwrap().len(), 2);

    if cfg!(feature = "ghostty-vt") {
        // Glue: `cleat launch ... -- PROGRAM ARGS` reaches the same spawn path.
        let capture = temp.path().join("cli");
        let output = Command::new(env!("CARGO_BIN_EXE_cleat"))
            .args(["--runtime-root", layout.root().to_str().unwrap(), "--server", "default", "launch", "cli", "--env-clear", "--json"])
            .args(["--env", &format!("PATH={declared_path}"), "--env", &format!("HOME={}", home.display())])
            .args(["--env", &format!("CAPTURE={}", capture.display()), "--", "probe"])
            .args(AWKWARD_ARGS)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(read_when_done(&capture), expected);
    }
    for id in ["shell", "direct", "cli"] {
        let _ = service.kill(id);
    }
}

// The provider descriptor's argv reaches both the in-process and the daemon
// spawn, and a descriptor naming both a command and an argv creates nothing.
#[test]
fn provider_descriptor_argv_runs_directly_in_both_backends() {
    let temp = tempfile::Builder::new().prefix("cleat-argv-").tempdir_in("/tmp").unwrap();
    let root = temp.path().to_string_lossy().into_owned();
    for (backend, name) in [(CLEAT_PROVIDER_BACKEND_IN_PROCESS, "in-process"), (CLEAT_PROVIDER_BACKEND_DAEMON, "daemon")] {
        let capture = temp.path().join(name);
        let script = format!("printf '[%s]\\n' \"$@\" > '{}'; echo '<end>' >> '{}'; sleep 30", capture.display(), capture.display());
        let argv = ["/bin/sh", "-c", script.as_str(), "sh", "two words", "\"q\""];
        let argv_strs: Vec<CleatStr> = argv.iter().map(|arg| CleatStr { ptr: arg.as_ptr(), len: arg.len() }).collect();
        let command = b"true";
        unsafe {
            let provider = cleat_provider_open(&CleatProviderDesc {
                abi_version: CLEAT_PROVIDER_ABI_VERSION,
                backend,
                runtime_root: root.as_ptr(),
                runtime_root_len: root.len(),
                ..CleatProviderDesc::default()
            });
            assert!(!provider.is_null());
            let desc = CleatSessionDesc {
                cols: 80,
                rows: 24,
                vt_engine: CLEAT_PROVIDER_VT_PASSTHROUGH,
                argv: argv_strs.as_ptr(),
                argc: argv_strs.len(),
                ..CleatSessionDesc::default()
            };
            let both = cleat_session_create(provider, &CleatSessionDesc { command: command.as_ptr(), command_len: command.len(), ..desc });
            assert!(both.is_null(), "{name}: command and argv together must be refused");
            let session = cleat_session_create(provider, &desc);
            assert!(!session.is_null(), "{name}");
            assert_eq!(read_when_done(&capture), "[two words]\n[\"q\"]\n<end>\n", "{name}");
            cleat_session_destroy(session);
            cleat_provider_close(provider);
        }
    }
    let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));
    for session in service.list().unwrap_or_default() {
        let _ = service.kill(&session.id);
    }
}
