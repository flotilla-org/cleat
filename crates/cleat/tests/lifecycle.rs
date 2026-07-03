#![cfg(unix)]

#[cfg(feature = "ghostty-vt")]
use std::process::{Command, Stdio};
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use clap::Parser;
#[cfg(feature = "ghostty-vt")]
use cleat::session::foreground_path;
use cleat::{
    cli::{self, Cli, ExecResult},
    protocol::{Frame, SessionInfo},
    provider::ProviderFeatures,
    provider_ffi::{
        cleat_provider_close, cleat_provider_open, cleat_session_create, cleat_session_destroy, cleat_session_write_bytes,
        CleatProviderDesc, CleatSessionDesc, CLEAT_PROVIDER_ABI_VERSION, CLEAT_PROVIDER_BACKEND_DAEMON, CLEAT_PROVIDER_VT_PASSTHROUGH,
    },
    recording::{SessionRecorder, CAST_FILE_NAME},
    runtime::{RuntimeLayout, TerminalSize},
    server::{EndBound, SessionService, StartBound},
    session::{daemon_pid_path, session_socket_path},
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

fn http_session_request(root: &std::path::Path, id: &str, request: &str) -> String {
    let socket_path = session_socket_path(root, id);
    let mut stream = UnixStream::connect(&socket_path).expect("connect session socket");
    stream.write_all(request.as_bytes()).expect("write request");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read response");
    response
}

fn http_body(response: &str) -> &str {
    response.split_once("\r\n\r\n").map(|(_, body)| body).expect("HTTP response body")
}

fn http_attach_stream(root: &std::path::Path, id: &str, cols: u16, rows: u16, capabilities: ClientCapabilities) -> UnixStream {
    http_upgrade_stream(root, id, "attach", cols, rows, capabilities)
}

fn http_watch_stream(root: &std::path::Path, id: &str, cols: u16, rows: u16, capabilities: ClientCapabilities) -> UnixStream {
    http_upgrade_stream(root, id, "watch", cols, rows, capabilities)
}

fn http_upgrade_stream(
    root: &std::path::Path,
    id: &str,
    action: &str,
    cols: u16,
    rows: u16,
    capabilities: ClientCapabilities,
) -> UnixStream {
    let socket_path = session_socket_path(root, id);
    let mut stream = UnixStream::connect(&socket_path).expect("connect session socket");
    let color_level = match capabilities.color_level {
        ColorLevel::Sixteen => "sixteen",
        ColorLevel::Ansi256 => "ansi256",
        ColorLevel::TrueColor => "true_color",
    };
    let body = format!(
        r#"{{"cols":{cols},"rows":{rows},"capabilities":{{"color_level":"{color_level}","kitty_keyboard":{}}}}}"#,
        capabilities.kitty_keyboard
    );
    write!(
        stream,
        "POST /sessions/{id}/{action} HTTP/1.1\r\nHost: cleat\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: Upgrade\r\nUpgrade: cleat-attach/1\r\n\r\n{}",
        body.len(),
        body
    )
    .expect("write upgrade request");

    let response = read_http_response_head(&mut stream);
    assert!(response.starts_with("HTTP/1.1 101 Switching Protocols\r\n"), "{response}");
    stream
}

fn read_http_response_head(stream: &mut UnixStream) -> String {
    let mut bytes = Vec::new();
    while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).expect("read response head");
        bytes.push(byte[0]);
    }
    String::from_utf8(bytes).expect("response utf8")
}

fn collect_output_until(stream: &mut UnixStream, needle: &str, timeout: Duration) -> String {
    stream.set_read_timeout(Some(Duration::from_millis(100))).expect("set read timeout");
    let deadline = Instant::now() + timeout;
    let mut output = Vec::new();
    while Instant::now() < deadline {
        match Frame::read(stream) {
            Ok(Frame::Output(bytes)) => {
                output.extend_from_slice(&bytes);
                let text = String::from_utf8_lossy(&output);
                if text.contains(needle) {
                    return text.into_owned();
                }
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock || err.kind() == std::io::ErrorKind::TimedOut => {}
            Err(err) => panic!("read output frame: {err}"),
        }
    }
    String::from_utf8_lossy(&output).into_owned()
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

#[cfg(all(not(feature = "ghostty-vt"), not(windows)))]
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

#[cfg(all(not(feature = "ghostty-vt"), not(windows)))]
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
fn list_reports_watch_only_session_as_detached() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), Some(VtEngineKind::Passthrough), None, Some("sleep 30".into()), false).expect("create alpha");
    let _watch = http_watch_stream(temp.path(), "alpha", 80, 24, ClientCapabilities::conservative_fallback());

    let listed = service.list().expect("list sessions");

    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "alpha");
    assert_eq!(listed[0].status, cleat::protocol::SessionStatus::Detached);
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

#[test]
fn session_daemon_accepts_http_control_requests_on_session_socket() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), Some(VtEngineKind::Passthrough), None, Some("sleep 30".into()), false).expect("create alpha");

    let health = http_session_request(temp.path(), "alpha", "GET /healthz HTTP/1.1\r\nHost: cleat\r\n\r\n");
    assert!(health.starts_with("HTTP/1.1 200 OK\r\n"), "{health}");
    assert!(http_body(&health).contains("\"service\":\"cleat-session\""));

    let inspect = http_session_request(temp.path(), "alpha", "GET /sessions/alpha HTTP/1.1\r\nHost: cleat\r\n\r\n");
    assert!(inspect.starts_with("HTTP/1.1 200 OK\r\n"), "{inspect}");
    let inspect_json: serde_json::Value = serde_json::from_str(http_body(&inspect)).expect("inspect json");
    assert_eq!(inspect_json["session"]["id"], "alpha");

    let keys_body = r#"{"bytes":[104,105,10]}"#;
    let keys = http_session_request(
        temp.path(),
        "alpha",
        &format!("POST /sessions/alpha/keys HTTP/1.1\r\nHost: cleat\r\nContent-Length: {}\r\n\r\n{}", keys_body.len(), keys_body),
    );
    assert!(keys.starts_with("HTTP/1.1 204 No Content\r\n"), "{keys}");

    let record_body = r#"{"enable":true}"#;
    let record = http_session_request(
        temp.path(),
        "alpha",
        &format!("POST /sessions/alpha/record HTTP/1.1\r\nHost: cleat\r\nContent-Length: {}\r\n\r\n{}", record_body.len(), record_body),
    );
    assert!(record.starts_with("HTTP/1.1 204 No Content\r\n"), "{record}");

    let mark_body = r#"{"name":"m1"}"#;
    let mark = http_session_request(
        temp.path(),
        "alpha",
        &format!("POST /sessions/alpha/mark HTTP/1.1\r\nHost: cleat\r\nContent-Length: {}\r\n\r\n{}", mark_body.len(), mark_body),
    );
    assert!(mark.starts_with("HTTP/1.1 200 OK\r\n"), "{mark}");
    let mark_json: serde_json::Value = serde_json::from_str(http_body(&mark)).expect("mark json");
    assert!(mark_json["offset"].is_u64());

    let input_text_body = r#"{"kind":"text","text":"structured input"}"#;
    let input_text = http_session_request(
        temp.path(),
        "alpha",
        &format!(
            "POST /sessions/alpha/input HTTP/1.1\r\nHost: cleat\r\nContent-Length: {}\r\n\r\n{}",
            input_text_body.len(),
            input_text_body
        ),
    );
    assert!(input_text.starts_with("HTTP/1.1 204 No Content\r\n"), "{input_text}");

    let input_key_body = r#"{"kind":"key","key":{"kind":"named","key":"enter"}}"#;
    let input_key = http_session_request(
        temp.path(),
        "alpha",
        &format!(
            "POST /sessions/alpha/input HTTP/1.1\r\nHost: cleat\r\nContent-Length: {}\r\n\r\n{}",
            input_key_body.len(),
            input_key_body
        ),
    );
    assert!(input_key.starts_with("HTTP/1.1 204 No Content\r\n"), "{input_key}");

    let resize_body = r#"{"cols":12,"rows":7}"#;
    let resize = http_session_request(
        temp.path(),
        "alpha",
        &format!("POST /sessions/alpha/resize HTTP/1.1\r\nHost: cleat\r\nContent-Length: {}\r\n\r\n{}", resize_body.len(), resize_body),
    );
    assert!(resize.starts_with("HTTP/1.1 204 No Content\r\n"), "{resize}");

    let resized = http_session_request(temp.path(), "alpha", "GET /sessions/alpha HTTP/1.1\r\nHost: cleat\r\n\r\n");
    let resized_json: serde_json::Value = serde_json::from_str(http_body(&resized)).expect("resized inspect json");
    assert_eq!(resized_json["terminal"]["cols"], 12);
    assert_eq!(resized_json["terminal"]["rows"], 7);

    let input_resize_body = r#"{"kind":"resize","cols":14,"rows":8}"#;
    let input_resize = http_session_request(
        temp.path(),
        "alpha",
        &format!(
            "POST /sessions/alpha/input HTTP/1.1\r\nHost: cleat\r\nContent-Length: {}\r\n\r\n{}",
            input_resize_body.len(),
            input_resize_body
        ),
    );
    assert!(input_resize.starts_with("HTTP/1.1 204 No Content\r\n"), "{input_resize}");
    let input_resized = http_session_request(temp.path(), "alpha", "GET /sessions/alpha HTTP/1.1\r\nHost: cleat\r\n\r\n");
    let input_resized_json: serde_json::Value = serde_json::from_str(http_body(&input_resized)).expect("input resized inspect json");
    assert_eq!(input_resized_json["terminal"]["cols"], 14);
    assert_eq!(input_resized_json["terminal"]["rows"], 8);

    let screen = http_session_request(temp.path(), "alpha", "GET /sessions/alpha/screen HTTP/1.1\r\nHost: cleat\r\n\r\n");
    assert!(screen.starts_with("HTTP/1.1 409 Conflict\r\n"), "{screen}");
    assert!(http_body(&screen).contains("placeholder"));

    let snapshot = http_session_request(temp.path(), "alpha", "GET /sessions/alpha/snapshot HTTP/1.1\r\nHost: cleat\r\n\r\n");
    assert!(snapshot.starts_with("HTTP/1.1 409 Conflict\r\n"), "{snapshot}");
    assert!(http_body(&snapshot).contains("placeholder"));

    service.kill("alpha").expect("kill alpha");
}

#[test]
fn daemon_provider_keeps_session_alive_after_provider_close() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::Builder::new().prefix("cleat-provider-").tempdir_in("/tmp").expect("tempdir");
    let root = temp.path().to_string_lossy();
    let command = b"sleep 30";

    unsafe {
        let provider = cleat_provider_open(&CleatProviderDesc {
            abi_version: CLEAT_PROVIDER_ABI_VERSION,
            requested_features: ProviderFeatures::CELL_SNAPSHOTS.bits(),
            backend: CLEAT_PROVIDER_BACKEND_DAEMON,
            runtime_root: root.as_ptr(),
            runtime_root_len: root.len(),
        });
        assert!(!provider.is_null());

        let session = cleat_session_create(provider, &CleatSessionDesc {
            cols: 80,
            rows: 24,
            vt_engine: CLEAT_PROVIDER_VT_PASSTHROUGH,
            command: command.as_ptr(),
            command_len: command.len(),
            ..CleatSessionDesc::default()
        });
        assert!(!session.is_null());
        assert!(cleat_session_write_bytes(session, b"ignored\n".as_ptr(), b"ignored\n".len()));

        cleat_session_destroy(session);
        cleat_provider_close(provider);
    }

    let service = service_for(temp.path());
    let sessions = service.list().expect("list sessions");
    assert_eq!(sessions.len(), 1);
    service.kill(&sessions[0].id).expect("kill daemon session");
}

#[test]
fn daemon_provider_uses_client_supplied_id() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::Builder::new().prefix("cleat-provider-").tempdir_in("/tmp").expect("tempdir");
    let root = temp.path().to_string_lossy();
    let command = b"sleep 30";
    let id = b"client-chosen-id";

    unsafe {
        let provider = cleat_provider_open(&CleatProviderDesc {
            abi_version: CLEAT_PROVIDER_ABI_VERSION,
            requested_features: ProviderFeatures::CELL_SNAPSHOTS.bits(),
            backend: CLEAT_PROVIDER_BACKEND_DAEMON,
            runtime_root: root.as_ptr(),
            runtime_root_len: root.len(),
        });
        assert!(!provider.is_null());

        let session = cleat_session_create(provider, &CleatSessionDesc {
            cols: 80,
            rows: 24,
            vt_engine: CLEAT_PROVIDER_VT_PASSTHROUGH,
            command: command.as_ptr(),
            command_len: command.len(),
            id: id.as_ptr(),
            id_len: id.len(),
            ..CleatSessionDesc::default()
        });
        assert!(!session.is_null());

        cleat_session_destroy(session);
        cleat_provider_close(provider);
    }

    let service = service_for(temp.path());
    let sessions = service.list().expect("list sessions");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, "client-chosen-id");
    service.kill("client-chosen-id").expect("kill daemon session");
}

// A crashed daemon leaves its socket and PID files behind without going through
// the graceful-exit cleanup. Re-creating the session must respawn a live daemon
// from the surviving recording rather than reusing the stale socket.
#[test]
fn create_respawns_over_a_stale_crashed_daemon() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::Builder::new().prefix("cleat-crash-").tempdir_in("/tmp").expect("tempdir");
    let root = temp.path();
    let id = "crashed";

    // Hand-build the husk a crashed recording daemon leaves behind: a session dir
    // with a real recording, a leftover socket file with no listener, and a PID
    // file pointing at a process that is not a live cleat.
    let dir = root.join(id);
    std::fs::create_dir_all(&dir).expect("create session dir");
    let mut recorder = SessionRecorder::new(&dir, 80, 24, "passthrough").expect("recorder");
    recorder.output(b"prior activation output\r\n", Duration::from_millis(1));
    recorder.flush();
    drop(recorder);
    assert!(dir.join(CAST_FILE_NAME).exists(), "recording should exist before respawn");

    let socket_path = session_socket_path(root, id);
    let stale = std::os::unix::net::UnixListener::bind(&socket_path).expect("bind stale socket");
    drop(stale);
    std::fs::write(daemon_pid_path(root, id), "999999999").expect("write stale pid");

    // Re-create with the same id: must respawn rather than reuse the dead socket.
    let service = service_for(root);
    service
        .create(Some(id.into()), Some(VtEngineKind::Passthrough), None, Some("sleep 30".into()), true)
        .expect("respawn over stale daemon");
    wait_for_socket(&socket_path);

    // The respawned daemon answers control requests and is the live session.
    let sessions = service.list().expect("list sessions");
    assert_eq!(sessions.len(), 1, "exactly one live session after respawn");
    assert_eq!(sessions[0].id, id);
    service.kill(id).expect("kill respawned session");
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

#[cfg(all(not(feature = "ghostty-vt"), not(windows)))]
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

    let _stream = http_attach_stream(temp.path(), "alpha", 100, 30, ClientCapabilities::new(ColorLevel::Ansi256, true));

    let err = service.attach(Some("alpha".into()), None, None, None, false).expect_err("second attach should fail");
    assert!(err.contains("foreground client"));
}

#[test]
fn http_detach_records_only_real_foreground_transition() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), Some(VtEngineKind::Passthrough), None, Some("sleep 30".into()), true).expect("create alpha");

    let attach_stream = http_attach_stream(temp.path(), "alpha", 80, 24, ClientCapabilities::conservative_fallback());

    let first_detach = http_session_request(temp.path(), "alpha", "POST /sessions/alpha/detach HTTP/1.1\r\nHost: cleat\r\n\r\n");
    assert!(first_detach.starts_with("HTTP/1.1 204 No Content\r\n"), "{first_detach}");

    let second_detach = http_session_request(temp.path(), "alpha", "POST /sessions/alpha/detach HTTP/1.1\r\nHost: cleat\r\n\r\n");
    assert!(second_detach.starts_with("HTTP/1.1 204 No Content\r\n"), "{second_detach}");
    drop(attach_stream);

    let cast_path = temp.path().join("alpha").join(cleat::recording::CAST_FILE_NAME);
    let events = cleat::cast_reader::read_all_events_since(&cast_path, 0).expect("read cast events");
    let attach_count = events.iter().filter(|event| matches!(event.code, cleat::asciicast::EventCode::Custom('a'))).count();
    let detach_count = events.iter().filter(|event| matches!(event.code, cleat::asciicast::EventCode::Custom('d'))).count();

    assert_eq!(attach_count, 1, "expected one foreground attach event, got {events:?}");
    assert_eq!(detach_count, 1, "expected one foreground detach event, got {events:?}");

    service.kill("alpha").expect("kill session");
}

#[test]
fn lifecycle_attach_init_capabilities_drive_replay_output_on_daemon_path() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvVarGuard::set("CLEAT_TEST_VT_ENGINE", "replay-probe");

    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, None, Some("sleep 5".into()), false).expect("create alpha");

    let mut stream = http_attach_stream(temp.path(), "alpha", 100, 30, ClientCapabilities::new(ColorLevel::Ansi256, true));

    let replay = Frame::read(&mut stream).expect("read replay output");
    assert_eq!(replay, Frame::Output(b"Ansi256:true".to_vec()));
}

#[test]
fn lifecycle_watch_gets_replay_without_resizing_session() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvVarGuard::set("CLEAT_TEST_VT_ENGINE", "replay-probe");

    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, None, Some("sleep 5".into()), false).expect("create alpha");

    let mut stream = http_watch_stream(temp.path(), "alpha", 100, 30, ClientCapabilities::new(ColorLevel::Ansi256, true));

    let replay = Frame::read(&mut stream).expect("read watch replay output");
    assert_eq!(replay, Frame::Output(b"Ansi256:true".to_vec()));
    let result = service.inspect("alpha").expect("inspect watched session");
    assert_eq!(result.terminal.cols, 80);
    assert_eq!(result.terminal.rows, 24);
    assert_eq!(result.attachments.iter().map(|attachment| attachment.role.as_str()).collect::<Vec<_>>(), vec!["watcher"]);
}

#[test]
fn send_keys_injects_input_into_running_session_pty() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, None, Some("cat".into()), false).expect("create alpha");

    let mut stream = http_attach_stream(temp.path(), "alpha", 100, 30, ClientCapabilities::conservative_fallback());

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
fn watcher_receives_live_output_without_taking_control() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), Some(VtEngineKind::Passthrough), None, Some("cat".into()), false).expect("create alpha");

    let mut controller = http_attach_stream(temp.path(), "alpha", 100, 30, ClientCapabilities::conservative_fallback());
    let mut watcher = http_watch_stream(temp.path(), "alpha", 12, 7, ClientCapabilities::conservative_fallback());

    let result = service.inspect("alpha").expect("inspect with watcher");
    assert_eq!(result.terminal.cols, 100, "watcher geometry should not resize the session");
    assert_eq!(result.terminal.rows, 30, "watcher geometry should not resize the session");
    let roles = result.attachments.iter().map(|attachment| attachment.role.as_str()).collect::<Vec<_>>();
    assert_eq!(roles, vec!["controller", "watcher"]);

    Frame::Input(b"ignored-from-watcher\n".to_vec()).write(&mut watcher).expect("write watcher input");
    std::thread::sleep(Duration::from_millis(100));
    service.send_keys("alpha", b"watched-from-control\n").expect("send keys");

    let controller_output = collect_output_until(&mut controller, "watched-from-control", Duration::from_secs(2));
    let watcher_output = collect_output_until(&mut watcher, "watched-from-control", Duration::from_secs(2));

    assert!(controller_output.contains("watched-from-control"), "controller output was {controller_output:?}");
    assert!(watcher_output.contains("watched-from-control"), "watcher output was {watcher_output:?}");
    assert!(!watcher_output.contains("ignored-from-watcher"), "watcher input should be ignored, got {watcher_output:?}");
}

#[test]
fn send_keys_cli_executes_end_to_end() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, None, Some("cat".into()), false).expect("create alpha");

    let mut stream = http_attach_stream(temp.path(), "alpha", 100, 30, ClientCapabilities::conservative_fallback());

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
    let mut stream = http_attach_stream(temp.path(), "alpha", 80, 24, ClientCapabilities::conservative_fallback());
    stream.set_read_timeout(Some(Duration::from_millis(100))).ok();

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

    let mut first = http_attach_stream(temp.path(), "alpha", 100, 30, ClientCapabilities::new(ColorLevel::Ansi256, true));

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

    let mut second = http_attach_stream(temp.path(), "alpha", 100, 30, ClientCapabilities::new(ColorLevel::Ansi256, true));

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

    let mut stream = http_attach_stream(temp.path(), "alpha", 100, 30, ClientCapabilities::new(ColorLevel::Ansi256, true));

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
fn create_with_size_sets_initial_terminal_geometry() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let info = service
        .create_with_size(Some("sized".into()), Some(VtEngineKind::Passthrough), None, Some("sleep 30".into()), false, TerminalSize {
            cols: 120,
            rows: 40,
        })
        .expect("create session");

    let socket_path = session_socket_path(temp.path(), &info.id);
    wait_for_socket(&socket_path);

    let deadline = Instant::now() + Duration::from_secs(2);
    let result = loop {
        match service.inspect(&info.id) {
            Ok(result) => break result,
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) => panic!("inspect sized session: {err}"),
        }
    };

    assert_eq!(result.terminal.cols, 120);
    assert_eq!(result.terminal.rows, 40);

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

    // Resolve range via the live daemon (socket-backed marker lookup), then
    // play into a buffer so we can assert the actual bytes rather than just
    // ExecResult::Ok.
    let cast_path = service.layout_root().join("alpha").join(cleat::recording::CAST_FILE_NAME);
    let (so, eo, _status) = service
        .resolve_slice_range(
            "alpha",
            cleat::server::StartBound::Marker("m1".into()),
            cleat::server::EndBound::Marker("m2".into()),
            &cast_path,
        )
        .expect("resolve");

    let opts = cleat::replay::ReplayOptions { speed: 1_000_000.0, max_idle: Some(Duration::ZERO) };
    let mut buf: Vec<u8> = Vec::new();
    cleat::replay::run_replay(&cast_path, so, eo, &opts, &mut buf, |_| {}).expect("run_replay");

    let output = String::from_utf8(buf).expect("utf-8");
    assert!(output.contains("middle"), "expected 'middle' between m1 and m2, got {output:?}");
    assert!(!output.contains("trailing"), "did not expect 'trailing' before m2, got {output:?}");

    // Cleanup.
    let _ = service.kill("alpha");
}
