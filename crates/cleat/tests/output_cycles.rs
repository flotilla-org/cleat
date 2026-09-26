#![cfg(unix)]
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    time::{Duration, Instant},
};

use cleat::{
    packet::{
        ChannelRole, ControlError, OpenChannel, PacketFrame, Resize, CHANNEL_CONTROL, MSG_CONTROL_ERROR, MSG_CONTROL_OPEN_CHANNEL,
        MSG_SESSION_RESIZE,
    },
    protocol::AttachmentIdentity,
    runtime::RuntimeLayout,
    server::SessionService,
    vt::VtEngineKind,
};

struct Session {
    _root: tempfile::TempDir,
    layout: RuntimeLayout,
    service: SessionService,
}
impl Session {
    fn new(daemon: &str) -> Self {
        Self::with_engine(daemon, VtEngineKind::Passthrough)
    }
    fn with_engine(daemon: &str, engine: VtEngineKind) -> Self {
        let root = tempfile::tempdir().unwrap();
        let layout = RuntimeLayout::new(root.path().to_owned()).with_daemon(daemon.into()).unwrap();
        let service = SessionService::new(layout.clone());
        service
            .create(Some("same".into()), Some(engine), None, Some(r"printf '\033[1;1H\033[31mrepaint\033[0m'; sleep 60".into()), true)
            .unwrap();
        Self { _root: root, layout, service }
    }
    fn context(&self) -> String {
        serde_json::json!({"version": 1, "context": {"kind": "session", "source": {
            "runtime_root": self.layout.root(), "daemon": self.layout.daemon_name(), "session": "same"
        }}})
        .to_string()
    }
    fn connect(&self, action: &str, context: Option<&str>) -> (UnixStream, String) {
        let mut stream = UnixStream::connect(self.layout.socket_path()).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let (path, upgrade, body) = if action == "connect" || action == "activity" || action == "filtered-activity" {
            let body = match action {
                "activity" => r#"{"screen_activity_stable_ms":10}"#,
                "filtered-activity" => r#"{"screen_activity_stable_ms":10,"selectors":["observe"]}"#,
                _ => "{}",
            };
            ("/connect".into(), "cleat-packet/1", body.to_owned())
        } else {
            (
                format!("/sessions/same/{}", if action == "strict" { "attach" } else { action }),
                "cleat-attach/1",
                serde_json::json!({
                    "cols": 80, "rows": 1, "take": action != "strict", "strict": action == "strict",
                    "capabilities": {"color_level": "true_color", "kitty_keyboard": false}
                })
                .to_string(),
            )
        };
        let context_header = context.map(|c| format!("x-cleat-output-context: {c}\r\n")).unwrap_or_default();
        write!(stream, "POST {path} HTTP/1.1\r\nHost: cleat\r\nConnection: Upgrade\r\nUpgrade: {upgrade}\r\n{context_header}Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            head.push(byte[0]);
        }
        (stream, String::from_utf8(head).unwrap())
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.service.kill("same");
    }
}

fn rejected(session: &Session, action: &str, context: Option<&str>, status: u16, message: &str) {
    let before = session.service.inspect("same").unwrap();
    let (mut stream, head) = session.connect(action, context);
    assert!(head.starts_with(&format!("HTTP/1.1 {status}")), "{head}");
    let mut body = String::new();
    stream.read_to_string(&mut body).unwrap();
    assert!(body.contains(message), "{body}");
    assert!(!body.contains('\x1b'), "must not replay terminal output on rejection");
    let after = session.service.inspect("same").unwrap();
    assert_eq!(before.terminal.cols, after.terminal.cols);
    assert_eq!(before.terminal.rows, after.terminal.rows);
    assert_eq!(before.attachments, after.attachments);
}

#[test]
fn output_admission_rejects_old_clients_and_one_row_self_feedback_before_side_effects() {
    let session = Session::new("default");
    for action in ["attach", "watch", "connect"] {
        rejected(&session, action, None, 426, "upgrade");
        rejected(&session, action, Some(r#"{"version":99,"context":{"kind":"external"}}"#), 426, "version");
        rejected(&session, action, Some(r#"{"version":1,"context":{"kind":"remote"}}"#), 426, "remote");
    }
    for action in ["attach", "watch"] {
        rejected(&session, action, Some(&session.context()), 409, "cycle");
    }
    let before = session.service.inspect("same").unwrap();
    let (mut packet, head) = session.connect("connect", Some(&session.context()));
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    PacketFrame::read(&mut packet).unwrap();
    PacketFrame::read(&mut packet).unwrap();
    for role in [ChannelRole::Controller, ChannelRole::Watcher] {
        PacketFrame::new(CHANNEL_CONTROL, MSG_CONTROL_OPEN_CHANNEL, &OpenChannel {
            channel: 1,
            session_id: "same".into(),
            role,
            take: true,
            identity: AttachmentIdentity::default(),
        })
        .unwrap()
        .write(&mut packet)
        .unwrap();
        PacketFrame::new(1, MSG_SESSION_RESIZE, &Resize { cols: 80, rows: 1 }).unwrap().write(&mut packet).unwrap();
        let frame = PacketFrame::read(&mut packet).unwrap();
        assert_eq!(frame.msg_type, MSG_CONTROL_ERROR);
        assert!(frame.decode::<ControlError>().unwrap().message.contains("cycle"));
    }
    let after = session.service.inspect("same").unwrap();
    assert_eq!(before.terminal.rows, after.terminal.rows);
    assert_eq!(before.attachments, after.attachments);
}

#[test]
fn output_admission_tracks_watch_and_multihop_across_daemons_and_releases_disconnects() {
    let a = Session::new("one");
    let b = Session::new("two");
    let c = Session::new("three");
    let (ab, head) = b.connect("watch", Some(&a.context()));
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    let (bc, head) = c.connect("attach", Some(&b.context()));
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    for action in ["attach", "watch"] {
        rejected(&a, action, Some(&c.context()), 409, "cycle");
    }
    drop(bc);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let (ca, head) = a.connect("watch", Some(&c.context()));
        if head.starts_with("HTTP/1.1 101") {
            drop(ca);
            break;
        }
        assert!(Instant::now() < deadline, "disconnected edge must be released: {head}");
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(ab);
}

#[test]
fn output_admission_serializes_simultaneous_attempts_in_separate_daemons() {
    let a = Session::new("one");
    let b = Session::new("two");
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            barrier.wait();
            b.connect("attach", Some(&a.context()))
        });
        let second = scope.spawn(|| {
            barrier.wait();
            a.connect("watch", Some(&b.context()))
        });
        let first = first.join().unwrap();
        let second = second.join().unwrap();
        assert_ne!(first.1.starts_with("HTTP/1.1 101"), second.1.starts_with("HTTP/1.1 101"), "{} / {}", first.1, second.1);
        assert!(first.1.starts_with("HTTP/1.1 409") || second.1.starts_with("HTTP/1.1 409"));
    });
}

#[test]
fn output_admission_client_process() {
    let Ok(socket) = std::env::var("CLEAT_TEST_OUTPUT_SOCKET") else { return };
    let context = std::env::var("CLEAT_TEST_OUTPUT_CONTEXT").unwrap();
    let mut stream = UnixStream::connect(socket).unwrap();
    let body = r#"{"cols":80,"rows":1,"capabilities":{"color_level":"true_color","kitty_keyboard":false}}"#;
    write!(stream, "POST /sessions/same/watch HTTP/1.1\r\nHost: cleat\r\nUpgrade: cleat-attach/1\r\nConnection: Upgrade\r\nx-cleat-output-context: {context}\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
    }
    std::fs::write(std::env::var("CLEAT_TEST_OUTPUT_READY").unwrap(), &head).unwrap();
    if head.starts_with(b"HTTP/1.1 101") {
        loop {
            std::thread::park_timeout(Duration::from_secs(1));
        }
    }
}

#[test]
fn output_admission_cleans_up_killed_clients_and_verifies_linux_peer_coordinates() {
    let a = Session::new("one");
    let b = Session::new("two");
    for lie in [false, true] {
        if lie && !cfg!(target_os = "linux") {
            continue;
        }
        let ready = a._root.path().join(if lie { "lie-ready" } else { "client-ready" });
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "output_admission_client_process"])
            .env("CLEAT_RUNTIME_DIR", a.layout.root())
            .env("CLEAT_DAEMON", a.layout.daemon_name())
            .env("CLEAT_SESSION", "same")
            .env("CLEAT_OUTPUT_DAEMON", a.layout.resolved().unwrap().daemon_name())
            .env("CLEAT_TEST_OUTPUT_SOCKET", b.layout.socket_path())
            .env("CLEAT_TEST_OUTPUT_CONTEXT", if lie { r#"{"version":1,"context":{"kind":"external"}}"#.to_owned() } else { a.context() })
            .env("CLEAT_TEST_OUTPUT_READY", &ready)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let response = std::fs::read_to_string(&ready);
        let _ = child.kill();
        child.wait().unwrap();
        let response = response.unwrap();
        assert!(response.starts_with(if lie { "HTTP/1.1 426" } else { "HTTP/1.1 101" }), "{response}");
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let (_ba, head) = a.connect("watch", Some(&b.context()));
        if head.starts_with("HTTP/1.1 101") {
            break;
        }
        assert!(Instant::now() < deadline, "dead client left an edge: {head}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn output_admission_releases_detach_and_failed_strict_admission() {
    let a = Session::new("one");
    let b = Session::new("two");
    let (_ab, head) = b.connect("attach", Some(&a.context()));
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    b.service.detach("same").unwrap();
    let (ba, head) = a.connect("watch", Some(&b.context()));
    assert!(head.starts_with("HTTP/1.1 101"), "detached edge remained: {head}");
    drop(ba);
    // Wait for the watcher to be reaped before testing independent rollback.
    let deadline = Instant::now() + Duration::from_secs(3);
    while !a.service.inspect("same").unwrap().attachments.is_empty() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let external = r#"{"version":1,"context":{"kind":"external"}}"#;
    let (_controller, head) = b.connect("attach", Some(external));
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    rejected(&b, "strict", Some(&a.context()), 409, "seat is held");
    let (_ba, head) = a.connect("watch", Some(&b.context()));
    assert!(head.starts_with("HTTP/1.1 101"), "failed strict admission leaked an edge: {head}");
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn output_admission_packet_watcher_blocks_reverse_stream_until_channel_close() {
    use cleat::packet::{CloseChannel, MSG_CONTROL_CLOSE_CHANNEL, MSG_SESSION_ROLE};
    let a = Session::new("one");
    let b = Session::with_engine("two", VtEngineKind::Ghostty);
    let (mut packet, head) = b.connect("connect", Some(&a.context()));
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    PacketFrame::read(&mut packet).unwrap();
    PacketFrame::read(&mut packet).unwrap();
    PacketFrame::new(CHANNEL_CONTROL, MSG_CONTROL_OPEN_CHANNEL, &OpenChannel {
        channel: 1,
        session_id: "same".into(),
        role: ChannelRole::Watcher,
        take: false,
        identity: AttachmentIdentity::default(),
    })
    .unwrap()
    .write(&mut packet)
    .unwrap();
    assert_eq!(PacketFrame::read(&mut packet).unwrap().msg_type, MSG_SESSION_ROLE);
    rejected(&a, "watch", Some(&b.context()), 409, "cycle");
    PacketFrame::new(CHANNEL_CONTROL, MSG_CONTROL_CLOSE_CHANNEL, &CloseChannel { channel: 1, reason: None })
        .unwrap()
        .write(&mut packet)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let (_ba, head) = a.connect("watch", Some(&b.context()));
        if head.starts_with("HTTP/1.1 101") {
            break;
        }
        assert!(Instant::now() < deadline, "closed channel retained an edge: {head}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn output_admission_failed_packet_open_does_not_leave_a_lease() {
    let a = Session::new("one");
    let b = Session::new("two");
    let (mut packet, head) = b.connect("connect", Some(&a.context()));
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    PacketFrame::read(&mut packet).unwrap();
    PacketFrame::read(&mut packet).unwrap();
    PacketFrame::new(CHANNEL_CONTROL, MSG_CONTROL_OPEN_CHANNEL, &OpenChannel {
        channel: 1,
        session_id: "same".into(),
        role: ChannelRole::Watcher,
        take: false,
        identity: AttachmentIdentity::default(),
    })
    .unwrap()
    .write(&mut packet)
    .unwrap();
    let frame = PacketFrame::read(&mut packet).unwrap();
    assert_eq!(frame.msg_type, MSG_CONTROL_ERROR, "passthrough cannot render a packet channel");
    let (_ba, head) = a.connect("watch", Some(&b.context()));
    assert!(head.starts_with("HTTP/1.1 101"), "failed packet open retained an edge: {head}");
}

#[test]
fn output_admission_checks_activity_snapshots_and_dynamic_membership_before_events() {
    let a = Session::new("one");
    rejected(&a, "activity", Some(&a.context()), 409, "cycle");
    let (mut packet, head) = a.connect("filtered-activity", Some(&a.context()));
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    PacketFrame::read(&mut packet).unwrap(); // hello
    PacketFrame::read(&mut packet).unwrap(); // directory
    PacketFrame::read(&mut packet).unwrap(); // empty activity snapshot
    a.service.update_tags("same", vec!["observe".into()], vec![]).unwrap();
    loop {
        let frame = PacketFrame::read(&mut packet).unwrap();
        if frame.msg_type == MSG_CONTROL_ERROR {
            assert!(frame.decode::<ControlError>().unwrap().message.contains("cycle"));
            break;
        }
        assert_ne!(frame.msg_type, cleat::packet::MSG_CONTROL_ACTIVITY_EVENT, "rejected membership must not produce activity feedback");
    }
}
