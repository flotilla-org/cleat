#![cfg(unix)]

use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

#[cfg(feature = "ghostty-vt")]
use cleat::runtime::TerminalSize;
use cleat::{runtime::RuntimeLayout, server::SessionService, vt::VtEngineKind};
use serde_json::{json, Value};

struct Fixture {
    temp: tempfile::TempDir,
    daemon: Child,
    service: SessionService,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let daemon = Command::new(env!("CARGO_BIN_EXE_cleat"))
            .args(["--runtime-root", temp.path().to_str().unwrap(), "serve"])
            .env_remove("CLEAT_DAEMON")
            .env_remove("CLEAT_SESSION")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let service = SessionService::new(RuntimeLayout::new(temp.path().into()));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !temp.path().join("default@1/socket").exists() {
            assert!(Instant::now() < deadline, "daemon did not start");
            std::thread::sleep(Duration::from_millis(10));
        }
        Self { temp, daemon, service }
    }
    fn cli(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_cleat"))
            .args(["--runtime-root", self.temp.path().to_str().unwrap()])
            .args(args)
            .env_remove("CLEAT_DAEMON")
            .env_remove("CLEAT_SESSION")
            .output()
            .unwrap()
    }
    fn http(&self, id: &str, method: &str, query: &str, body: Value) -> (u16, Value) {
        let mut stream = UnixStream::connect(cleat::session::session_socket_path(self.temp.path(), id)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let body = body.to_string();
        write!(
            stream,
            "{method} /sessions/{id}/{query} HTTP/1.1\r\nHost: cleat\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        (head.split_whitespace().nth(1).unwrap().parse().unwrap(), serde_json::from_str(body).unwrap())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.service.kill("screen");
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

// No-VT screen queries must fail explicitly, including selector waits that
// would otherwise remain pending. Malformed selectors are client errors.
#[test]
fn unsupported_screen_queries() {
    let f = Fixture::new();
    f.service.create(Some("screen".into()), Some(VtEngineKind::Passthrough), None, Some("sleep 120".into()), false).unwrap();
    for query in ["screen", "screen?tree=1", "screen?select=region"] {
        let (status, body) = f.http("screen", "GET", query, json!(null));
        assert_eq!(status, 409);
        assert_eq!(body["error"]["code"], "unsupported");
    }
    let (status, body) = f.http("screen", "GET", "screen?select=%3Ainvalid", json!(null));
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "invalid_selector");
    let start = Instant::now();
    let output = f.cli(&["wait", "screen", "--select", "region", "--timeout", "60"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unsupported"));
}

// Replay the #312 Claude and Codex recordings through real daemon sessions.
// CLI and HTTP select the graph composer/footer regions, both with an attached
// packet watcher and without one; no role recognizer is required.
#[cfg(feature = "ghostty-vt")]
#[test]
fn recorded_sessions_select_regions_and_wait() {
    for (cast, composer, footer) in [
        (include_str!("../../../docs/design/semantic-prompt-evidence/osc133-codex-normal.cast"), "draft probe", "GPT-6.1-Sol"),
        (include_str!("../../../docs/design/semantic-prompt-evidence/osc133-claude-ready-normal.cast"), "draft probe", "auto mode on"),
    ] {
        let f = Fixture::new();
        let mut lines = cast.lines();
        let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        let output: String = lines
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|event| event[1] == "o")
            .map(|event| event[2].as_str().unwrap().to_string())
            .collect();
        let path = f.temp.path().join("output");
        std::fs::write(&path, output).unwrap();
        f.service
            .create_with_size(
                Some("screen".into()),
                Some(VtEngineKind::Ghostty),
                None,
                Some(format!("stty raw -echo; cat {}; sleep 120", path.display())),
                false,
                TerminalSize {
                    cols: header["term"]["cols"].as_u64().unwrap() as u16,
                    rows: header["term"]["rows"].as_u64().unwrap() as u16,
                },
            )
            .unwrap();
        assert_eq!(f.cli(&["wait", "screen", "--text", composer, "--timeout", "10"]).status.code(), Some(0));
        let (mut packets, _) = f.service.connect_packets("screen").unwrap();
        packets.open_channel(1, "screen", cleat::packet::ChannelRole::Watcher).unwrap();
        let _ = packets.read_render(1).unwrap();
        // Keep a legacy HTTP attachment alive alongside the packet watcher.
        let mut http_watch = UnixStream::connect(cleat::session::session_socket_path(f.temp.path(), "screen")).unwrap();
        http_watch.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let body = json!({"cols":header["term"]["cols"],"rows":header["term"]["rows"],"capabilities":{"color_level":"true_color","kitty_keyboard":true}}).to_string();
        write!(http_watch, "POST /sessions/screen/watch HTTP/1.1\r\nHost: cleat\r\nContent-Length: {}\r\nConnection: Upgrade\r\nUpgrade: cleat-attach/1\r\nx-cleat-output-context: {{\"version\":1,\"context\":{{\"kind\":\"external\"}}}}\r\n\r\n{body}", body.len()).unwrap();
        let mut response = Vec::new();
        while !response.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            http_watch.read_exact(&mut byte).unwrap();
            response.push(byte[0]);
        }
        assert!(String::from_utf8_lossy(&response).starts_with("HTTP/1.1 101"), "{}", String::from_utf8_lossy(&response));
        for text in [composer, footer] {
            let select = format!("region:has-text('{text}'):last-match");
            let cli = f.cli(&["inspect", "screen", "--screen", "--select", &select]);
            assert!(cli.status.success(), "{}", String::from_utf8_lossy(&cli.stderr));
            let cli: Value = serde_json::from_slice(&cli.stdout).unwrap();
            let encoded: String = select.bytes().map(|b| format!("%{b:02X}")).collect();
            let (status, http) = f.http("screen", "GET", &format!("screen?select={encoded}"), json!(null));
            assert_eq!(status, 200);
            assert_eq!(cli, http);
            let node = &cli["nodes"][0];
            assert_eq!(node["element"], "region", "selector {select}: {cli}; screen={:?}", f.service.capture("screen"));
            assert!(node["text"].as_str().unwrap().contains(text));
            assert_eq!(node["attributes"]["provenance"], "graph");
            assert!(node["confidence"].is_number());
            assert!(node["bounds"]["width"].as_u64().unwrap() > 0);
            let ready = f.cli(&["wait", "screen", "--select", &select, "--timeout", "0", "--json"]);
            assert_eq!(ready.status.code(), Some(0));
            assert_eq!(serde_json::from_slice::<Value>(&ready.stdout).unwrap()["condition"], json!({"kind":"select","select":select}));
        }
        let (_, raw) = f.http("screen", "GET", "screen", json!(null));
        let text = f.cli(&["inspect", "screen", "--screen", "--text-only"]);
        assert_eq!(String::from_utf8(text.stdout).unwrap(), format!("{}\n", raw["text"].as_str().unwrap()));
        drop(packets);
        let (_, ready) = f.http("screen", "POST", "wait", json!({"select":"region","timeout_ms":0}));
        assert_eq!(ready["condition"]["kind"], "select");
        let timeout = f.cli(&["wait", "screen", "--select", "dialog", "--timeout", "0.05", "--json"]);
        assert_eq!(timeout.status.code(), Some(1));
        assert_eq!(serde_json::from_slice::<Value>(&timeout.stdout).unwrap()["status"], "timeout");
        let invalid = f.cli(&["wait", "screen", "--select", "region:nth-match(0)"]);
        assert_eq!(invalid.status.code(), Some(2));
    }
}

// An unmatched selector stays pending, then fires on new output; it ORs with
// existing conditions and reports the actual winner through HTTP and the CLI.
#[cfg(feature = "ghostty-vt")]
#[test]
fn selector_wait_observes_later_output_and_ors() {
    let f = Fixture::new();
    f.service
        .create(
            Some("screen".into()),
            Some(VtEngineKind::Ghostty),
            None,
            Some("stty raw -echo; printf initial; dd bs=1 count=1 >/dev/null 2>&1; printf '\\r\\nready'; sleep 120".into()),
            false,
        )
        .unwrap();
    assert_eq!(f.cli(&["wait", "screen", "--text", "initial", "--timeout", "10"]).status.code(), Some(0));
    std::thread::scope(|scope| {
        let (sender, receiver) = std::sync::mpsc::channel();
        let f = &f;
        scope.spawn(move || {
            sender
                .send(f.cli(&["wait", "screen", "--select", "region:has-text('ready')", "--idle-time", "30", "--timeout", "5", "--json"]))
                .unwrap();
        });
        assert!(
            matches!(receiver.recv_timeout(Duration::from_millis(200)), Err(std::sync::mpsc::RecvTimeoutError::Timeout)),
            "unmatched selector returned early"
        );
        f.service.send_keys("screen", b"x").unwrap();
        let output = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(output.status.code(), Some(0), "{}", String::from_utf8_lossy(&output.stderr));
        let ready: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(ready["condition"]["kind"], "select");
    });
    let (status, ready) =
        f.http("screen", "POST", "wait", json!({"select":"dialog","conditions":[{"kind":"output_idle","quiet_ms":0}],"timeout_ms":1000}));
    assert_eq!(status, 200);
    assert_eq!(ready["condition"]["kind"], "output_idle");
    let (status, ready) =
        f.http("screen", "POST", "wait", json!({"select":"dialog","conditions":[{"kind":"text_match","text":"ready"}],"timeout_ms":1000}));
    assert_eq!(status, 200);
    assert_eq!(ready["condition"]["kind"], "text_match");
    for select in ["", "region:nth-child(2n+1)", "region:nth-match(0)"] {
        let (status, invalid) = f.http("screen", "POST", "wait", json!({"select":select,"timeout_ms":1000}));
        assert_eq!(status, 400);
        assert_eq!(invalid["error"]["code"], "invalid_selector");
    }
}
