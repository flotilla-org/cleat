#[cfg(feature = "ghostty-vt")]
use std::process::{Command, Stdio};
use std::{
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use clap::Parser;
#[cfg(feature = "ghostty-vt")]
use cleat::session::{daemon_pid_path, foreground_path};
use cleat::{
    cli::{self, Cli, ExecResult},
    protocol::{Frame, SessionInfo},
    runtime::RuntimeLayout,
    server::{EndBound, SessionService, StartBound},
    session::session_socket_path,
    vt::{self, ClientCapabilities, ColorLevel, VtEngineKind},
};

fn service_for(path: &std::path::Path) -> SessionService {
    SessionService::new(RuntimeLayout::new(path.to_path_buf()))
}

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn wait_for_socket(path: &std::path::Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for socket {}", path.display());
}

struct EnvVarGuard {
    key: &'static str,
    original: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let original = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, original }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match self.original.take() {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn create_makes_session_directory_and_returns_metadata() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let cli = Cli::try_parse_from(["cleat", "create", "alpha", "--cmd", "bash"]).expect("parse create");

    let output = cli::execute(cli, &service).expect("execute create").expect("create output");
    assert_eq!(output, "alpha");
    assert!(temp.path().join("alpha").exists());
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn create_json_returns_structured_metadata() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let cli = Cli::try_parse_from(["cleat", "create", "--json", "alpha", "--cmd", "bash"]).expect("parse create");

    let output = cli::execute(cli, &service).expect("execute create").expect("create output");
    let created: SessionInfo = serde_json::from_str(&output).expect("parse create output");

    assert_eq!(created.id, "alpha");
    assert_eq!(created.vt_engine, vt::default_vt_engine_kind());
    assert_eq!(created.vt_engine_status, vt::vt_engine_status(vt::default_vt_engine_kind()));
    assert_eq!(created.functional_vt_available, vt::functional_vt_available());
    assert_eq!(created.cmd.as_deref(), Some("bash"));
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn create_uses_requested_vt_engine() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let cli = Cli::try_parse_from(["cleat", "create", "--json", "--vt", "passthrough", "alpha"]).expect("parse create");

    let output = cli::execute(cli, &service).expect("execute create").expect("create output");
    let created: SessionInfo = serde_json::from_str(&output).expect("parse create output");

    assert_eq!(created.vt_engine, VtEngineKind::Passthrough);
}

#[cfg(not(feature = "ghostty-vt"))]
#[test]
fn create_rejects_unavailable_vt_engine() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let cli = Cli::try_parse_from(["cleat", "create", "--vt", "ghostty", "alpha"]).expect("parse create");

    let err = cli::execute(cli, &service).expect_err("ghostty should be unavailable");

    assert!(err.contains("non-functional for real terminal usage"));
    assert!(err.contains("ghostty-vt"));
}

#[cfg(not(feature = "ghostty-vt"))]
#[test]
fn create_rejects_default_nonfunctional_build() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let cli = Cli::try_parse_from(["cleat", "create", "alpha"]).expect("parse create");

    let err = cli::execute(cli, &service).expect_err("default create should be rejected");

    assert!(err.contains("non-functional for real terminal usage"));
    assert!(err.contains("ghostty-vt"));
}

#[test]
fn list_reports_existing_sessions() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, Some(PathBuf::from("/repo")), None, false).expect("create alpha");
    service.create(Some("beta".into()), Some(VtEngineKind::Passthrough), None, Some("zsh".into()), false).expect("create beta");
    let cli = Cli::try_parse_from(["cleat", "list"]).expect("parse list");

    let output = cli::execute(cli, &service).expect("execute list").expect("list output");
    let lines: Vec<_> = output.lines().collect();

    assert_eq!(lines, vec![
        format!(
            "alpha\tdetached\t{} ({})\t/repo",
            vt::default_vt_engine_kind().as_str(),
            vt::vt_engine_status(vt::default_vt_engine_kind())
        ),
        format!("beta\tdetached\tpassthrough ({})\tzsh", vt::vt_engine_status(VtEngineKind::Passthrough)),
    ]);
}

#[test]
fn list_json_reports_existing_sessions() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, Some(PathBuf::from("/repo")), None, false).expect("create alpha");
    service.create(Some("beta".into()), Some(VtEngineKind::Passthrough), None, Some("zsh".into()), false).expect("create beta");
    let cli = Cli::try_parse_from(["cleat", "list", "--json"]).expect("parse list");

    let output = cli::execute(cli, &service).expect("execute list").expect("list output");
    let listed: Vec<SessionInfo> = serde_json::from_str(&output).expect("parse list output");

    assert_eq!(listed.iter().map(|item| item.id.as_str()).collect::<Vec<_>>(), vec!["alpha", "beta"]);
    assert_eq!(listed[0].vt_engine, vt::default_vt_engine_kind());
    assert_eq!(listed[0].vt_engine_status, vt::vt_engine_status(vt::default_vt_engine_kind()));
    assert_eq!(listed[0].functional_vt_available, vt::functional_vt_available());
    assert_eq!(listed[1].vt_engine, VtEngineKind::Passthrough);
    assert_eq!(listed[1].vt_engine_status, vt::vt_engine_status(VtEngineKind::Passthrough));
    assert_eq!(listed[1].functional_vt_available, vt::functional_vt_available());
}

#[test]
fn capture_rejects_passthrough_sessions() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), Some(VtEngineKind::Passthrough), None, Some("sleep 5".into()), false).expect("create alpha");
    let cli = Cli::try_parse_from(["cleat", "capture", "alpha"]).expect("parse capture");

    let err = cli::execute(cli, &service).expect_err("passthrough capture should fail");

    assert!(err.contains("placeholder"));
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn capture_returns_text_for_ghostty_sessions() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service
        .create(Some("alpha".into()), Some(VtEngineKind::Ghostty), None, Some("sh -c 'stty raw; exec cat'".into()), false)
        .expect("create alpha");

    // Wait for sh + stty + exec cat to start
    std::thread::sleep(Duration::from_millis(500));

    // Send text via send-keys — cat echoes it back in raw mode
    service.send_keys("alpha", b"hello capture").expect("send keys");

    let deadline = Instant::now() + Duration::from_secs(2);
    let output = loop {
        let cli = Cli::try_parse_from(["cleat", "capture", "alpha"]).expect("parse capture");
        match cli::execute(cli, &service) {
            ExecResult::Ok(Some(text)) if text.contains("hello capture") => break text,
            ExecResult::Ok(Some(_)) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            ExecResult::Ok(Some(text)) => panic!("capture did not include expected text: {text}"),
            ExecResult::Ok(None) => panic!("capture returned no output"),
            ExecResult::Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            ExecResult::Err(err) => panic!("capture failed: {err}"),
            other => panic!("unexpected result: {other:?}"),
        }
    };

    assert!(output.contains("hello capture"));
}

#[test]
fn kill_removes_session_directory() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, None, None, false).expect("create alpha");
    let cli = Cli::try_parse_from(["cleat", "kill", "alpha"]).expect("parse kill");

    let output = cli::execute(cli, &service).expect("execute kill");

    assert_eq!(output, None);
    assert!(!temp.path().join("alpha").exists());
}

#[test]
fn kill_missing_session_is_an_error() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let cli = Cli::try_parse_from(["cleat", "kill", "missing"]).expect("parse kill");

    let err = cli::execute(cli, &service).expect_err("missing kill should fail");

    assert!(err.contains("missing"));
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn attach_creates_session_lazily_and_reuses_it_on_later_attach() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());

    let (first, attach) = service.attach(Some("alpha".into()), None, None, Some("sleep 5".into()), false).expect("first attach");
    assert_eq!(first.id, "alpha");
    assert_eq!(first.vt_engine, vt::default_vt_engine_kind());
    assert!(daemon_pid_path(temp.path(), "alpha").exists());

    drop(attach);

    let (second, _attach2) = service.attach(Some("alpha".into()), Some(VtEngineKind::Passthrough), None, None, false).expect("reattach");
    assert_eq!(second.id, "alpha");
    assert_eq!(second.vt_engine, vt::default_vt_engine_kind());
}

#[cfg(not(feature = "ghostty-vt"))]
#[test]
fn attach_rejects_lazy_create_in_nonfunctional_build() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let cli = Cli::try_parse_from(["cleat", "attach", "alpha", "--cmd", "sleep 5"]).expect("parse attach");

    let err = cli::execute(cli, &service).expect_err("lazy attach should be rejected without ghostty");

    assert!(err.contains("non-functional for real terminal usage"));
    assert!(err.contains("ghostty-vt"));
}

#[test]
fn attach_vt_only_applies_when_creating_new_session() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());

    let (created, attach) =
        service.attach(Some("alpha".into()), Some(VtEngineKind::Passthrough), None, Some("sleep 5".into()), false).expect("first attach");
    assert_eq!(created.vt_engine, VtEngineKind::Passthrough);
    drop(attach);

    let (reattached, _attach2) =
        service.attach(Some("alpha".into()), Some(vt::default_vt_engine_kind()), None, None, false).expect("reattach");
    assert_eq!(reattached.vt_engine, VtEngineKind::Passthrough);
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn attach_rejects_second_foreground_client_while_one_is_active() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());

    let (_session, _attach) = service.attach(Some("alpha".into()), None, None, Some("sleep 5".into()), false).expect("first attach");
    let err = service.attach(Some("alpha".into()), None, None, None, false).expect_err("second attach should fail");

    assert!(err.contains("foreground client"));
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn lifecycle_attach_init_with_capabilities_is_accepted_without_changing_single_client_policy() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());

    service.create(Some("alpha".into()), None, None, Some("sleep 5".into()), false).expect("create alpha");

    let mut stream = UnixStream::connect(session_socket_path(temp.path(), "alpha")).expect("connect socket");
    Frame::AttachInit { cols: 100, rows: 30, capabilities: ClientCapabilities::new(ColorLevel::Ansi256, true) }
        .write(&mut stream)
        .expect("write attach init");

    let response = Frame::read(&mut stream).expect("read attach response");
    assert_eq!(response, Frame::Ack);

    let err = service.attach(Some("alpha".into()), None, None, None, false).expect_err("second attach should fail");
    assert!(err.contains("foreground client"));
}

#[test]
fn lifecycle_attach_init_capabilities_drive_replay_output_on_daemon_path() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvVarGuard::set("CLEAT_TEST_VT_ENGINE", "replay-probe");

    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, None, Some("sleep 5".into()), false).expect("create alpha");

    let mut stream = UnixStream::connect(session_socket_path(temp.path(), "alpha")).expect("connect socket");
    Frame::AttachInit { cols: 100, rows: 30, capabilities: ClientCapabilities::new(ColorLevel::Ansi256, true) }
        .write(&mut stream)
        .expect("write attach init");

    let response = Frame::read(&mut stream).expect("read attach response");
    assert_eq!(response, Frame::Ack);

    let replay = Frame::read(&mut stream).expect("read replay output");
    assert_eq!(replay, Frame::Output(b"Ansi256:true".to_vec()));
}

#[test]
fn send_keys_injects_input_into_running_session_pty() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, None, Some("cat".into()), false).expect("create alpha");

    let mut stream = UnixStream::connect(session_socket_path(temp.path(), "alpha")).expect("connect socket");
    Frame::AttachInit { cols: 100, rows: 30, capabilities: ClientCapabilities::conservative_fallback() }
        .write(&mut stream)
        .expect("write attach init");
    assert_eq!(Frame::read(&mut stream).expect("read attach response"), Frame::Ack);

    service.send_keys("alpha", b"hello\n").expect("send keys");

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut output = Vec::new();
    while Instant::now() < deadline {
        match Frame::read(&mut stream).expect("read output") {
            Frame::Output(bytes) => {
                output.extend_from_slice(&bytes);
                if String::from_utf8_lossy(&output).contains("hello") {
                    break;
                }
            }
            other => panic!("expected output frame, got {other:?}"),
        }
    }

    assert!(
        String::from_utf8_lossy(&output).contains("hello"),
        "send-keys output should reach the attached session, got {:?}",
        String::from_utf8_lossy(&output)
    );
}

#[test]
fn send_keys_cli_executes_end_to_end() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, None, Some("cat".into()), false).expect("create alpha");

    let mut stream = UnixStream::connect(session_socket_path(temp.path(), "alpha")).expect("connect socket");
    Frame::AttachInit { cols: 100, rows: 30, capabilities: ClientCapabilities::conservative_fallback() }
        .write(&mut stream)
        .expect("write attach init");
    assert_eq!(Frame::read(&mut stream).expect("read attach response"), Frame::Ack);

    let cli = Cli::try_parse_from(["cleat", "send-keys", "alpha", "h", "i", "Enter"]).expect("parse send-keys");
    assert_eq!(cli::execute(cli, &service).expect("execute send-keys"), None);

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut output = Vec::new();
    while Instant::now() < deadline {
        match Frame::read(&mut stream).expect("read output") {
            Frame::Output(bytes) => {
                output.extend_from_slice(&bytes);
                if String::from_utf8_lossy(&output).contains("hi") {
                    break;
                }
            }
            other => panic!("expected output frame, got {other:?}"),
        }
    }

    assert!(
        String::from_utf8_lossy(&output).contains("hi"),
        "cli send-keys output should reach the attached session, got {:?}",
        String::from_utf8_lossy(&output)
    );
}

/// When no client is attached, the daemon's DA tracker should inject a synthetic
/// DA1 response into the PTY when it sees a DA query in the output stream.
///
/// Strategy: launch `sh -c 'stty raw; exec cat'` with recording. Raw mode
/// disables line buffering so the DA response passes through immediately.
/// send-keys injects the DA query → cat echoes it → PTY output → daemon sees
/// it and (detached) injects the response → PTY input → cat echoes the
/// response → PTY output (recorded).
#[test]
fn detached_session_answers_da_queries() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, None, Some("sh -c 'stty raw; exec cat'".into()), true).expect("create alpha");

    // Wait for sh + stty + exec cat to complete
    std::thread::sleep(Duration::from_secs(1));

    // Mark, then send DA1 query while detached
    let offset = service.mark("alpha").expect("mark");
    service.send_keys("alpha", b"\x1b[c").expect("send DA query");
    std::thread::sleep(Duration::from_secs(1));

    // Read recorded output since the mark
    let (output, _outcome) =
        service.capture_slice_raw("alpha", StartBound::Offset(offset), EndBound::EndOfRecording).expect("capture slice");

    assert!(output.contains("\x1b[?62;22c"), "detached session should inject DA1 response in recorded output, got: {output:?}");
}

/// When a client IS attached, the daemon should NOT inject synthetic DA responses —
/// the real terminal handles them.
///
/// Strategy: launch `sh -c 'stty raw; exec cat'`, attach first, THEN send DA
/// query via send-keys. cat echoes it → PTY output → daemon forwards to attached
/// client but does NOT inject a response. We read frames from the client stream
/// and verify the DA response is absent.
#[test]
fn attached_session_does_not_get_synthetic_da_reply() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, None, Some("sh -c 'stty raw; exec cat'".into()), false).expect("create alpha");

    // Wait for sh + stty + exec cat to complete
    std::thread::sleep(Duration::from_secs(1));

    // Attach BEFORE sending the DA query
    let mut stream = UnixStream::connect(session_socket_path(temp.path(), "alpha")).expect("connect");
    stream.set_read_timeout(Some(Duration::from_millis(100))).ok();
    Frame::AttachInit { cols: 80, rows: 24, capabilities: ClientCapabilities::conservative_fallback() }
        .write(&mut stream)
        .expect("write attach init");
    assert_eq!(Frame::read(&mut stream).expect("read ack"), Frame::Ack);

    // Send DA1 query while attached — cat echoes it, daemon forwards but should NOT inject response
    service.send_keys("alpha", b"\x1b[c").expect("send DA query");

    // Read output frames for a short window — we should see the echoed query but NOT a DA response
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut output = Vec::new();
    while Instant::now() < deadline {
        match Frame::read(&mut stream) {
            Ok(Frame::Output(bytes)) => output.extend_from_slice(&bytes),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
                continue;
            }
            Err(e) => panic!("read frame: {e}"),
        }
    }

    assert!(
        !output.windows(b"\x1b[?62;22c".len()).any(|w| w == b"\x1b[?62;22c"),
        "attached session should NOT inject DA1 response, but got one in output"
    );
}

#[test]
fn resolve_next_marker_returns_minimum_offset_above() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, None, Some("sh -c 'stty raw; exec cat'".into()), true).expect("create");

    std::thread::sleep(Duration::from_millis(500));

    // Named marks register offsets in the daemon's marker map; unnamed `mark`
    // does not. `resolve_next_marker_after` searches that map, so we need
    // named marks here.
    let off_a = service.named_mark("alpha", "a").expect("mark a");
    service.send_keys("alpha", b"x").expect("send x");
    std::thread::sleep(Duration::from_millis(300));
    let off_b = service.named_mark("alpha", "b").expect("mark b");
    service.send_keys("alpha", b"y").expect("send y");
    std::thread::sleep(Duration::from_millis(300));
    let off_c = service.named_mark("alpha", "c").expect("mark c");

    assert_eq!(service.resolve_next_marker_after("alpha", off_a).expect("resolve"), Some(off_b), "next after A should be B");
    assert_eq!(service.resolve_next_marker_after("alpha", off_b).expect("resolve"), Some(off_c), "next after B should be C");
    assert_eq!(service.resolve_next_marker_after("alpha", off_c).expect("resolve"), None, "no marker after C should return None");
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn replay_reattach_delivers_restore_before_new_live_output() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service
        .create(Some("alpha".into()), None, None, Some("printf 'before'; sleep 1; printf 'after'; sleep 5".into()), false)
        .expect("create alpha");

    let mut first = UnixStream::connect(session_socket_path(temp.path(), "alpha")).expect("connect first socket");
    Frame::AttachInit { cols: 100, rows: 30, capabilities: ClientCapabilities::new(ColorLevel::Ansi256, true) }
        .write(&mut first)
        .expect("write first attach init");
    assert_eq!(Frame::read(&mut first).expect("read first attach response"), Frame::Ack);

    let first_live = Frame::read(&mut first).expect("read first live output");
    let first_live_bytes = match first_live {
        Frame::Output(bytes) => bytes,
        other => panic!("expected first live output, got {other:?}"),
    };
    assert!(String::from_utf8_lossy(&first_live_bytes).contains("before"));
    drop(first);

    let detach_deadline = Instant::now() + Duration::from_secs(2);
    while foreground_path(temp.path(), "alpha").exists() && Instant::now() < detach_deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!foreground_path(temp.path(), "alpha").exists(), "foreground marker should clear before reattach");

    let mut second = UnixStream::connect(session_socket_path(temp.path(), "alpha")).expect("connect second socket");
    Frame::AttachInit { cols: 100, rows: 30, capabilities: ClientCapabilities::new(ColorLevel::Ansi256, true) }
        .write(&mut second)
        .expect("write second attach init");
    assert_eq!(Frame::read(&mut second).expect("read second attach response"), Frame::Ack);

    let clear = Frame::read(&mut second).expect("read clear output");
    assert_eq!(clear, Frame::Output(b"\x1b[2J\x1b[H".to_vec()));

    let replay = Frame::read(&mut second).expect("read replay output");
    let replay_bytes = match replay {
        Frame::Output(bytes) => bytes,
        other => panic!("expected replay output, got {other:?}"),
    };
    let replay_text = String::from_utf8_lossy(&replay_bytes);
    assert!(replay_text.contains("before"), "replay should include prior output: {replay_text:?}");
    assert!(!replay_text.contains("after"), "replay should arrive before later live output: {replay_text:?}");

    let live = loop {
        match Frame::read(&mut second).expect("read live output after replay") {
            Frame::Output(bytes) if String::from_utf8_lossy(&bytes).contains("after") => break bytes,
            Frame::Output(_) => continue,
            other => panic!("expected output frame after replay, got {other:?}"),
        }
    };
    assert!(String::from_utf8_lossy(&live).contains("after"));
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn first_attach_replay_does_not_clear_before_output() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, None, Some("printf 'before'; sleep 5".into()), false).expect("create alpha");

    let mut stream = UnixStream::connect(session_socket_path(temp.path(), "alpha")).expect("connect socket");
    Frame::AttachInit { cols: 100, rows: 30, capabilities: ClientCapabilities::new(ColorLevel::Ansi256, true) }
        .write(&mut stream)
        .expect("write attach init");
    assert_eq!(Frame::read(&mut stream).expect("read attach response"), Frame::Ack);

    let first = Frame::read(&mut stream).expect("read first output");
    let bytes = match first {
        Frame::Output(bytes) => bytes,
        other => panic!("expected output frame, got {other:?}"),
    };

    assert_ne!(bytes, b"\x1b[2J\x1b[H".to_vec(), "first attach should not clear before replay/output");
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn dropping_foreground_attach_keeps_session_alive_for_later_attach() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());

    let (_session, attach) = service.attach(Some("alpha".into()), None, None, Some("sleep 5".into()), false).expect("first attach");
    let pid_path = daemon_pid_path(temp.path(), "alpha");
    assert!(pid_path.exists());

    drop(attach);

    let (_session, _reattach) = service.attach(Some("alpha".into()), None, None, None, false).expect("reattach after disconnect");
    assert!(pid_path.exists());
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn stale_foreground_file_does_not_block_attach() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());

    service.create(Some("alpha".into()), None, None, Some("sleep 5".into()), false).expect("create alpha");
    std::fs::write(foreground_path(temp.path(), "alpha"), b"999999").expect("write stale foreground marker");

    let (_session, _attach) = service.attach(Some("alpha".into()), None, None, None, false).expect("attach with stale foreground marker");
}

#[test]
fn attach_no_create_rejects_missing_session() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let cli = Cli::try_parse_from(["cleat", "attach", "--no-create", "missing"]).expect("parse attach");

    let err = cli::execute(cli, &service).expect_err("missing attach should fail");

    assert!(err.contains("missing"));
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn cleat_attach_exits_when_session_is_killed() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, None, Some("sleep 30".into()), false).expect("create alpha");

    let cleat_bin = std::env::var("CARGO_BIN_EXE_cleat").expect("cleat bin");
    let mut child = Command::new(cleat_bin)
        .arg("--runtime-root")
        .arg(temp.path())
        .arg("attach")
        .arg("alpha")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn cleat attach");
    let _stdin = child.stdin.take().expect("attach stdin");

    let attach_deadline = Instant::now() + Duration::from_secs(2);
    while !foreground_path(temp.path(), "alpha").exists() && Instant::now() < attach_deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(foreground_path(temp.path(), "alpha").exists(), "attach should establish a foreground client before kill");

    service.kill("alpha").expect("kill session");

    let exit_deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(status) = child.try_wait().expect("try_wait attach child") {
            assert!(status.success(), "attach should exit cleanly after session kill: {status:?}");
            break;
        }
        if Instant::now() >= exit_deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("cleat attach did not exit after session kill");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn cleat_detach_exits_foreground_client_and_keeps_session_alive() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, None, Some("sleep 30".into()), false).expect("create alpha");

    let cleat_bin = std::env::var("CARGO_BIN_EXE_cleat").expect("cleat bin");
    let mut child = Command::new(cleat_bin)
        .arg("--runtime-root")
        .arg(temp.path())
        .arg("attach")
        .arg("alpha")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn cleat attach");
    let _stdin = child.stdin.take().expect("attach stdin");

    let attach_deadline = Instant::now() + Duration::from_secs(2);
    while !foreground_path(temp.path(), "alpha").exists() && Instant::now() < attach_deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(foreground_path(temp.path(), "alpha").exists(), "attach should establish a foreground client before detach");

    service.detach("alpha").expect("detach session");

    let exit_deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(status) = child.try_wait().expect("try_wait attach child") {
            assert!(status.success(), "attach should exit cleanly after detach: {status:?}");
            break;
        }
        if Instant::now() >= exit_deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("cleat attach did not exit after detach");
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    assert!(!foreground_path(temp.path(), "alpha").exists(), "detach should clear the foreground marker");
    assert!(temp.path().join("alpha").exists(), "detach should leave the session directory intact");
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn cleat_attach_exits_on_sigterm_and_keeps_session_alive() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, None, Some("sleep 30".into()), false).expect("create alpha");

    let cleat_bin = std::env::var("CARGO_BIN_EXE_cleat").expect("cleat bin");
    let mut child = Command::new(cleat_bin)
        .arg("--runtime-root")
        .arg(temp.path())
        .arg("attach")
        .arg("alpha")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn cleat attach");
    let _stdin = child.stdin.take().expect("attach stdin");

    let attach_deadline = Instant::now() + Duration::from_secs(2);
    while !foreground_path(temp.path(), "alpha").exists() && Instant::now() < attach_deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(foreground_path(temp.path(), "alpha").exists(), "attach should establish a foreground client before signal exit");

    let rc = unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    assert_eq!(rc, 0, "send SIGTERM to attach process");

    let exit_deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(status) = child.try_wait().expect("try_wait attach child") {
            assert!(status.success(), "attach should exit cleanly after SIGTERM: {status:?}");
            break;
        }
        if Instant::now() >= exit_deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("cleat attach did not exit after SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let cleared_deadline = Instant::now() + Duration::from_secs(2);
    while foreground_path(temp.path(), "alpha").exists() && Instant::now() < cleared_deadline {
        std::thread::sleep(Duration::from_millis(20));
    }

    assert!(!foreground_path(temp.path(), "alpha").exists(), "signal exit should clear the foreground marker");
    assert!(temp.path().join("alpha").exists(), "signal exit should leave the session directory intact");
}

#[test]
fn inspect_returns_structured_session_state() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let info = service.create(Some("alpha".into()), None, None, Some("bash".into()), false).expect("create session");

    let socket_path = session_socket_path(temp.path(), &info.id);
    wait_for_socket(&socket_path);

    let deadline = Instant::now() + Duration::from_secs(2);
    let result = loop {
        match service.inspect(&info.id) {
            Ok(result) => break result,
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) => panic!("inspect session: {err}"),
        }
    };

    assert_eq!(result.session.id, "alpha");
    assert_eq!(result.session.state, "running");
    assert_eq!(result.session.vt_engine, vt::default_vt_engine_kind().as_str());
    assert_eq!(result.session.vt_engine_status, vt::vt_engine_status(vt::default_vt_engine_kind()));
    assert_eq!(result.session.functional_vt_available, vt::functional_vt_available());
    assert!(result.process.leader_pid > 0);
    assert!(result.process.foreground_pgid.is_some());
    assert_eq!(result.terminal.cols, 80);
    assert_eq!(result.terminal.rows, 24);
    assert!(!result.recording.active);

    service.kill(&info.id).expect("kill session");
}

#[test]
fn signal_term_to_leader_terminates_session() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let info = service.create(Some("beta".into()), None, None, Some("sleep 60".into()), false).expect("create session");

    let socket_path = session_socket_path(temp.path(), &info.id);
    wait_for_socket(&socket_path);

    let inspect_deadline = Instant::now() + Duration::from_secs(2);
    let result = loop {
        match service.inspect(&info.id) {
            Ok(result) => break result,
            Err(_) if Instant::now() < inspect_deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) => panic!("inspect before signal: {err}"),
        }
    };
    assert!(result.process.leader_pid > 0);

    service.signal(&info.id, libc::SIGTERM, cleat::protocol::SignalTarget::Leader).expect("signal session");

    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if !socket_path.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!socket_path.exists(), "socket should be gone after SIGTERM to leader");
}

#[test]
fn short_lived_session_reaps_its_directory_after_child_exit() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, None, Some("printf done; sleep 0.1".into()), false).expect("create alpha");

    let session_dir = temp.path().join("alpha");
    let deadline = Instant::now() + Duration::from_secs(2);
    while session_dir.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }

    assert!(!session_dir.exists(), "session directory should be reaped after child exit");
}

#[test]
fn record_command_activates_recording_on_running_session() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let info = service.create(Some("delta".into()), None, None, Some("sleep 30".into()), false).expect("create session");

    let socket_path = session_socket_path(temp.path(), &info.id);
    wait_for_socket(&socket_path);

    // Wait for daemon to be ready for inspect
    let inspect_deadline = Instant::now() + Duration::from_secs(2);
    let result = loop {
        match service.inspect(&info.id) {
            Ok(result) => break result,
            Err(_) if Instant::now() < inspect_deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) => panic!("inspect before record: {err}"),
        }
    };
    assert!(!result.recording.active);

    // Activate recording
    service.record(&info.id, true).expect("activate recording");

    // Verify recording is now on
    let result = service.inspect(&info.id).expect("inspect after record");
    assert!(result.recording.active);

    service.kill(&info.id).expect("kill session");
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn create_with_record_flag_activates_recording() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());

    let cli = Cli::try_parse_from(["cleat", "create", "gamma", "--record"]).expect("parse create --record");
    cli::execute(cli, &service).expect("execute create --record");

    let socket_path = session_socket_path(temp.path(), "gamma");
    wait_for_socket(&socket_path);

    let inspect_deadline = Instant::now() + Duration::from_secs(2);
    let result = loop {
        match service.inspect("gamma") {
            Ok(result) => break result,
            Err(_) if Instant::now() < inspect_deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) => panic!("inspect after create --record: {err}"),
        }
    };
    assert!(result.recording.active, "recording should be active with --record flag");

    service.kill("gamma").expect("kill session");
}

#[test]
fn inspect_missing_session_is_an_error() {
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let err = service.inspect("missing").expect_err("missing session should error");
    assert!(err.contains("missing"));
}

#[test]
fn signal_missing_session_is_an_error() {
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let err = service.signal("missing", libc::SIGINT, cleat::protocol::SignalTarget::Foreground).expect_err("missing session should error");
    assert!(err.contains("missing"));
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn expect_finds_text_in_recorded_output_after_marker() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, None, Some("sh -c 'stty raw; exec cat'".into()), true).expect("create alpha");

    // Wait for sh + stty + exec cat to start
    std::thread::sleep(Duration::from_secs(1));

    // Set a marker, then send text that cat will echo
    let offset = service.named_mark("alpha", "m1").expect("mark");
    service.send_keys("alpha", b"HELLO_EXPECT\n").expect("send keys");

    // expect should find the text in recorded output
    let (status, _elapsed) = service.expect("alpha", "HELLO_EXPECT", offset, 5000).expect("expect call");
    assert_eq!(status, cleat::protocol::WaitStatus::Ready, "expect should find text in recording");
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn expect_times_out_when_text_not_in_recording() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, None, Some("sh -c 'stty raw; exec cat'".into()), true).expect("create alpha");

    // Wait for sh + stty + exec cat to start
    std::thread::sleep(Duration::from_secs(1));

    let offset = service.named_mark("alpha", "m1").expect("mark");

    // expect for text that will never appear — should timeout
    let (status, _elapsed) = service.expect("alpha", "NEVER_APPEARS", offset, 500).expect("expect call");
    assert_eq!(status, cleat::protocol::WaitStatus::Timeout, "expect should timeout when text absent");
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn inspect_reports_dynamic_leader_cwd() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("cwd-test".into()), None, None, Some("bash".into()), false).expect("create");

    // Wait for shell to start
    std::thread::sleep(Duration::from_secs(1));

    // Change directory
    service.send_keys("cwd-test", b"cd /tmp\n").expect("send cd");
    // Wait for command to complete
    let _ = service.wait("cwd-test", vec![cleat::protocol::WaitCondition::OutputIdle { quiet_ms: 500 }], 5000);

    let result = service.inspect("cwd-test").expect("inspect");
    let leader_cwd = result.process.leader_cwd.expect("leader_cwd should be Some");

    // On macOS /tmp is a symlink to /private/tmp
    let expected = std::fs::canonicalize("/tmp").expect("canonicalize /tmp");
    assert_eq!(std::fs::canonicalize(&leader_cwd).unwrap_or_else(|_| leader_cwd.clone()), expected, "leader_cwd should reflect cd /tmp");

    // When shell is in foreground, foreground_cwd should match leader_cwd
    let fg_cwd = result.process.foreground_cwd.expect("foreground_cwd should be Some");
    assert_eq!(
        std::fs::canonicalize(&fg_cwd).unwrap_or_else(|_| fg_cwd.clone()),
        expected,
        "foreground_cwd should match leader_cwd when shell is in foreground"
    );

    service.kill("cwd-test").expect("kill");
}

#[test]
fn transcript_between_two_named_markers_returns_exact_range() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, None, Some("sh -c 'stty raw; exec cat'".into()), true).expect("create");

    std::thread::sleep(Duration::from_millis(500));

    service.named_mark("alpha", "m1").expect("mark m1");
    service.send_keys("alpha", b"first").expect("send first");
    std::thread::sleep(Duration::from_millis(300));
    service.named_mark("alpha", "m2").expect("mark m2");
    service.send_keys("alpha", b"second").expect("send second");
    std::thread::sleep(Duration::from_millis(300));

    let cli = Cli::try_parse_from(["cleat", "transcript", "alpha", "--since-marker", "m1", "--until-marker", "m2"]).expect("parse");
    let result = cli::execute(cli, &service);
    let output = match result {
        ExecResult::Ok(Some(s)) => s,
        other => panic!("expected Ok(Some(...)), got {other:?}"),
    };
    assert!(output.contains("first"), "expected 'first' in output, got: {output:?}");
    assert!(!output.contains("second"), "did not expect 'second', got: {output:?}");
}

#[test]
fn transcript_until_idle_terminates_at_quiet_period() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, None, Some("sh -c 'stty raw; exec cat'".into()), true).expect("create");

    std::thread::sleep(Duration::from_millis(500));

    service.named_mark("alpha", "start").expect("mark start");
    service.send_keys("alpha", b"burst").expect("send burst");
    std::thread::sleep(Duration::from_millis(1500));
    service.send_keys("alpha", b"after").expect("send after");
    std::thread::sleep(Duration::from_millis(300));

    let cli = Cli::try_parse_from(["cleat", "transcript", "alpha", "--since-marker", "start", "--until-idle", "500ms"]).expect("parse");
    let result = cli::execute(cli, &service);
    let output = match result {
        ExecResult::Ok(Some(s)) => s,
        other => panic!("expected Ok(Some(...)), got {other:?}"),
    };
    assert!(output.contains("burst"), "expected 'burst' in output");
    assert!(!output.contains("after"), "idle gap should have terminated slice before 'after'");
}

#[test]
fn transcript_until_raw_offset_returns_exact_range() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, None, Some("sh -c 'stty raw; exec cat'".into()), true).expect("create");

    std::thread::sleep(Duration::from_millis(500));

    let off_a = service.named_mark("alpha", "a").expect("mark a");
    service.send_keys("alpha", b"middle").expect("send middle");
    std::thread::sleep(Duration::from_millis(300));
    let off_b = service.named_mark("alpha", "b").expect("mark b");
    service.send_keys("alpha", b"trailing").expect("send trailing");
    std::thread::sleep(Duration::from_millis(300));

    // Raw offsets via --since / --until should slice exactly the same as
    // --since-marker a / --until-marker b — proves the raw-offset code path.
    let cli =
        Cli::try_parse_from(["cleat", "transcript", "alpha", "--since", &off_a.to_string(), "--until", &off_b.to_string()]).expect("parse");
    let result = cli::execute(cli, &service);
    let output = match result {
        ExecResult::Ok(Some(s)) => s,
        other => panic!("expected Ok(Some(...)), got {other:?}"),
    };
    assert!(output.contains("middle"), "expected 'middle' in output, got: {output:?}");
    assert!(!output.contains("trailing"), "did not expect 'trailing', got: {output:?}");
}

#[test]
fn replay_with_session_and_markers_while_daemon_alive() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, None, Some("sh -c 'stty raw; exec cat'".into()), true).expect("create");

    std::thread::sleep(Duration::from_millis(500));

    service.named_mark("alpha", "m1").expect("mark m1");
    service.send_keys("alpha", b"middle").expect("send middle");
    std::thread::sleep(Duration::from_millis(300));
    service.named_mark("alpha", "m2").expect("mark m2");
    service.send_keys("alpha", b"trailing").expect("send trailing");
    std::thread::sleep(Duration::from_millis(300));

    // While daemon still alive, run replay through the CLI dispatch.
    // --speed 1000 keeps the test well under a second.
    // --max-idle 0ms removes any residual sleep.
    let cli = Cli::try_parse_from([
        "cleat",
        "replay",
        "--session",
        "alpha",
        "--since-marker",
        "m1",
        "--until-marker",
        "m2",
        "--speed",
        "1000",
        "--max-idle",
        "0ms",
    ])
    .expect("parse");

    let result = cli::execute(cli, &service);
    assert!(matches!(result, ExecResult::Ok(_)), "expected Ok, got {result:?}");

    // Cleanup.
    let _ = service.kill("alpha");
}
