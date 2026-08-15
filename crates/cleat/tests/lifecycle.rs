#![cfg(unix)]

#[cfg(feature = "ghostty-vt")]
use std::process::Stdio;
use std::{
    io::{Read, Write},
    os::unix::{
        net::UnixStream,
        process::{CommandExt, ExitStatusExt},
    },
    path::PathBuf,
    process::Command,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

#[cfg(feature = "ghostty-vt")]
use cleat::packet::ScreenActivity;
#[cfg(feature = "ghostty-vt")]
use cleat::session::foreground_path;
use cleat::{
    cli::{self, Cli, ExecResult},
    packet::{
        ActivityEvent, ActivitySnapshot, ControlHello, DirectoryDelta, DirectorySnapshot, PacketFrame, CHANNEL_CONTROL,
        MSG_CONTROL_ACTIVITY_EVENT, MSG_CONTROL_ACTIVITY_SNAPSHOT, MSG_CONTROL_DIRECTORY_DELTA, MSG_CONTROL_DIRECTORY_SNAPSHOT,
        MSG_CONTROL_HELLO, PROTOCOL_VERSION,
    },
    protocol::{AttachmentIdentity, AttachmentKind, Frame, SeatState, SessionInfo},
    provider::ProviderFeatures,
    provider_ffi::{
        cleat_provider_close, cleat_provider_directory_generation, cleat_provider_directory_release, cleat_provider_directory_snapshot,
        cleat_provider_open, cleat_session_connection_state, cleat_session_create, cleat_session_destroy, cleat_session_id,
        cleat_session_write_bytes, CleatDirectory, CleatProviderDesc, CleatSessionDesc, CleatStr, CLEAT_PROVIDER_ABI_VERSION,
        CLEAT_PROVIDER_BACKEND_DAEMON, CLEAT_PROVIDER_VT_PASSTHROUGH, CLEAT_SESSION_CLOSED,
    },
    recording::{SessionRecorder, CAST_FILE_NAME},
    runtime::{RuntimeLayout, TerminalSize, DEFAULT_DAEMON_NAME},
    server::{AttachOptions, EndBound, SessionService, StartBound},
    session::{daemon_pid_path, ensure_session_started, run_session_daemon, session_socket_path, SessionStartOptions},
    vt::{self, ClientCapabilities, ColorLevel, VtEngineKind},
};
#[cfg(feature = "ghostty-vt")]
use cleat::{
    packet::{
        Ack, ChannelRole, Input, OpenChannel, RenderPacket, RoleRequest, RoleState, MSG_CONTROL_OPEN_CHANNEL, MSG_SESSION_ACK,
        MSG_SESSION_INPUT, MSG_SESSION_RENDER, MSG_SESSION_ROLE,
    },
    provider::{TerminalInputEvent, TerminalPasteEvent, TerminalRenderUpdate, TerminalTextEvent},
    provider_ffi::{
        cleat_session_attach, cleat_session_role, cleat_session_take_control, CLEAT_PROVIDER_VT_GHOSTTY, CLEAT_ROLE_CONTROLLER,
        CLEAT_ROLE_WATCHER, CLEAT_SESSION_STREAMING,
    },
};

fn service_for(path: &std::path::Path) -> SessionService {
    SessionService::new(RuntimeLayout::new(path.to_path_buf()))
}

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
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

fn http_packet_stream(root: &std::path::Path, id: &str) -> UnixStream {
    http_packet_stream_with_selectors(root, id, &[])
}

fn http_packet_stream_with_selectors(root: &std::path::Path, id: &str, selectors: &[String]) -> UnixStream {
    let socket_path = session_socket_path(root, id);
    let mut stream = UnixStream::connect(&socket_path).expect("connect session socket");
    let body = if selectors.is_empty() { String::new() } else { serde_json::json!({ "selectors": selectors }).to_string() };
    if body.is_empty() {
        write!(
            stream,
            "POST /connect HTTP/1.1\r\nHost: cleat\r\nContent-Length: 0\r\nConnection: Upgrade\r\nUpgrade: cleat-packet/1\r\n\r\n",
        )
        .expect("write packet upgrade request");
    } else {
        write!(
            stream,
            "POST /connect HTTP/1.1\r\nHost: cleat\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: Upgrade\r\nUpgrade: cleat-packet/1\r\n\r\n{}",
            body.len(),
            body
        )
        .expect("write packet upgrade request");
    }

    let response = read_http_response_head(&mut stream);
    assert!(response.starts_with("HTTP/1.1 101 Switching Protocols\r\n"), "{response}");
    assert!(response.contains("Upgrade: cleat-packet/1\r\n"), "{response}");
    stream
}

fn http_activity_stream(root: &std::path::Path, id: &str, selectors: &[String], stable_threshold: Duration) -> UnixStream {
    let socket_path = session_socket_path(root, id);
    let mut stream = UnixStream::connect(&socket_path).expect("connect session socket");
    let body = serde_json::json!({
        "selectors": selectors,
        "screen_activity_stable_ms": stable_threshold.as_millis() as u64,
    })
    .to_string();
    write!(
        stream,
        "POST /connect HTTP/1.1\r\nHost: cleat\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: Upgrade\r\nUpgrade: cleat-packet/1\r\n\r\n{}",
        body.len(),
        body
    )
    .expect("write activity upgrade request");

    let response = read_http_response_head(&mut stream);
    assert!(response.starts_with("HTTP/1.1 101 Switching Protocols\r\n"), "{response}");
    stream
}

fn read_activity_event(stream: &mut UnixStream, timeout: Duration) -> ActivityEvent {
    stream.set_read_timeout(Some(timeout)).expect("set activity timeout");
    loop {
        let frame = PacketFrame::read(stream).expect("read activity frame");
        if frame.channel == CHANNEL_CONTROL && frame.msg_type == MSG_CONTROL_ACTIVITY_EVENT {
            return frame.decode().expect("decode activity event");
        }
    }
}

#[cfg(feature = "ghostty-vt")]
fn packet_write<T: serde::Serialize>(stream: &mut UnixStream, channel: u32, msg_type: u8, value: &T) {
    PacketFrame::new(channel, msg_type, value).expect("encode packet frame").write(stream).expect("write packet frame");
}

#[cfg(feature = "ghostty-vt")]
fn packet_open_channel(stream: &mut UnixStream, channel: u32, session_id: &str) {
    packet_open_channel_role(stream, channel, session_id, ChannelRole::Controller, false);
}

#[cfg(feature = "ghostty-vt")]
fn packet_open_channel_role(stream: &mut UnixStream, channel: u32, session_id: &str, role: ChannelRole, take: bool) {
    packet_write(stream, CHANNEL_CONTROL, MSG_CONTROL_OPEN_CHANNEL, &OpenChannel {
        channel,
        session_id: session_id.to_string(),
        role,
        take,
        identity: AttachmentIdentity::default(),
    });
}

/// Frame reader with a persistent buffer. The free-standing read helpers each
/// use a private buffer and silently drop any frame that arrived in the same
/// chunk as the one they return; when consecutive frames matter (role state
/// followed by a render), reads must share one buffer.
#[cfg(feature = "ghostty-vt")]
struct PacketReader<'a> {
    stream: &'a mut UnixStream,
    buffer: Vec<u8>,
}

#[cfg(feature = "ghostty-vt")]
impl<'a> PacketReader<'a> {
    fn new(stream: &'a mut UnixStream) -> Self {
        stream.set_read_timeout(Some(Duration::from_millis(50))).expect("set read timeout");
        Self { stream, buffer: Vec::new() }
    }

    fn next_matching(&mut self, timeout: Duration, keep: impl FnMut(&PacketFrame) -> bool, label: &str) -> PacketFrame {
        next_matching_packet_frame(self.stream, &mut self.buffer, timeout, keep, label)
    }

    fn role(&mut self, channel: u32, timeout: Duration) -> ChannelRole {
        self.role_state(channel, timeout).role
    }

    fn role_state(&mut self, channel: u32, timeout: Duration) -> RoleState {
        self.next_matching(timeout, |frame| frame.channel == channel && frame.msg_type == MSG_SESSION_ROLE, "role state packet")
            .decode::<RoleState>()
            .expect("decode role state packet")
    }

    fn render(&mut self, channel: u32, timeout: Duration) -> TerminalRenderUpdate {
        self.next_matching(timeout, |frame| frame.channel == channel && frame.msg_type == MSG_SESSION_RENDER, "render packet")
            .decode::<RenderPacket>()
            .expect("decode render packet")
            .update
    }

    fn expect_no_render(&mut self, channel: u32, timeout: Duration) {
        assert_no_matching_packet_frame(
            self.stream,
            &mut self.buffer,
            timeout,
            |frame| frame.channel == channel && frame.msg_type == MSG_SESSION_RENDER,
            "render packet",
        );
    }

    /// Drain (and ack) renders until the channel has been quiet for `quiet`.
    /// Use before a negative render assertion: late startup output (e.g. a
    /// `stty raw` mode change) or the tail of a split echo would otherwise
    /// race into the no-render window on a slow machine.
    fn settle_renders(&mut self, channel: u32, quiet: Duration, max: Duration) {
        let deadline = Instant::now() + max;
        let mut last_render = Instant::now();
        while Instant::now() < deadline && last_render.elapsed() < quiet {
            while let Some(frame) = PacketFrame::read_from_buffer(&mut self.buffer).expect("parse packet buffer") {
                if frame.channel == channel && frame.msg_type == MSG_SESSION_RENDER {
                    let update = frame.decode::<RenderPacket>().expect("decode render packet").update;
                    packet_write(self.stream, channel, MSG_SESSION_ACK, &Ack { generation: update.render_generation });
                    last_render = Instant::now();
                }
            }
            let mut chunk = [0; 4096];
            match self.stream.read(&mut chunk) {
                Ok(0) => return,
                Ok(n) => self.buffer.extend_from_slice(&chunk[..n]),
                Err(err) if packet_read_would_wait(&err) => {}
                Err(err) => panic!("read packet frame: {err}"),
            }
        }
    }
}

/// The single read/parse/timeout loop behind every packet-frame test helper.
/// Frames not selected by `keep` are consumed and discarded.
fn next_matching_packet_frame(
    stream: &mut UnixStream,
    buffer: &mut Vec<u8>,
    timeout: Duration,
    mut keep: impl FnMut(&PacketFrame) -> bool,
    label: &str,
) -> PacketFrame {
    stream.set_read_timeout(Some(Duration::from_millis(50))).expect("set read timeout");
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        while let Some(frame) = PacketFrame::read_from_buffer(buffer).expect("parse packet buffer") {
            if keep(&frame) {
                return frame;
            }
        }
        let mut chunk = [0; 4096];
        match stream.read(&mut chunk) {
            Ok(0) => panic!("packet stream closed while waiting for {label}"),
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
            Err(err) if packet_read_would_wait(&err) => {}
            Err(err) => panic!("read packet frame: {err}"),
        }
    }
    panic!("timed out waiting for {label}");
}

/// Companion to `next_matching_packet_frame`: asserts no frame selected by
/// `reject` arrives before the timeout (or the stream closes).
#[cfg(feature = "ghostty-vt")]
fn assert_no_matching_packet_frame(
    stream: &mut UnixStream,
    buffer: &mut Vec<u8>,
    timeout: Duration,
    mut reject: impl FnMut(&PacketFrame) -> bool,
    label: &str,
) {
    stream.set_read_timeout(Some(Duration::from_millis(50))).expect("set read timeout");
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        while let Some(frame) = PacketFrame::read_from_buffer(buffer).expect("parse packet buffer") {
            assert!(!reject(&frame), "unexpected {label}: {frame:?}");
        }
        let mut chunk = [0; 4096];
        match stream.read(&mut chunk) {
            Ok(0) => return,
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
            Err(err) if packet_read_would_wait(&err) => {}
            Err(err) => panic!("read packet frame: {err}"),
        }
    }
}

#[cfg(feature = "ghostty-vt")]
fn packet_ack(stream: &mut UnixStream, channel: u32, generation: u64) {
    packet_write(stream, channel, MSG_SESSION_ACK, &Ack { generation });
}

#[cfg(feature = "ghostty-vt")]
fn packet_input(stream: &mut UnixStream, channel: u32, event: TerminalInputEvent) {
    packet_write(stream, channel, MSG_SESSION_INPUT, &Input { event });
}

#[cfg(feature = "ghostty-vt")]
fn read_packet_render(stream: &mut UnixStream, buffer: &mut Vec<u8>, channel: u32, timeout: Duration) -> TerminalRenderUpdate {
    next_matching_packet_frame(
        stream,
        buffer,
        timeout,
        |frame| frame.channel == channel && frame.msg_type == MSG_SESSION_RENDER,
        &format!("render packet on channel {channel}"),
    )
    .decode::<RenderPacket>()
    .expect("decode render packet")
    .update
}

fn read_directory_delta_named(stream: &mut UnixStream, buffer: &mut Vec<u8>, timeout: Duration, label: &str) -> DirectoryDelta {
    next_matching_packet_frame(
        stream,
        buffer,
        timeout,
        |frame| frame.channel == CHANNEL_CONTROL && frame.msg_type == MSG_CONTROL_DIRECTORY_DELTA,
        label,
    )
    .decode::<DirectoryDelta>()
    .expect("decode directory delta")
}

fn read_until_directory_remove(stream: &mut UnixStream, buffer: &mut Vec<u8>, session_id: &str, timeout: Duration) -> DirectoryDelta {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let delta = read_directory_delta_named(stream, buffer, remaining, &format!("remove delta for {session_id}"));
        if delta.removed_session_ids.iter().any(|id| id == session_id) {
            return delta;
        }
        assert!(Instant::now() < deadline, "timed out waiting for remove delta for {session_id}");
    }
}

#[cfg(feature = "ghostty-vt")]
fn expect_no_render(stream: &mut UnixStream, buffer: &mut Vec<u8>, timeout: Duration) {
    assert_no_matching_packet_frame(stream, buffer, timeout, |frame| frame.msg_type == MSG_SESSION_RENDER, "render packet");
}

fn packet_read_would_wait(err: &std::io::Error) -> bool {
    matches!(err.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) || err.raw_os_error() == Some(libc::EINVAL)
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

fn http_attach_with_seat_options(
    root: &std::path::Path,
    id: &str,
    identity: AttachmentIdentity,
    strict: bool,
    take: bool,
) -> (UnixStream, String) {
    let socket_path = session_socket_path(root, id);
    let mut stream = UnixStream::connect(&socket_path).expect("connect session socket");
    let body = serde_json::json!({
        "cols": 80,
        "rows": 24,
        "capabilities": {
            "color_level": "sixteen",
            "kitty_keyboard": false
        },
        "identity": identity,
        "strict": strict,
        "take": take
    })
    .to_string();
    write!(
        stream,
        "POST /sessions/{id}/attach HTTP/1.1\r\nHost: cleat\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: Upgrade\r\nUpgrade: cleat-attach/1\r\n\r\n{}",
        body.len(),
        body
    )
    .expect("write seat-aware attach request");
    let response = read_http_response_head(&mut stream);
    (stream, response)
}

fn read_until_seat_state(stream: &mut UnixStream, timeout: Duration) -> SeatState {
    stream.set_read_timeout(Some(timeout)).expect("set seat state timeout");
    loop {
        match Frame::read(stream).expect("read attachment frame") {
            Frame::SeatState(state) => return state,
            Frame::Output(_) => {}
            other => panic!("unexpected attachment frame before seat state: {other:?}"),
        }
    }
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

fn drain_available_output(stream: &mut UnixStream) {
    stream.set_read_timeout(Some(Duration::from_millis(20))).expect("set read timeout");
    loop {
        match Frame::read(stream) {
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock || err.kind() == std::io::ErrorKind::TimedOut => return,
            Err(err) => panic!("drain output frame: {err}"),
        }
    }
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

    #[cfg(feature = "ghostty-vt")]
    fn remove(key: &'static str) -> Self {
        let original = std::env::var_os(key);
        std::env::remove_var(key);
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
    assert!(service.session_dir("alpha").exists());
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn launch_owns_term_when_daemon_environment_is_scrubbed_and_honors_override() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _term = EnvVarGuard::remove("TERM");
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());

    let default_output = temp.path().join("default-term");
    let default_command = format!("printf %s \"$TERM\" > {}; sleep 30", default_output.display());
    let default_launch = Cli::try_parse_from(["cleat", "launch", "default-term", "--no-record", "--cmd", &default_command])
        .expect("parse default TERM launch");
    cli::execute(default_launch, &service).expect("launch with scrubbed TERM");
    wait_until("default TERM output", || matches!(std::fs::read_to_string(&default_output), Ok(value) if value == "xterm-256color"));

    let override_output = temp.path().join("override-term");
    let override_command = format!("printf %s \"$TERM\" > {}; sleep 30", override_output.display());
    let override_launch = Cli::try_parse_from([
        "cleat",
        "launch",
        "override-term",
        "--env",
        "TERM=screen-256color",
        "--no-record",
        "--cmd",
        &override_command,
    ])
    .expect("parse TERM override launch");
    cli::execute(override_launch, &service).expect("launch with TERM override");
    wait_until("overridden TERM output", || matches!(std::fs::read_to_string(&override_output), Ok(value) if value == "screen-256color"));

    service.kill("default-term").expect("kill default TERM session");
    service.kill("override-term").expect("kill overridden TERM session");
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn launch_from_creates_a_sibling_in_the_source_daemon() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let source_daemon = service.with_daemon("source-daemon".to_string()).expect("source daemon service");
    let _ssh_tty = EnvVarGuard::set("SSH_TTY", "/dev/stale-tty");
    let _ssh_connection = EnvVarGuard::set("SSH_CONNECTION", "stale connection");
    let _ssh_client = EnvVarGuard::set("SSH_CLIENT", "stale client");
    source_daemon
        .create(Some("source".into()), Some(VtEngineKind::Passthrough), None, Some("sleep 30".into()), false)
        .expect("create source session");
    let _ambient_daemon = EnvVarGuard::set("CLEAT_DAEMON", "ambient-daemon");

    let mut observer = UnixStream::connect(temp.path().join("source-daemon/socket")).expect("connect source daemon");
    write!(
        observer,
        "POST /connect HTTP/1.1\r\nHost: cleat\r\nContent-Length: 0\r\nConnection: Upgrade\r\nUpgrade: cleat-packet/1\r\n\r\n"
    )
    .expect("write packet upgrade request");
    let response = read_http_response_head(&mut observer);
    assert!(response.starts_with("HTTP/1.1 101 Switching Protocols\r\n"), "{response}");
    let _hello = PacketFrame::read(&mut observer).expect("read hello");
    let initial_directory = PacketFrame::read(&mut observer).expect("read initial directory");
    let initial_directory = initial_directory.decode::<DirectorySnapshot>().expect("decode initial directory");
    assert_eq!(initial_directory.sessions.iter().map(|entry| entry.session_id.as_str()).collect::<Vec<_>>(), vec!["source"]);

    let env_output = temp.path().join("sibling-env");
    let sibling_command = format!(
        "sh -c 'printf \"%s|%s|%s|%s\" \"${{SSH_TTY-unset}}\" \"${{SSH_CONNECTION-unset}}\" \"${{SSH_CLIENT-unset}}\" \"${{CLEAT_DAEMON-unset}}\" > {}; sleep 30'",
        env_output.display()
    );
    let cli = Cli::try_parse_from([
        "cleat",
        "launch",
        "sibling",
        "--from",
        "source",
        "--tag",
        "kind=sibling",
        "--no-record",
        "--cmd",
        &sibling_command,
    ])
    .expect("parse launch --from");

    let output = cli::execute(cli, &service).expect("launch sibling").expect("sibling id");

    assert_eq!(output, "sibling");
    let mut packet_buffer = Vec::new();
    let delta = read_directory_delta_named(&mut observer, &mut packet_buffer, Duration::from_secs(2), "sibling directory delta");
    let sibling = delta.upserted.iter().find(|entry| entry.session_id == "sibling").expect("sibling upsert");
    assert_eq!(sibling.tags, vec!["kind=sibling"]);
    wait_until(
        "sibling environment output",
        || matches!(std::fs::read_to_string(&env_output), Ok(value) if value == "unset|unset|unset|source-daemon"),
    );
    assert_eq!(std::fs::read_to_string(&env_output).expect("read sibling environment"), "unset|unset|unset|source-daemon");
    assert!(service.list().expect("list default daemon").is_empty());
    assert!(service
        .with_daemon("ambient-daemon".to_string())
        .expect("ambient daemon service")
        .list()
        .expect("list ambient daemon")
        .is_empty());
    assert_eq!(source_daemon.list().expect("list source daemon").iter().map(|session| session.id.as_str()).collect::<Vec<_>>(), vec![
        "sibling", "source"
    ]);

    source_daemon.kill("sibling").expect("kill sibling");
    source_daemon.kill("source").expect("kill source");
}

#[test]
fn create_existing_session_returns_its_running_metadata() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = RuntimeLayout::new(temp.path().to_path_buf());
    let service = SessionService::new(layout.clone());
    let first_cwd = temp.path().join("first");
    let second_cwd = temp.path().join("second");
    std::fs::create_dir_all(&first_cwd).expect("create first cwd");
    std::fs::create_dir_all(&second_cwd).expect("create second cwd");

    let first = ensure_session_started(
        &layout,
        Some("alpha".into()),
        Some(VtEngineKind::Passthrough),
        Some(first_cwd),
        Some("sleep 30".into()),
        SessionStartOptions::default(),
    )
    .expect("create first session");
    let tags = service.update_tags("alpha", vec!["project=cleat".into()], Vec::new()).expect("tag running session");
    assert_eq!(tags, ["project=cleat"]);
    let second = ensure_session_started(
        &layout,
        Some("alpha".into()),
        Some(VtEngineKind::Passthrough),
        Some(second_cwd),
        Some("printf replacement".into()),
        SessionStartOptions::default(),
    )
    .expect("ensure existing session");

    let mut expected = first;
    expected.tags = tags;
    assert_eq!(second, expected);

    service.kill("alpha").expect("kill session");
}

#[test]
fn create_in_running_daemon_rejects_a_replacement_daemon() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let source_daemon = service.with_daemon("source-daemon".to_string()).expect("named daemon service");
    source_daemon
        .create(Some("source".into()), Some(VtEngineKind::Passthrough), None, Some("sleep 30".into()), false)
        .expect("create source");
    let resolved = service.daemon_owning_session("source").expect("resolve source daemon instance");

    cleat::platform::daemon::terminate_session_daemon_if_expected(temp.path(), "source-daemon");
    let socket_path = temp.path().join("source-daemon/socket");
    wait_until("source daemon exit", || UnixStream::connect(&socket_path).is_err());
    source_daemon
        .create(Some("replacement".into()), Some(VtEngineKind::Passthrough), None, Some("sleep 30".into()), false)
        .expect("start replacement daemon");

    let err = source_daemon
        .create_with_options_in_running_daemon(
            &resolved,
            Some("sibling".into()),
            Some(VtEngineKind::Passthrough),
            None,
            Some("sleep 30".into()),
            SessionStartOptions::default(),
        )
        .expect_err("replacement daemon must reject sibling launch");

    assert!(err.contains("source daemon instance changed"), "{err}");
    assert_eq!(source_daemon.list().expect("list replacement daemon").iter().map(|session| session.id.as_str()).collect::<Vec<_>>(), vec![
        "replacement"
    ]);
    source_daemon.kill("replacement").expect("kill replacement");
}

#[test]
fn daemon_exports_private_root_name_and_session_id_to_the_child() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = RuntimeLayout::new(temp.path().join("private-state")).with_daemon("agent-loop".to_string()).expect("named daemon layout");
    let service = SessionService::new(layout.clone());
    let output_path = temp.path().join("ambient-coordinates");
    let command = "printf '%s\\n%s\\n%s\\n' \"$CLEAT_RUNTIME_DIR\" \"$CLEAT_DAEMON\" \"$CLEAT_SESSION\" > ambient-coordinates; sleep 30";

    service
        .create(Some("worker".into()), Some(VtEngineKind::Passthrough), Some(temp.path().to_path_buf()), Some(command.into()), false)
        .expect("create session");
    wait_until("ambient coordinates file", || output_path.exists());

    let coordinates = std::fs::read_to_string(&output_path).expect("read ambient coordinates");
    assert_eq!(coordinates, format!("{}\nagent-loop\nworker\n", layout.root().display()));

    service.kill("worker").expect("kill session");
}

#[test]
fn daemons_lists_ambient_and_well_known_roots_best_effort() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let ambient_root = temp.path().join("private-state");
    let xdg_home = temp.path().join("xdg-state");
    let well_known_root = xdg_home.join("cleat");
    let home = temp.path().join("home");
    std::fs::create_dir_all(ambient_root.join("ambient-daemon/sessions")).expect("create ambient daemon directory");
    std::fs::create_dir_all(well_known_root.join("well-known/sessions")).expect("create well-known daemon directory");
    std::fs::create_dir_all(well_known_root.join("not a daemon/sessions")).expect("create malformed daemon directory");
    let _runtime = EnvVarGuard::set("CLEAT_RUNTIME_DIR", ambient_root.to_str().expect("utf8 ambient root"));
    let _daemon = EnvVarGuard::set("CLEAT_DAEMON", "ambient-daemon");
    let _xdg = EnvVarGuard::set("XDG_STATE_HOME", xdg_home.to_str().expect("utf8 xdg root"));
    let _home = EnvVarGuard::set("HOME", home.to_str().expect("utf8 home"));
    let service = SessionService::new(RuntimeLayout::new(ambient_root.clone()));
    let cli = Cli::try_parse_from(["cleat", "daemons", "--json"]).expect("parse daemons");

    let output = cli::execute(cli, &service).expect("execute daemons").expect("daemon list output");
    let daemons: serde_json::Value = serde_json::from_str(&output).expect("parse daemon list JSON");

    assert_eq!(
        daemons,
        serde_json::json!([
            {"name": "ambient-daemon", "runtime_root": ambient_root},
            {"name": "well-known", "runtime_root": well_known_root},
        ])
    );
}

#[test]
fn bare_list_targets_ambient_daemon_and_explicit_server_wins() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let ambient = service.with_daemon("agent-loop".to_string()).expect("ambient daemon service");
    service
        .create(Some("default-session".into()), Some(VtEngineKind::Passthrough), None, Some("sleep 30".into()), false)
        .expect("create default session");
    ambient
        .create(Some("ambient-session".into()), Some(VtEngineKind::Passthrough), None, Some("sleep 30".into()), false)
        .expect("create ambient session");
    let _daemon = EnvVarGuard::set("CLEAT_DAEMON", "agent-loop");

    let bare = Cli::try_parse_from(["cleat", "list", "--json"]).expect("parse bare list");
    let bare_output = cli::execute(bare, &service).expect("execute bare list").expect("bare list output");
    let bare_sessions: Vec<SessionInfo> = serde_json::from_str(&bare_output).expect("parse bare list JSON");
    assert_eq!(bare_sessions.iter().map(|session| session.id.as_str()).collect::<Vec<_>>(), ["ambient-session"]);

    let explicit = Cli::try_parse_from(["cleat", "--server", "default", "list", "--json"]).expect("parse explicit list");
    let explicit_output = cli::execute(explicit, &service).expect("execute explicit list").expect("explicit list output");
    let explicit_sessions: Vec<SessionInfo> = serde_json::from_str(&explicit_output).expect("parse explicit list JSON");
    assert_eq!(explicit_sessions.iter().map(|session| session.id.as_str()).collect::<Vec<_>>(), ["default-session"]);

    ambient.kill("ambient-session").expect("kill ambient session");
    service.kill("default-session").expect("kill default session");
}

#[test]
fn failures_after_protocol_upgrade_close_without_an_http_error() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    for route in ["attach", "watch", "packet"] {
        let _failure = EnvVarGuard::set("CLEAT_TEST_FAIL_AFTER_HTTP_UPGRADE", route);
        let temp = tempfile::tempdir().expect("tempdir");
        let service = service_for(temp.path());
        service
            .create(Some("alpha".into()), Some(VtEngineKind::Passthrough), None, Some("sleep 30".into()), false)
            .expect("create session");

        let mut stream = match route {
            "attach" => http_attach_stream(temp.path(), "alpha", 80, 24, ClientCapabilities::conservative_fallback()),
            "watch" => http_watch_stream(temp.path(), "alpha", 80, 24, ClientCapabilities::conservative_fallback()),
            "packet" => http_packet_stream(temp.path(), "alpha"),
            _ => unreachable!(),
        };
        let mut post_upgrade = Vec::new();
        stream.read_to_end(&mut post_upgrade).expect("read upgraded stream to close");

        assert!(post_upgrade.is_empty(), "HTTP bytes followed the {route} 101 response: {}", String::from_utf8_lossy(&post_upgrade));

        service.kill("alpha").expect("kill session");
    }
}

#[test]
fn partial_handshakes_do_not_block_ready_control_requests() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), Some(VtEngineKind::Passthrough), None, Some("sleep 30".into()), false).expect("create session");

    let socket_path = session_socket_path(temp.path(), "alpha");
    let mut partials = Vec::new();
    for _ in 0..4 {
        let mut stream = UnixStream::connect(&socket_path).expect("connect partial request");
        stream.write_all(b"GET /").expect("write partial request");
        partials.push(stream);
    }
    std::thread::sleep(Duration::from_millis(50));

    let start = Instant::now();
    let mut ready = UnixStream::connect(&socket_path).expect("connect ready request");
    ready.set_read_timeout(Some(Duration::from_secs(2))).expect("set ready response timeout");
    ready.write_all(b"GET /healthz HTTP/1.1\r\nHost: cleat\r\n\r\n").expect("write ready request");
    let response = read_http_response_head(&mut ready);

    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(start.elapsed() < Duration::from_millis(700), "ready request waited behind partial handshakes: {:?}", start.elapsed());

    drop(partials);
    service.kill("alpha").expect("kill session");
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
fn list_and_inspect_report_opaque_tags() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service
        .create_with_options(
            Some("alpha".into()),
            Some(VtEngineKind::Passthrough),
            None,
            Some("zsh".into()),
            cleat::session::SessionStartOptions {
                record: false,
                initial_size: TerminalSize::default(),
                colors: cleat::vt::TerminalColors::default(),
                tags: vec!["task=99".into(), "role=impl".into(), "role=impl".into()],
                environment: Vec::new(),
            },
        )
        .expect("create alpha");
    let cli = Cli::try_parse_from(["cleat", "list"]).expect("parse list");

    let output = cli::execute(cli, &service).expect("execute list").expect("list output");
    let listed = service.inspect("alpha").expect("inspect alpha");

    assert!(output.contains("tags=role=impl,task=99"), "{output}");
    assert_eq!(listed.session.tags, vec!["role=impl", "task=99"]);
}

#[test]
fn list_and_inspect_json_report_screen_activity_for_detached_sessions() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service
        .create(Some("alpha".into()), Some(VtEngineKind::Passthrough), None, Some("sleep 30".into()), false)
        .expect("create detached session");

    let list_cli = Cli::try_parse_from(["cleat", "list", "--json"]).expect("parse list --json");
    let list_output = cli::execute(list_cli, &service).expect("execute list --json").expect("list JSON");
    let list: serde_json::Value = serde_json::from_str(&list_output).expect("parse list JSON");
    let row = &list.as_array().expect("list array")[0];
    assert_eq!(row["screen_activity"], "stable");
    assert!(row.get("stable_since").is_some(), "stable_since field should be present: {row}");
    assert!(row.get("last_output_at").is_some(), "last_output_at field should be present: {row}");

    let inspect_cli = Cli::try_parse_from(["cleat", "inspect", "alpha", "--json"]).expect("parse inspect --json");
    let inspect_output = cli::execute(inspect_cli, &service).expect("execute inspect --json").expect("inspect JSON");
    let inspect: serde_json::Value = serde_json::from_str(&inspect_output).expect("parse inspect JSON");
    assert_eq!(inspect["screen_activity"], "stable");
    assert!(inspect.get("stable_since").is_some(), "stable_since field should be present: {inspect}");
    assert!(inspect.get("last_output_at").is_some(), "last_output_at field should be present: {inspect}");

    service.kill("alpha").expect("kill detached session");
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn detached_spinner_activity_changes_from_active_to_stable() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let vt_engine = VtEngineKind::Ghostty;
    service
        .create(
            Some("alpha".into()),
            Some(vt_engine),
            None,
            Some("sh -c 'i=0; while [ $i -lt 25 ]; do printf \"\\r%s\" \"$((i % 10))\"; i=$((i + 1)); sleep 0.1; done; sleep 30'".into()),
            false,
        )
        .expect("create outputting detached session");

    let active_deadline = Instant::now() + Duration::from_secs(5);
    let active = loop {
        let inspected = service.inspect("alpha").expect("inspect active session");
        if inspected.screen_activity == cleat::protocol::ScreenActivity::Active {
            break inspected;
        }
        assert!(Instant::now() < active_deadline, "session never reported active: {inspected:?}");
        std::thread::sleep(Duration::from_millis(50));
    };
    let last_output_at = active.last_output_at.expect("active session last_output_at");
    assert_eq!(active.stable_since, None);
    assert!(active.attachments.is_empty(), "session should remain detached");

    let stable_deadline = Instant::now() + Duration::from_secs(8);
    let stable = loop {
        let inspected = service.inspect("alpha").expect("inspect stabilizing session");
        if inspected.screen_activity == cleat::protocol::ScreenActivity::Stable && inspected.last_output_at.is_some() {
            break inspected;
        }
        assert!(Instant::now() < stable_deadline, "session never reported stable: {inspected:?}");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(stable.last_output_at.expect("stable session last_output_at") >= last_output_at);
    assert_eq!(stable.stable_since, stable.last_output_at.map(|timestamp| timestamp + 1_000));
    assert!(stable.attachments.is_empty(), "session should remain detached");

    let listed = service.list().expect("list stable session");
    assert_eq!(listed[0].screen_activity, cleat::protocol::ScreenActivity::Stable);
    assert_eq!(listed[0].stable_since, stable.stable_since);
    assert_eq!(listed[0].last_output_at, stable.last_output_at);

    service.kill("alpha").expect("kill detached session");
}

#[test]
fn list_selector_requires_exact_opaque_tag_matches() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service
        .create_with_options(
            Some("alpha".into()),
            Some(VtEngineKind::Passthrough),
            None,
            Some("zsh".into()),
            cleat::session::SessionStartOptions {
                record: false,
                initial_size: TerminalSize::default(),
                colors: cleat::vt::TerminalColors::default(),
                tags: vec!["role=impl".into(), "task=99".into()],
                environment: Vec::new(),
            },
        )
        .expect("create alpha");
    service
        .create_with_options(
            Some("beta".into()),
            Some(VtEngineKind::Passthrough),
            None,
            Some("zsh".into()),
            cleat::session::SessionStartOptions {
                record: false,
                initial_size: TerminalSize::default(),
                colors: cleat::vt::TerminalColors::default(),
                tags: vec!["role=shepherd".into()],
                environment: Vec::new(),
            },
        )
        .expect("create beta");
    let cli = Cli::try_parse_from(["cleat", "list", "--selector", "role=impl", "--selector", "task=99"]).expect("parse list");

    let output = cli::execute(cli, &service).expect("execute list").expect("list output");

    assert!(output.contains("alpha"), "{output}");
    assert!(!output.contains("beta"), "{output}");
}

#[test]
fn list_defaults_to_selected_daemon_and_all_enumerates_every_daemon() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let other = service.with_daemon("other".to_string()).expect("other daemon");
    service.create(Some("alpha".into()), Some(VtEngineKind::Passthrough), None, Some("sleep 30".into()), false).expect("create alpha");
    other.create(Some("beta".into()), Some(VtEngineKind::Passthrough), None, Some("sleep 30".into()), false).expect("create beta");

    let default_sessions = service.list().expect("list default daemon");
    let other_sessions = other.list().expect("list other daemon");
    let all_sessions = service.list_all_with_selectors(&[]).expect("list all daemons");
    let all_cli = Cli::try_parse_from(["cleat", "list", "--all"]).expect("parse list --all");
    let all_output = cli::execute(all_cli, &service).expect("execute list --all").expect("list all output");

    assert_eq!(default_sessions.iter().map(|session| session.id.as_str()).collect::<Vec<_>>(), vec!["alpha"]);
    assert_eq!(other_sessions.iter().map(|session| session.id.as_str()).collect::<Vec<_>>(), vec!["beta"]);
    assert_eq!(all_sessions.iter().map(|session| session.id.as_str()).collect::<Vec<_>>(), vec!["alpha", "beta"]);
    assert!(all_output.contains("alpha"), "{all_output}");
    assert!(all_output.contains("beta"), "{all_output}");

    service.kill("alpha").expect("kill alpha");
    other.kill("beta").expect("kill beta");
}

#[test]
fn tag_command_adds_and_removes_opaque_tags() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), Some(VtEngineKind::Passthrough), None, Some("sleep 30".into()), false).expect("create alpha");
    let cli = Cli::try_parse_from(["cleat", "tag", "alpha", "+role=impl", "+task=99", "-role=impl"]).expect("parse tag");

    let output = cli::execute(cli, &service).expect("execute tag").expect("tag output");
    let inspect = service.inspect("alpha").expect("inspect alpha");

    assert_eq!(output, "task=99");
    assert_eq!(inspect.session.tags, vec!["task=99"]);
}

#[test]
fn tag_command_applies_mutations_in_cli_order() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), Some(VtEngineKind::Passthrough), None, Some("sleep 30".into()), false).expect("create alpha");

    let cli = Cli::try_parse_from(["cleat", "tag", "alpha", "-role=impl", "+role=impl"]).expect("parse remove then add");
    let output = cli::execute(cli, &service).expect("execute remove then add").expect("tag output");
    assert_eq!(output, "role=impl");
    assert_eq!(service.inspect("alpha").expect("inspect alpha").session.tags, vec!["role=impl"]);

    let cli = Cli::try_parse_from(["cleat", "tag", "alpha", "+role=impl", "-role=impl"]).expect("parse add then remove");
    let output = cli::execute(cli, &service).expect("execute add then remove");
    assert_eq!(output, None);
    assert!(service.inspect("alpha").expect("inspect alpha").session.tags.is_empty());
}

#[test]
fn directory_subscription_filters_and_emits_lifecycle_deltas() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let selector = "role=impl".to_string();
    service
        .create_with_options(
            Some("alpha".into()),
            Some(VtEngineKind::Passthrough),
            None,
            Some("sleep 30".into()),
            cleat::session::SessionStartOptions {
                record: false,
                initial_size: TerminalSize::default(),
                colors: cleat::vt::TerminalColors::default(),
                tags: vec![selector.clone()],
                environment: Vec::new(),
            },
        )
        .expect("create alpha");
    let mut stream = http_packet_stream_with_selectors(temp.path(), "alpha", std::slice::from_ref(&selector));
    let mut buffer = Vec::new();
    stream.set_read_timeout(Some(Duration::from_secs(2))).expect("set read timeout");
    let hello_frame = PacketFrame::read(&mut stream).expect("read hello");
    let directory_frame = PacketFrame::read(&mut stream).expect("read directory");

    assert_eq!(hello_frame.channel, CHANNEL_CONTROL);
    assert_eq!(hello_frame.msg_type, MSG_CONTROL_HELLO);
    assert_eq!(directory_frame.channel, CHANNEL_CONTROL);
    assert_eq!(directory_frame.msg_type, MSG_CONTROL_DIRECTORY_SNAPSHOT);
    let snapshot = directory_frame.decode::<DirectorySnapshot>().expect("decode directory");
    assert_eq!(snapshot.sessions.iter().map(|entry| entry.session_id.as_str()).collect::<Vec<_>>(), vec!["alpha"]);

    service
        .create_with_options(
            Some("gamma".into()),
            Some(VtEngineKind::Passthrough),
            None,
            Some("sleep 30".into()),
            cleat::session::SessionStartOptions {
                record: false,
                initial_size: TerminalSize::default(),
                colors: cleat::vt::TerminalColors::default(),
                tags: vec![selector.clone()],
                environment: Vec::new(),
            },
        )
        .expect("create gamma");
    let delta = read_directory_delta_named(&mut stream, &mut buffer, Duration::from_secs(30), "gamma create delta");
    assert_eq!(delta.removed_session_ids, Vec::<String>::new());
    assert_eq!(delta.upserted.iter().map(|entry| entry.session_id.as_str()).collect::<Vec<_>>(), vec!["gamma"]);

    service.create(Some("beta".into()), Some(VtEngineKind::Passthrough), None, Some("sleep 30".into()), false).expect("create beta");
    service.update_tags("beta", vec![selector.clone()], Vec::new()).expect("tag beta");
    let delta = read_directory_delta_named(&mut stream, &mut buffer, Duration::from_secs(30), "beta retag delta");
    assert_eq!(delta.upserted.iter().map(|entry| entry.session_id.as_str()).collect::<Vec<_>>(), vec!["beta"]);

    let (_info, attach) = service.attach(Some("beta".into()), None, None, None, true, AttachOptions::default()).expect("attach beta");
    let delta = read_directory_delta_named(&mut stream, &mut buffer, Duration::from_secs(30), "beta attach delta");
    assert_eq!(delta.upserted[0].session_id, "beta");
    assert_eq!(delta.upserted[0].controller_count, 1);
    drop(attach);

    service
        .create_with_options(
            Some("short".into()),
            Some(VtEngineKind::Passthrough),
            None,
            Some("true".into()),
            cleat::session::SessionStartOptions {
                record: false,
                initial_size: TerminalSize::default(),
                colors: cleat::vt::TerminalColors::default(),
                tags: vec![selector.clone()],
                environment: Vec::new(),
            },
        )
        .expect("create short");
    let delta = read_until_directory_remove(&mut stream, &mut buffer, "short", Duration::from_secs(30));
    assert_eq!(delta.removed_session_ids, vec!["short"]);

    service.update_tags("alpha", Vec::new(), vec![selector]).expect("untag alpha");
    let delta = read_until_directory_remove(&mut stream, &mut buffer, "alpha", Duration::from_secs(30));
    assert_eq!(delta.removed_session_ids, vec!["alpha"]);
}

#[test]
fn activity_subscription_snapshot_covers_all_matching_sessions() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let selector = "role=impl".to_string();
    for (id, tags) in [
        ("alpha", vec![selector.clone(), "vessel=one".to_string()]),
        ("beta", vec![selector.clone(), "vessel=two".to_string()]),
        ("gamma", vec!["role=review".to_string()]),
    ] {
        service
            .create_with_options(Some(id.into()), Some(VtEngineKind::Passthrough), None, Some("sleep 30".into()), SessionStartOptions {
                record: false,
                initial_size: TerminalSize::default(),
                colors: cleat::vt::TerminalColors::default(),
                tags,
                environment: Vec::new(),
            })
            .expect("create session");
    }

    let mut stream = http_activity_stream(temp.path(), "alpha", std::slice::from_ref(&selector), Duration::from_millis(50));
    let hello = PacketFrame::read(&mut stream).expect("read hello");
    assert_eq!(hello.msg_type, MSG_CONTROL_HELLO);
    let directory = PacketFrame::read(&mut stream).expect("read directory");
    assert_eq!(directory.msg_type, MSG_CONTROL_DIRECTORY_SNAPSHOT);
    let snapshot = PacketFrame::read(&mut stream).expect("read activity snapshot");
    assert_eq!(snapshot.channel, CHANNEL_CONTROL);
    assert_eq!(snapshot.msg_type, MSG_CONTROL_ACTIVITY_SNAPSHOT);
    let snapshot = snapshot.decode::<ActivitySnapshot>().expect("decode activity snapshot");

    assert_eq!(snapshot.stable_threshold_ms, 50);
    assert_eq!(snapshot.sessions.iter().map(|session| session.session_id.as_str()).collect::<Vec<_>>(), vec!["alpha", "beta"]);
    assert_eq!(snapshot.sessions[0].tags, vec!["role=impl", "vessel=one"]);
    assert_eq!(snapshot.sessions[1].tags, vec!["role=impl", "vessel=two"]);

    let (_client, snapshot) =
        service.connect_activity(std::slice::from_ref(&selector), Duration::from_millis(50)).expect("connect through activity client API");
    assert_eq!(snapshot.sessions.iter().map(|session| session.session_id.as_str()).collect::<Vec<_>>(), vec!["alpha", "beta"]);
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn activity_subscription_emits_threshold_transitions() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let selector = "vessel=work".to_string();
    service
        .create_with_options(
            Some("alpha".into()),
            Some(VtEngineKind::Ghostty),
            None,
            Some("sh -c 'stty raw; exec cat'".into()),
            SessionStartOptions {
                record: false,
                initial_size: TerminalSize::default(),
                colors: cleat::vt::TerminalColors::default(),
                tags: vec![selector.clone()],
                environment: Vec::new(),
            },
        )
        .expect("create alpha");

    let mut stream = http_activity_stream(temp.path(), "alpha", std::slice::from_ref(&selector), Duration::from_millis(500));
    let _hello = PacketFrame::read(&mut stream).expect("read hello");
    let _directory = PacketFrame::read(&mut stream).expect("read directory");
    let snapshot =
        PacketFrame::read(&mut stream).expect("read activity snapshot").decode::<ActivitySnapshot>().expect("decode activity snapshot");
    assert_eq!(snapshot.sessions[0].activity, ScreenActivity::Stable);
    assert_eq!(snapshot.sessions[0].last_output_at_unix_ms, None);

    service.send_keys("alpha", b"screen changed").expect("write output-producing input");
    let active = read_activity_event(&mut stream, Duration::from_secs(2));
    let ActivityEvent::ActivityChanged { session: active, changed_at_unix_ms: active_at } = active else {
        panic!("expected activity change");
    };
    assert_eq!(active.activity, ScreenActivity::Active);
    assert_eq!(active.tags, vec![selector]);
    assert!(active.last_output_at_unix_ms.is_some());
    assert_eq!(active_at, active.last_output_at_unix_ms.expect("active transition output timestamp"));

    let stable_again = read_activity_event(&mut stream, Duration::from_secs(2));
    let ActivityEvent::ActivityChanged { session: stable_again, changed_at_unix_ms: stable_again_at } = stable_again else {
        panic!("expected activity change");
    };
    assert_eq!(stable_again.activity, ScreenActivity::Stable);
    assert_eq!(stable_again_at, stable_again.stable_since_unix_ms + 500);
    assert!(stable_again_at >= active_at);
}

#[test]
fn activity_subscription_rejects_zero_stability_threshold() {
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());

    let result = service.connect_activity(&[], Duration::ZERO);

    assert!(result.is_err());
    assert!(result.err().expect("zero threshold error").contains("greater than zero"));
}

#[test]
fn activity_subscription_emits_membership_deltas_and_reconnects_with_a_fresh_snapshot() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let selector = "vessel=work".to_string();
    for (id, tags) in [("alpha", vec![selector.clone()]), ("beta", Vec::new())] {
        service
            .create_with_options(Some(id.into()), Some(VtEngineKind::Passthrough), None, Some("sleep 30".into()), SessionStartOptions {
                record: false,
                initial_size: TerminalSize::default(),
                colors: cleat::vt::TerminalColors::default(),
                tags,
                environment: Vec::new(),
            })
            .expect("create session");
    }

    let stable_threshold = Duration::from_secs(60);
    let mut stream = http_activity_stream(temp.path(), "alpha", std::slice::from_ref(&selector), stable_threshold);
    let _hello = PacketFrame::read(&mut stream).expect("read hello");
    let _directory = PacketFrame::read(&mut stream).expect("read directory");
    let initial =
        PacketFrame::read(&mut stream).expect("read activity snapshot").decode::<ActivitySnapshot>().expect("decode activity snapshot");
    assert_eq!(initial.sessions.iter().map(|session| session.session_id.as_str()).collect::<Vec<_>>(), vec!["alpha"]);

    service
        .create_with_options(Some("gamma".into()), Some(VtEngineKind::Passthrough), None, Some("sleep 30".into()), SessionStartOptions {
            record: false,
            initial_size: TerminalSize::default(),
            colors: cleat::vt::TerminalColors::default(),
            tags: vec![selector.clone()],
            environment: Vec::new(),
        })
        .expect("create gamma");
    let ActivityEvent::MembershipAdded { session: added, .. } = read_activity_event(&mut stream, Duration::from_secs(2)) else {
        panic!("expected membership add");
    };
    assert_eq!(added.session_id, "gamma");
    assert_eq!(added.tags, vec![selector.clone()]);

    service.update_tags("beta", vec![selector.clone()], Vec::new()).expect("tag beta into selector");
    let ActivityEvent::MembershipAdded { session: added, .. } = read_activity_event(&mut stream, Duration::from_secs(2)) else {
        panic!("expected membership add");
    };
    assert_eq!(added.session_id, "beta");

    service.update_tags("alpha", Vec::new(), vec![selector.clone()]).expect("tag alpha out of selector");
    let ActivityEvent::MembershipRemoved { session_id, tags, .. } = read_activity_event(&mut stream, Duration::from_secs(2)) else {
        panic!("expected membership remove");
    };
    assert_eq!(session_id, "alpha");
    assert_eq!(tags, vec![selector.clone()]);

    service.kill("gamma").expect("kill gamma");
    let ActivityEvent::MembershipRemoved { session_id, .. } = read_activity_event(&mut stream, Duration::from_secs(2)) else {
        panic!("expected membership remove");
    };
    assert_eq!(session_id, "gamma");
    drop(stream);

    let mut reconnected = http_activity_stream(temp.path(), "beta", std::slice::from_ref(&selector), stable_threshold);
    let _hello = PacketFrame::read(&mut reconnected).expect("read reconnect hello");
    let _directory = PacketFrame::read(&mut reconnected).expect("read reconnect directory");
    let snapshot = PacketFrame::read(&mut reconnected)
        .expect("read reconnect activity snapshot")
        .decode::<ActivitySnapshot>()
        .expect("decode reconnect activity snapshot");
    assert_eq!(snapshot.sessions.iter().map(|session| session.session_id.as_str()).collect::<Vec<_>>(), vec!["beta"]);
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

    let invalid_daemon_instance = http_session_request(
        temp.path(),
        "alpha",
        "POST /sessions HTTP/1.1\r\nHost: cleat\r\nx-cleat-daemon-pid: invalid\r\nContent-Length: 0\r\n\r\n",
    );
    assert!(invalid_daemon_instance.starts_with("HTTP/1.1 400 Bad Request\r\n"), "{invalid_daemon_instance}");
    assert!(http_body(&invalid_daemon_instance).contains("invalid daemon instance header"));

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

    let paste_with_mark_body = r#"{"text":"structured paste","marker_name":"m2"}"#;
    let paste_with_mark = http_session_request(
        temp.path(),
        "alpha",
        &format!(
            "POST /sessions/alpha/paste-with-mark HTTP/1.1\r\nHost: cleat\r\nContent-Length: {}\r\n\r\n{}",
            paste_with_mark_body.len(),
            paste_with_mark_body
        ),
    );
    assert!(paste_with_mark.starts_with("HTTP/1.1 200 OK\r\n"), "{paste_with_mark}");
    let paste_with_mark_json: serde_json::Value = serde_json::from_str(http_body(&paste_with_mark)).expect("paste-with-mark json");
    assert!(paste_with_mark_json["offset"].is_u64());

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
            ..CleatProviderDesc::default()
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
            ..CleatProviderDesc::default()
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

// Drives the daemon FFI surface end to end through the C ABI functions:
// session identity, role grant, directory snapshot, attach-by-id,
// take-control preemption, and closed-channel notification when the session
// exits out from under attached clients.
#[cfg(feature = "ghostty-vt")]
#[test]
fn daemon_provider_ffi_attach_roles_directory_and_close() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::Builder::new().prefix("cleat-provider-").tempdir_in("/tmp").expect("tempdir");
    let root = temp.path().to_string_lossy();
    let command = b"cat";
    let id = b"ffi-alpha";

    unsafe {
        let provider = cleat_provider_open(&CleatProviderDesc {
            abi_version: CLEAT_PROVIDER_ABI_VERSION,
            requested_features: ProviderFeatures::CELL_SNAPSHOTS.bits(),
            backend: CLEAT_PROVIDER_BACKEND_DAEMON,
            runtime_root: root.as_ptr(),
            runtime_root_len: root.len(),
            ..CleatProviderDesc::default()
        });
        assert!(!provider.is_null());

        let controller = cleat_session_create(provider, &CleatSessionDesc {
            cols: 80,
            rows: 24,
            vt_engine: CLEAT_PROVIDER_VT_GHOSTTY,
            command: command.as_ptr(),
            command_len: command.len(),
            id: id.as_ptr(),
            id_len: id.len(),
            ..CleatSessionDesc::default()
        });
        assert!(!controller.is_null());

        let mut id_out = CleatStr::default();
        assert!(cleat_session_id(controller, &mut id_out));
        assert_eq!(std::slice::from_raw_parts(id_out.ptr, id_out.len), id);

        wait_until("controller role grant", || cleat_session_role(controller) == CLEAT_ROLE_CONTROLLER);
        wait_until("streaming connection state", || cleat_session_connection_state(controller) == CLEAT_SESSION_STREAMING);
        wait_until("directory generation bump", || cleat_provider_directory_generation(provider) > 0);
        wait_until("directory lists the session with a controller", || {
            let mut directory = CleatDirectory::default();
            if !cleat_provider_directory_snapshot(provider, &mut directory) {
                return false;
            }
            let entries = std::slice::from_raw_parts(directory.entries, directory.entry_count);
            let found = entries
                .iter()
                .any(|entry| std::slice::from_raw_parts(entry.session_id.ptr, entry.session_id.len) == id && entry.controller_count >= 1);
            cleat_provider_directory_release(provider, &mut directory);
            found
        });

        let watcher = cleat_session_attach(provider, &CleatSessionDesc {
            cols: 80,
            rows: 24,
            id: id.as_ptr(),
            id_len: id.len(),
            role: CLEAT_ROLE_WATCHER,
            ..CleatSessionDesc::default()
        });
        assert!(!watcher.is_null());
        wait_until("watcher role grant", || cleat_session_role(watcher) == CLEAT_ROLE_WATCHER);

        assert!(cleat_session_take_control(watcher));
        wait_until("take-control grant", || cleat_session_role(watcher) == CLEAT_ROLE_CONTROLLER);
        wait_until("preempted controller demoted", || cleat_session_role(controller) == CLEAT_ROLE_WATCHER);

        // Kill the session out of band: both attachments must observe the
        // channel close rather than reporting stale STREAMING forever.
        service_for(temp.path()).kill("ffi-alpha").expect("kill session");
        wait_until("controller observes close", || cleat_session_connection_state(controller) == CLEAT_SESSION_CLOSED);
        wait_until("watcher observes close", || cleat_session_connection_state(watcher) == CLEAT_SESSION_CLOSED);

        cleat_session_destroy(watcher);
        cleat_session_destroy(controller);
        cleat_provider_close(provider);
    }
}

// A session whose VT engine cannot serve render state (passthrough is a
// placeholder engine) must fail its packet channel with a channel-scoped
// error, not tear down the daemon that hosts every other session.
#[test]
fn daemon_provider_passthrough_channel_failure_is_contained() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::Builder::new().prefix("cleat-provider-").tempdir_in("/tmp").expect("tempdir");
    let root = temp.path().to_string_lossy();
    let command = b"cat";
    let id = b"ffi-passthrough";

    unsafe {
        let provider = cleat_provider_open(&CleatProviderDesc {
            abi_version: CLEAT_PROVIDER_ABI_VERSION,
            requested_features: ProviderFeatures::CELL_SNAPSHOTS.bits(),
            backend: CLEAT_PROVIDER_BACKEND_DAEMON,
            runtime_root: root.as_ptr(),
            runtime_root_len: root.len(),
            ..CleatProviderDesc::default()
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

        let mut id_out = CleatStr::default();
        assert!(cleat_session_id(session, &mut id_out));
        assert_eq!(std::slice::from_raw_parts(id_out.ptr, id_out.len), id);

        // The channel open fails daemon-side and surfaces as CLOSED (not
        // DISCONNECTED, which would mean the daemon itself went down).
        wait_until("channel failure surfaces as closed", || cleat_session_connection_state(session) == CLEAT_SESSION_CLOSED);

        // The daemon survived: the directory subscription still answers and
        // lists the session.
        wait_until("directory generation bump", || cleat_provider_directory_generation(provider) > 0);
        wait_until("directory lists the session", || {
            let mut directory = CleatDirectory::default();
            if !cleat_provider_directory_snapshot(provider, &mut directory) {
                return false;
            }
            let entries = std::slice::from_raw_parts(directory.entries, directory.entry_count);
            let found = entries.iter().any(|entry| std::slice::from_raw_parts(entry.session_id.ptr, entry.session_id.len) == id);
            cleat_provider_directory_release(provider, &mut directory);
            found
        });

        cleat_session_destroy(session);
        cleat_provider_close(provider);
    }

    service_for(temp.path()).kill("ffi-passthrough").expect("kill session via a live daemon");
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
    let layout = RuntimeLayout::new(root.to_path_buf());
    layout.ensure_daemon_dirs().expect("create daemon dirs");
    let dir = layout.session_dir(id);
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

#[test]
fn packet_connect_emits_hello_and_directory_snapshot() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), Some(VtEngineKind::Passthrough), None, Some("sleep 30".into()), false).expect("create alpha");
    service.create(Some("beta".into()), Some(VtEngineKind::Passthrough), None, Some("sleep 30".into()), false).expect("create beta");

    let mut stream = http_packet_stream(temp.path(), "alpha");
    stream.set_read_timeout(Some(Duration::from_secs(2))).expect("set read timeout");

    let hello_frame = PacketFrame::read(&mut stream).expect("read hello");
    let directory_frame = PacketFrame::read(&mut stream).expect("read directory");

    assert_eq!(hello_frame.channel, CHANNEL_CONTROL);
    assert_eq!(hello_frame.msg_type, MSG_CONTROL_HELLO);
    assert_eq!(hello_frame.decode::<ControlHello>().expect("decode hello").version, PROTOCOL_VERSION);
    assert_eq!(directory_frame.channel, CHANNEL_CONTROL);
    assert_eq!(directory_frame.msg_type, MSG_CONTROL_DIRECTORY_SNAPSHOT);
    assert_eq!(directory_frame.decode::<DirectorySnapshot>().expect("decode directory").sessions, vec![
        cleat::packet::DirectoryEntry {
            session_id: "alpha".to_string(),
            tags: Vec::new(),
            state: "running".to_string(),
            controller_count: 0,
            watcher_count: 0,
            controller: None,
            recreatable: false,
            cols: 80,
            rows: 24,
        },
        cleat::packet::DirectoryEntry {
            session_id: "beta".to_string(),
            tags: Vec::new(),
            state: "running".to_string(),
            controller_count: 0,
            watcher_count: 0,
            controller: None,
            recreatable: false,
            cols: 80,
            rows: 24,
        },
    ]);
}

#[test]
fn contained_session_panic_does_not_stop_sibling_session() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _panic = EnvVarGuard::set("CLEAT_TEST_PANIC_SESSION_TICK", "alpha");
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());

    service.create(Some("alpha".into()), Some(VtEngineKind::Passthrough), None, Some("sleep 30".into()), false).expect("create alpha");
    service.create(Some("beta".into()), Some(VtEngineKind::Passthrough), None, Some("sleep 30".into()), false).expect("create beta");

    let deadline = Instant::now() + Duration::from_secs(2);
    while service.inspect("alpha").is_ok() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }

    assert!(service.inspect("alpha").is_err(), "faulted session should be removed");
    assert_eq!(service.inspect("beta").expect("sibling session should remain hosted").session.id, "beta");
    let listed = service.list().expect("list after contained panic");
    assert_eq!(listed.iter().map(|session| session.id.as_str()).collect::<Vec<_>>(), vec!["beta"]);
    service.kill("beta").expect("kill sibling session");
}

// An actor worker thread that dies without recording an exit (VT engine
// panic, readiness-poll failure) must fault its one session — via the failed
// actor request or the worker_finished backstop — and leave the daemon
// serving.
#[test]
fn actor_worker_death_faults_the_session_and_daemon_survives() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _panic = EnvVarGuard::set("CLEAT_TEST_PANIC_ACTOR", "doomed");
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());

    // Silent command: creation completes cleanly (the hook only fires on
    // pumps that read output). PTY echo of the sent keys then arms it.
    service.create(Some("doomed".into()), Some(VtEngineKind::Passthrough), None, Some("sleep 30".into()), false).expect("create doomed");
    service.send_keys("doomed", b"boom\n").expect("send keys to doomed");

    let deadline = Instant::now() + Duration::from_secs(3);
    while service.inspect("doomed").is_ok() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(service.inspect("doomed").is_err(), "worker-dead session should be faulted, not wedge the daemon");

    service
        .create(Some("survivor".into()), Some(VtEngineKind::Passthrough), None, Some("sleep 30".into()), false)
        .expect("daemon should still serve after a worker death");
    service.kill("survivor").expect("kill survivor");
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn packet_channel_initial_render_and_packet_input_flow() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service
        .create(Some("alpha".into()), Some(VtEngineKind::Ghostty), None, Some("sh -c 'stty raw; exec cat'".into()), false)
        .expect("create alpha");

    let mut stream = http_packet_stream(temp.path(), "alpha");
    let mut buffer = Vec::new();
    let _hello = PacketFrame::read(&mut stream).expect("read hello");
    let _directory = PacketFrame::read(&mut stream).expect("read directory");

    packet_open_channel(&mut stream, 1, "alpha");
    let initial = read_packet_render(&mut stream, &mut buffer, 1, Duration::from_secs(2));
    assert_eq!(initial.dirty, cleat::provider::DirtyState::Full);
    assert!(!initial.ops.is_empty());
    packet_ack(&mut stream, 1, initial.render_generation);

    std::thread::sleep(Duration::from_millis(500));
    packet_input(&mut stream, 1, TerminalInputEvent::Paste(TerminalPasteEvent { text: "packet paste".to_string() }));
    let update = read_packet_render(&mut stream, &mut buffer, 1, Duration::from_secs(2));
    assert!(update.render_generation > initial.render_generation);
    assert!(!update.ops.is_empty());
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn packet_roles_gate_input_and_take_control_demotes() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service
        .create(Some("alpha".into()), Some(VtEngineKind::Ghostty), None, Some("sh -c 'stty raw; exec cat'".into()), false)
        .expect("create alpha");

    // first client requests and receives control
    let mut controller_stream = http_packet_stream(temp.path(), "alpha");
    let _hello = PacketFrame::read(&mut controller_stream).expect("read hello");
    let _directory = PacketFrame::read(&mut controller_stream).expect("read directory");
    packet_open_channel_role(&mut controller_stream, 1, "alpha", ChannelRole::Controller, false);
    let mut controller = PacketReader::new(&mut controller_stream);
    assert_eq!(controller.role(1, Duration::from_secs(2)), ChannelRole::Controller);
    let initial = controller.render(1, Duration::from_secs(2));
    packet_write(controller.stream, 1, MSG_SESSION_ACK, &Ack { generation: initial.render_generation });
    std::thread::sleep(Duration::from_millis(500));

    // second client also asks for control without take: granted watcher
    let mut watcher_stream = http_packet_stream(temp.path(), "alpha");
    let _hello = PacketFrame::read(&mut watcher_stream).expect("read hello");
    let _directory = PacketFrame::read(&mut watcher_stream).expect("read directory");
    packet_open_channel_role(&mut watcher_stream, 1, "alpha", ChannelRole::Controller, false);
    let mut watcher = PacketReader::new(&mut watcher_stream);
    let denied = watcher.role_state(1, Duration::from_secs(2));
    assert_eq!(denied.role, ChannelRole::Watcher);
    assert_eq!(denied.denial_reason, Some(cleat::packet::RoleDenialReason { held_by: cleat::packet::ControllerHolder::Packet }));
    let watcher_initial = watcher.render(1, Duration::from_secs(2));
    packet_write(watcher.stream, 1, MSG_SESSION_ACK, &Ack { generation: watcher_initial.render_generation });

    // Let late startup output (the `stty raw` mode change) settle before the
    // negative assertion below.
    watcher.settle_renders(1, Duration::from_millis(300), Duration::from_secs(2));

    // watcher input is dropped: the raw-mode cat would echo it back as output
    packet_write(watcher.stream, 1, MSG_SESSION_INPUT, &Input {
        event: TerminalInputEvent::Paste(TerminalPasteEvent { text: "blocked".to_string() }),
    });
    watcher.expect_no_render(1, Duration::from_millis(700));

    // a fresh subscriber sees both attachments in the directory counts
    {
        let mut observer = http_packet_stream(temp.path(), "alpha");
        let _hello = PacketFrame::read(&mut observer).expect("read hello");
        let directory = PacketFrame::read(&mut observer).expect("read directory");
        let directory = directory.decode::<DirectorySnapshot>().expect("decode directory snapshot");
        let alpha = directory.sessions.iter().find(|entry| entry.session_id == "alpha").expect("alpha in directory");
        assert_eq!((alpha.controller_count, alpha.watcher_count), (1, 1));
    }

    // take-control: the requester is granted controller, the old controller
    // is demoted and told so
    packet_write(watcher.stream, 1, MSG_SESSION_ROLE, &RoleRequest { role: ChannelRole::Controller, take: true });
    assert_eq!(watcher.role(1, Duration::from_secs(2)), ChannelRole::Controller);
    assert_eq!(controller.role(1, Duration::from_secs(2)), ChannelRole::Watcher);

    // input from the new controller flows
    packet_write(watcher.stream, 1, MSG_SESSION_INPUT, &Input {
        event: TerminalInputEvent::Paste(TerminalPasteEvent { text: "allowed".to_string() }),
    });
    let update = watcher.render(1, Duration::from_secs(2));
    assert!(update.render_generation > watcher_initial.render_generation);

    // ...and input from the demoted client no longer does. Settle first: the
    // "allowed" echo may split across renders, and its tail would race into
    // the no-render window.
    packet_write(watcher.stream, 1, MSG_SESSION_ACK, &Ack { generation: update.render_generation });
    watcher.settle_renders(1, Duration::from_millis(300), Duration::from_secs(2));
    packet_write(controller.stream, 1, MSG_SESSION_INPUT, &Input {
        event: TerminalInputEvent::Paste(TerminalPasteEvent { text: "stale".to_string() }),
    });
    watcher.expect_no_render(1, Duration::from_millis(700));
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn packet_role_denial_identifies_legacy_stream_holder_and_inspect_exposes_it() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service
        .create(Some("alpha".into()), Some(VtEngineKind::Ghostty), None, Some("sh -c 'stty raw; exec cat'".into()), false)
        .expect("create alpha");

    let legacy_identity = AttachmentIdentity { kind: AttachmentKind::Principal, name: "legacy attach".to_string() };
    let (_legacy, response) = http_attach_with_seat_options(temp.path(), "alpha", legacy_identity, false, false);
    assert!(response.starts_with("HTTP/1.1 101 Switching Protocols\r\n"), "{response}");

    let mut stream = http_packet_stream(temp.path(), "alpha");
    let _hello = PacketFrame::read(&mut stream).expect("read hello");
    let _directory = PacketFrame::read(&mut stream).expect("read directory");
    packet_open_channel_role(&mut stream, 1, "alpha", ChannelRole::Controller, false);
    let mut reader = PacketReader::new(&mut stream);
    let denied = reader.role_state(1, Duration::from_secs(2));
    assert_eq!(denied.role, ChannelRole::Watcher);
    assert_eq!(denied.denial_reason, Some(cleat::packet::RoleDenialReason { held_by: cleat::packet::ControllerHolder::Stream }));

    let inspected = service.inspect("alpha").expect("inspect denied packet attachment");
    let watcher = inspected.attachments.iter().find(|attachment| attachment.role == "watcher").expect("watcher attachment");
    assert_eq!(watcher.denial_reason, denied.denial_reason);
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn interactive_packet_attach_is_demoted_by_take_without_losing_recording_state() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service
        .create(Some("alpha".into()), Some(VtEngineKind::Ghostty), None, Some("sh -c 'stty raw; exec cat'".into()), true)
        .expect("create alpha");

    let first_identity = AttachmentIdentity { kind: AttachmentKind::Principal, name: "first attach".to_string() };
    let taker_identity = AttachmentIdentity { kind: AttachmentKind::Supervisor, name: "governor".to_string() };
    let (_session, _first) = service
        .attach(Some("alpha".into()), None, None, None, false, AttachOptions {
            identity: first_identity.clone(),
            strict: false,
            take: false,
        })
        .expect("first interactive packet attach");
    let (_session, _taker) = service
        .attach(Some("alpha".into()), None, None, None, false, AttachOptions {
            identity: taker_identity.clone(),
            strict: false,
            take: true,
        })
        .expect("take interactive packet controller");

    let inspected = service.inspect("alpha").expect("inspect after interactive transfer");
    assert!(inspected.recording.active);
    assert_eq!(inspected.session.state, "running");
    assert!(inspected.attachments.iter().any(|attachment| {
        attachment.role == "controller" && attachment.identity == taker_identity && attachment.denial_reason.is_none()
    }));
    assert!(inspected.attachments.iter().any(|attachment| {
        attachment.role == "watcher"
            && attachment.identity == first_identity
            && attachment.denial_reason == Some(cleat::packet::RoleDenialReason { held_by: cleat::packet::ControllerHolder::Packet })
    }));
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn packet_mode_only_change_produces_zero_row_ops() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service
        .create(
            Some("alpha".into()),
            Some(VtEngineKind::Ghostty),
            None,
            Some("sh -c 'sleep 1; printf \"\\033[?1003h\"; sleep 30'".into()),
            false,
        )
        .expect("create alpha");

    let mut stream = http_packet_stream(temp.path(), "alpha");
    let mut buffer = Vec::new();
    let _hello = PacketFrame::read(&mut stream).expect("read hello");
    let _directory = PacketFrame::read(&mut stream).expect("read directory");
    packet_open_channel(&mut stream, 1, "alpha");
    let initial = read_packet_render(&mut stream, &mut buffer, 1, Duration::from_secs(2));
    packet_ack(&mut stream, 1, initial.render_generation);

    let update = read_packet_render(&mut stream, &mut buffer, 1, Duration::from_secs(2));

    assert!(update.terminal_modes.mouse_tracking);
    assert_eq!(update.terminal_modes.mouse_tracking_mode, vt::MouseTrackingMode::Any);
    assert!(update.ops.is_empty(), "mode-only packet should not carry row ops: {:?}", update.ops);
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn packet_render_ack_enforces_one_in_flight_and_coalesces_slow_clients() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service
        .create(Some("alpha".into()), Some(VtEngineKind::Ghostty), None, Some("sh -c 'stty raw; exec cat'".into()), false)
        .expect("create alpha");

    let mut stream = http_packet_stream(temp.path(), "alpha");
    let mut buffer = Vec::new();
    let _hello = PacketFrame::read(&mut stream).expect("read hello");
    let _directory = PacketFrame::read(&mut stream).expect("read directory");
    packet_open_channel(&mut stream, 1, "alpha");
    let initial = read_packet_render(&mut stream, &mut buffer, 1, Duration::from_secs(2));
    packet_ack(&mut stream, 1, initial.render_generation);

    std::thread::sleep(Duration::from_millis(500));
    packet_input(&mut stream, 1, TerminalInputEvent::Text(TerminalTextEvent { text: "a".to_string() }));
    let first = read_packet_render(&mut stream, &mut buffer, 1, Duration::from_secs(2));
    packet_ack(&mut stream, 1, initial.render_generation);
    packet_input(&mut stream, 1, TerminalInputEvent::Text(TerminalTextEvent { text: "b".to_string() }));
    expect_no_render(&mut stream, &mut buffer, Duration::from_millis(120));

    packet_ack(&mut stream, 1, first.render_generation);
    let coalesced = read_packet_render(&mut stream, &mut buffer, 1, Duration::from_secs(2));
    assert!(coalesced.render_generation > first.render_generation);
    packet_ack(&mut stream, 1, coalesced.render_generation);
    expect_no_render(&mut stream, &mut buffer, Duration::from_millis(120));
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn packet_concurrent_channels_each_progress_past_initial_generation() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service
        .create(Some("alpha".into()), Some(VtEngineKind::Ghostty), None, Some("sh -c 'stty raw; exec cat'".into()), false)
        .expect("create alpha");

    let mut first_stream = http_packet_stream(temp.path(), "alpha");
    let mut first_buffer = Vec::new();
    let _hello = PacketFrame::read(&mut first_stream).expect("read first hello");
    let _directory = PacketFrame::read(&mut first_stream).expect("read first directory");
    packet_open_channel(&mut first_stream, 1, "alpha");
    let first_initial = read_packet_render(&mut first_stream, &mut first_buffer, 1, Duration::from_secs(2));
    packet_ack(&mut first_stream, 1, first_initial.render_generation);

    let mut second_stream = http_packet_stream(temp.path(), "alpha");
    let mut second_buffer = Vec::new();
    let _hello = PacketFrame::read(&mut second_stream).expect("read second hello");
    let _directory = PacketFrame::read(&mut second_stream).expect("read second directory");
    packet_open_channel(&mut second_stream, 1, "alpha");
    let second_initial = read_packet_render(&mut second_stream, &mut second_buffer, 1, Duration::from_secs(2));
    packet_ack(&mut second_stream, 1, second_initial.render_generation);

    std::thread::sleep(Duration::from_millis(500));
    packet_input(&mut first_stream, 1, TerminalInputEvent::Text(TerminalTextEvent { text: "x".to_string() }));
    let first_update = read_packet_render(&mut first_stream, &mut first_buffer, 1, Duration::from_secs(2));
    let second_update = read_packet_render(&mut second_stream, &mut second_buffer, 1, Duration::from_secs(2));

    // Not exact equality: the session is live, so an extra pump (echo split
    // across reads, terminal-mode sync) can advance the generation between the
    // two reads. Each viewer must have progressed past its initial render.
    assert!(first_update.render_generation > first_initial.render_generation, "first viewer should progress past its initial generation");
    assert!(
        second_update.render_generation > second_initial.render_generation,
        "second viewer should progress past its initial generation"
    );
    assert!(!first_update.ops.is_empty(), "first viewer should receive row content");
    assert!(!second_update.ops.is_empty(), "second viewer should receive row content");
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn packet_lagging_channel_receives_cached_generation_after_session_goes_clean() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service
        .create(Some("alpha".into()), Some(VtEngineKind::Ghostty), None, Some("sh -c 'stty raw; exec cat'".into()), false)
        .expect("create alpha");

    let mut fast_stream = http_packet_stream(temp.path(), "alpha");
    let mut fast_buffer = Vec::new();
    let _hello = PacketFrame::read(&mut fast_stream).expect("read fast hello");
    let _directory = PacketFrame::read(&mut fast_stream).expect("read fast directory");
    packet_open_channel(&mut fast_stream, 1, "alpha");
    let fast_initial = read_packet_render(&mut fast_stream, &mut fast_buffer, 1, Duration::from_secs(2));
    packet_ack(&mut fast_stream, 1, fast_initial.render_generation);

    let mut slow_stream = http_packet_stream(temp.path(), "alpha");
    let mut slow_buffer = Vec::new();
    let _hello = PacketFrame::read(&mut slow_stream).expect("read slow hello");
    let _directory = PacketFrame::read(&mut slow_stream).expect("read slow directory");
    packet_open_channel(&mut slow_stream, 1, "alpha");
    let slow_initial = read_packet_render(&mut slow_stream, &mut slow_buffer, 1, Duration::from_secs(2));

    std::thread::sleep(Duration::from_millis(500));
    packet_input(&mut fast_stream, 1, TerminalInputEvent::Text(TerminalTextEvent { text: "y".to_string() }));
    let fast_update = loop {
        let update = read_packet_render(&mut fast_stream, &mut fast_buffer, 1, Duration::from_secs(2));
        if !update.ops.is_empty() {
            break update;
        }
        packet_ack(&mut fast_stream, 1, update.render_generation);
    };
    packet_ack(&mut fast_stream, 1, fast_update.render_generation);

    packet_ack(&mut slow_stream, 1, slow_initial.render_generation);
    let slow_update = read_packet_render(&mut slow_stream, &mut slow_buffer, 1, Duration::from_secs(2));

    // Not exact equality: the session is live, so an extra pump between the
    // fast viewer's read and the slow viewer's read can advance the generation
    // (issue #149). The lagging viewer must catch up to at least the
    // generation the fast viewer acked.
    assert!(
        slow_update.render_generation >= fast_update.render_generation,
        "lagging viewer (gen {}) should catch up to the fast viewer's acked generation ({})",
        slow_update.render_generation,
        fast_update.render_generation
    );
    assert!(!slow_update.ops.is_empty(), "lagging viewer should receive cached row content after fast ack");
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn packets_command_prints_render_summaries() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service
        .create(
            Some("alpha".into()),
            Some(VtEngineKind::Ghostty),
            None,
            Some("sh -c 'sleep 1; printf \"\\033[?1003h\"; sleep 30'".into()),
            false,
        )
        .expect("create alpha");
    let cli = Cli::try_parse_from(["cleat", "packets", "alpha", "--count", "2"]).expect("parse packets");

    let output = cli::execute(cli, &service).expect("execute packets").expect("packets output");
    let lines: Vec<_> = output.lines().collect();

    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("gen="), "{output}");
    assert!(lines[0].contains("ops="), "{output}");
    assert!(lines[0].contains("mode_changes=initial"), "{output}");
    assert!(lines[1].contains("mode_changes="), "{output}");
    assert!(lines[1].contains("mouse_tracking=true"), "{output}");
    assert!(lines[1].contains("rows=0"), "{output}");
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
    assert!(!service.session_dir("alpha").exists());
}

#[test]
fn kill_preserves_session_directory_with_recording() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service
        .create(Some("alpha".into()), Some(VtEngineKind::Passthrough), None, Some("sh -c 'printf preserved; sleep 30'".into()), true)
        .expect("create alpha");

    let cast_path = service.session_dir("alpha").join(CAST_FILE_NAME);
    let deadline = Instant::now() + Duration::from_secs(2);
    while !cast_path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(cast_path.exists(), "recording should exist before kill");

    let cli = Cli::try_parse_from(["cleat", "kill", "alpha"]).expect("parse kill");
    cli::execute(cli, &service).expect("execute kill");

    assert!(service.session_dir("alpha").exists(), "recording-bearing session directory should be preserved");
    assert!(cast_path.exists(), "recording should survive kill");
    assert!(session_socket_path(temp.path(), "alpha").exists(), "daemon socket should linger after session-scoped kill");
    assert!(daemon_pid_path(temp.path(), "alpha").exists(), "daemon pid should linger after session-scoped kill");
}

#[test]
fn kill_purge_removes_session_directory_with_recording() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service
        .create(Some("alpha".into()), Some(VtEngineKind::Passthrough), None, Some("sh -c 'printf purged; sleep 30'".into()), true)
        .expect("create alpha");

    let cast_path = service.session_dir("alpha").join(CAST_FILE_NAME);
    let deadline = Instant::now() + Duration::from_secs(2);
    while !cast_path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(cast_path.exists(), "recording should exist before purge");

    let cli = Cli::try_parse_from(["cleat", "kill", "alpha", "--purge"]).expect("parse kill --purge");
    cli::execute(cli, &service).expect("execute kill --purge");

    assert!(!service.session_dir("alpha").exists(), "purge should delete the recording-bearing session directory");
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

    let (first, attach) =
        service.attach(Some("alpha".into()), None, None, Some("sleep 5".into()), false, AttachOptions::default()).expect("first attach");
    assert_eq!(first.id, "alpha");
    assert_eq!(first.vt_engine, vt::default_vt_engine_kind());
    assert!(daemon_pid_path(temp.path(), "alpha").exists());

    drop(attach);

    let (second, _attach2) = service
        .attach(Some("alpha".into()), Some(VtEngineKind::Passthrough), None, None, false, AttachOptions::default())
        .expect("reattach");
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

    let (created, attach) = service
        .attach(Some("alpha".into()), Some(VtEngineKind::Passthrough), None, Some("sleep 5".into()), false, AttachOptions::default())
        .expect("first attach");
    assert_eq!(created.vt_engine, VtEngineKind::Passthrough);
    drop(attach);

    let (reattached, _attach2) = service
        .attach(Some("alpha".into()), Some(vt::default_vt_engine_kind()), None, None, false, AttachOptions::default())
        .expect("reattach");
    assert_eq!(reattached.vt_engine, VtEngineKind::Passthrough);
}

#[test]
fn attach_falls_back_strictly_refuses_and_take_demotes_without_losing_session_state() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), Some(VtEngineKind::Passthrough), None, Some("cat".into()), true).expect("create alpha");

    let alice = AttachmentIdentity { kind: AttachmentKind::Principal, name: "alice".to_string() };
    let bob = AttachmentIdentity { kind: AttachmentKind::Supervisor, name: "bob".to_string() };
    let carol = AttachmentIdentity { kind: AttachmentKind::Tool, name: "carol".to_string() };

    let (mut controller, first_response) = http_attach_with_seat_options(temp.path(), "alpha", alice.clone(), false, false);
    assert!(first_response.starts_with("HTTP/1.1 101 Switching Protocols\r\n"), "{first_response}");

    let (mut strict_stream, strict_response) = http_attach_with_seat_options(temp.path(), "alpha", bob.clone(), true, false);
    assert!(strict_response.starts_with("HTTP/1.1 409 Conflict\r\n"), "{strict_response}");
    let mut strict_body = String::new();
    strict_stream.read_to_string(&mut strict_body).expect("read strict error body");
    assert!(strict_body.contains("alice (principal)"), "{strict_body}");

    let (mut watcher, fallback_response) = http_attach_with_seat_options(temp.path(), "alpha", bob.clone(), false, false);
    assert!(fallback_response.starts_with("HTTP/1.1 101 Switching Protocols\r\n"), "{fallback_response}");
    assert_eq!(read_until_seat_state(&mut watcher, Duration::from_secs(2)), SeatState {
        role: "watcher".to_string(),
        controller: Some(alice.clone())
    });

    let before = service.inspect("alpha").expect("inspect before take");
    assert!(before.recording.active);
    assert_eq!(before.attachments, vec![
        cleat::protocol::AttachmentInspect { role: "controller".to_string(), identity: alice, denial_reason: None },
        cleat::protocol::AttachmentInspect {
            role: "watcher".to_string(),
            identity: bob,
            denial_reason: Some(cleat::packet::RoleDenialReason { held_by: cleat::packet::ControllerHolder::Stream }),
        },
    ]);

    let (_new_controller, take_response) = http_attach_with_seat_options(temp.path(), "alpha", carol.clone(), false, true);
    assert!(take_response.starts_with("HTTP/1.1 101 Switching Protocols\r\n"), "{take_response}");
    assert_eq!(read_until_seat_state(&mut controller, Duration::from_secs(2)), SeatState {
        role: "watcher".to_string(),
        controller: Some(carol.clone())
    });

    let after = service.inspect("alpha").expect("inspect after take");
    assert_eq!(after.session.state, "running");
    assert!(after.recording.active);
    assert_eq!(after.attachments.iter().filter(|attachment| attachment.role == "controller").collect::<Vec<_>>(), vec![
        &cleat::protocol::AttachmentInspect { role: "controller".to_string(), identity: carol.clone(), denial_reason: None }
    ]);
    assert_eq!(service.list().expect("list sessions")[0].controller, Some(carol));
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn attach_strict_rejects_second_foreground_client_while_one_is_active() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());

    let (_session, _attach) = service
        .attach(Some("alpha".into()), None, None, Some("sleep 5".into()), false, AttachOptions {
            identity: AttachmentIdentity { kind: AttachmentKind::Principal, name: "first".to_string() },
            strict: false,
            take: false,
        })
        .expect("first attach");
    let err = service
        .attach(Some("alpha".into()), None, None, None, false, AttachOptions {
            identity: AttachmentIdentity { kind: AttachmentKind::Principal, name: "second".to_string() },
            strict: true,
            take: false,
        })
        .expect_err("second strict attach should fail");

    assert!(err.contains("first (principal)"));
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn lifecycle_attach_init_with_capabilities_is_accepted_with_strict_seat_policy() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());

    service.create(Some("alpha".into()), None, None, Some("sleep 5".into()), false).expect("create alpha");

    let _stream = http_attach_stream(temp.path(), "alpha", 100, 30, ClientCapabilities::new(ColorLevel::Ansi256, true));

    let err = service
        .attach(Some("alpha".into()), None, None, None, false, AttachOptions {
            identity: AttachmentIdentity::default(),
            strict: true,
            take: false,
        })
        .expect_err("second attach should fail");
    assert!(err.contains("controller seat is held by unknown (principal)"));
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

    let cast_path = service.session_dir("alpha").join(cleat::recording::CAST_FILE_NAME);
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

    let seat = Frame::read(&mut stream).expect("read watcher seat state");
    assert_eq!(seat, Frame::SeatState(SeatState { role: "watcher".to_string(), controller: None }));
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
fn dribbling_http_handshake_does_not_block_attached_output() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let cmd = r#"i=0; while :; do printf 'dribble-tick-%04d\n' "$i"; i=$((i+1)); sleep 0.05; done"#;
    service.create(Some("alpha".into()), Some(VtEngineKind::Passthrough), None, Some(cmd.into()), false).expect("create alpha");

    let mut controller = http_attach_stream(temp.path(), "alpha", 100, 30, ClientCapabilities::conservative_fallback());
    let initial_output = collect_output_until(&mut controller, "dribble-tick-", Duration::from_secs(2));
    assert!(initial_output.contains("dribble-tick-"), "initial output was {initial_output:?}");
    drain_available_output(&mut controller);

    let socket_path = session_socket_path(temp.path(), "alpha");
    let dribbler = std::thread::spawn(move || {
        let mut stream = UnixStream::connect(socket_path).expect("connect dribbler");
        let request = b"POST /sessions/alpha/inspect HTTP/1.1\r\nHost: cleat\r\nContent-Length: 0\r\n\r\n";
        for (index, byte) in request.iter().enumerate() {
            if let Err(err) = stream.write_all(&[*byte]) {
                assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe, "write dribbled byte: {err}");
                return;
            }
            if index >= 4 {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    });

    let output_while_dribbling = collect_output_until(&mut controller, "dribble-tick-", Duration::from_millis(750));
    dribbler.join().expect("join dribbler");

    assert!(
        output_while_dribbling.contains("dribble-tick-"),
        "attached output stalled behind a partial HTTP request; received {output_while_dribbling:?}"
    );
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

    let (_session, attach) =
        service.attach(Some("alpha".into()), None, None, Some("sleep 5".into()), false, AttachOptions::default()).expect("first attach");
    let pid_path = daemon_pid_path(temp.path(), "alpha");
    assert!(pid_path.exists());

    drop(attach);

    let (_session, _reattach) =
        service.attach(Some("alpha".into()), None, None, None, false, AttachOptions::default()).expect("reattach after disconnect");
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

    let (_session, _attach) = service
        .attach(Some("alpha".into()), None, None, None, false, AttachOptions::default())
        .expect("attach with stale foreground marker");
}

// A session flooding output at PTY saturation (`yes`) must not starve the
// daemon's control plane: the actor's pump slice is budgeted so commands
// keep draining, and the serve loop's actor requests carry a deadline
// (ADR 0004: the servicing side is never blocked). Probes use a raw
// socket with a read timeout because the CLI client would hang forever
// in the failure mode this guards against.
#[cfg(feature = "ghostty-vt")]
#[test]
fn control_socket_answers_while_a_session_floods_output() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    // The delay lets the create response escape before the flood begins.
    service.create(Some("flood".into()), None, None, Some("sh -c 'sleep 0.3; exec yes'".into()), false).expect("create flood session");
    std::thread::sleep(Duration::from_millis(700));

    for probe in 0..5 {
        let start = Instant::now();
        let socket = session_socket_path(temp.path(), "flood");
        let mut stream = UnixStream::connect(&socket).expect("connect control socket");
        stream.set_read_timeout(Some(Duration::from_secs(2))).expect("set read timeout");
        stream.write_all(b"GET /sessions HTTP/1.1\r\nHost: cleat\r\n\r\n").expect("write list request");
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => panic!("probe {probe}: daemon closed the control socket"),
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                Err(err) => {
                    panic!("probe {probe}: control plane starved by output flood after {:?}: {err}", start.elapsed())
                }
            }
        }
        assert!(start.elapsed() < Duration::from_secs(2), "probe {probe}: control response took {:?} under output flood", start.elapsed());
        std::thread::sleep(Duration::from_millis(100));
    }

    service.kill("flood").expect("kill flood session");
}

const DAEMON_LAUNCHER_ROOT: &str = "CLEAT_TEST_DAEMON_LAUNCHER_ROOT";

#[test]
#[ignore = "helper process for auto_started_daemon_survives_launcher_process_group_cleanup"]
fn daemon_process_group_launcher_helper() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let Some(root) = std::env::var_os(DAEMON_LAUNCHER_ROOT) else {
        return;
    };
    let service = service_for(std::path::Path::new(&root));
    service
        .create(Some("alpha".into()), Some(VtEngineKind::Passthrough), None, Some("sleep 30".into()), false)
        .expect("create session from launcher process");
    std::fs::write(std::path::Path::new(&root).join("launcher.ready"), b"").expect("write launcher readiness marker");
    loop {
        std::thread::park();
    }
}

#[test]
fn auto_started_daemon_survives_launcher_process_group_cleanup() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let mut launcher = Command::new(std::env::current_exe().expect("current lifecycle test executable"));
    launcher
        .args(["--ignored", "--exact", "daemon_process_group_launcher_helper", "--nocapture"])
        .env(DAEMON_LAUNCHER_ROOT, temp.path())
        .process_group(0);
    let mut launcher = launcher.spawn().expect("spawn isolated daemon launcher");
    let launcher_process_group = launcher.id() as i32;
    let ready_path = temp.path().join("launcher.ready");
    let ready_deadline = Instant::now() + Duration::from_secs(10);
    while !ready_path.exists() {
        if let Some(status) = launcher.try_wait().expect("poll daemon launcher") {
            panic!("daemon launcher exited before reporting readiness: {status:?}");
        }
        assert!(Instant::now() < ready_deadline, "timed out waiting for daemon launcher readiness");
        std::thread::sleep(Duration::from_millis(20));
    }

    let kill_result = unsafe { libc::killpg(launcher_process_group, libc::SIGKILL) };
    assert_eq!(kill_result, 0, "kill launcher process group: {}", std::io::Error::last_os_error());
    let status = launcher.wait().expect("reap daemon launcher");
    assert_eq!(status.signal(), Some(libc::SIGKILL), "daemon launcher should be killed with its process group");
    std::thread::sleep(Duration::from_millis(100));

    let service = service_for(temp.path());
    let inspected = service.inspect("alpha").expect("auto-started daemon should survive cleanup of the launcher process group");
    assert_eq!(inspected.session.id, "alpha");

    service.kill("alpha").expect("kill test session");
    cleat::platform::daemon::terminate_session_daemon_if_expected(temp.path(), DEFAULT_DAEMON_NAME);
}

#[test]
fn daemon_exits_when_runtime_root_is_removed() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), Some(VtEngineKind::Passthrough), None, Some("cat".into()), false).expect("create alpha");

    let pid: i32 =
        std::fs::read_to_string(daemon_pid_path(temp.path(), "alpha")).expect("read daemon pid").trim().parse().expect("parse daemon pid");

    // Delete the runtime root out from under the daemon, exactly as a dropped
    // test tempdir would. The hosted `cat` never exits on its own, so only
    // the registration watchdog can reap this daemon.
    std::fs::remove_dir_all(temp.path()).expect("remove runtime root");

    wait_for_daemon_exit(pid, "daemon should exit after its runtime root is removed");
}

#[test]
fn daemon_exits_when_pid_file_names_another_process() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), Some(VtEngineKind::Passthrough), None, Some("cat".into()), false).expect("create alpha");

    let pid_path = daemon_pid_path(temp.path(), "alpha");
    let pid: i32 = std::fs::read_to_string(&pid_path).expect("read daemon pid").trim().parse().expect("parse daemon pid");

    // Simulate a successor daemon reclaiming the identity: the pid file no
    // longer names this daemon, so it should fence itself off and exit.
    std::fs::write(&pid_path, "999999999").expect("overwrite daemon pid");

    wait_for_daemon_exit(pid, "daemon should exit after losing its pid-file registration");
}

#[test]
fn overlong_daemon_socket_path_is_rejected_before_spawn() {
    let temp = tempfile::Builder::new().prefix("cleat-socket-").tempdir_in("/tmp").expect("short-path tempdir");
    let root = temp.path().join("x".repeat(120));
    let service = service_for(&root);

    let started = Instant::now();
    let err = service
        .create(Some("alpha".into()), Some(VtEngineKind::Passthrough), None, Some("cat".into()), false)
        .expect_err("overlong daemon socket path should be rejected");

    assert!(started.elapsed() < Duration::from_secs(1), "socket path validation should fail before spawning a daemon");
    assert!(err.contains("Unix socket path"), "{err}");
    assert!(err.contains("CLEAT_RUNTIME_DIR"), "{err}");
}

#[test]
fn direct_daemon_serve_rejects_overlong_socket_path_before_creating_directories() {
    let temp = tempfile::Builder::new().prefix("cleat-socket-").tempdir_in("/tmp").expect("short-path tempdir");
    let root = temp.path().join("x".repeat(120));

    let err = run_session_daemon(&root, DEFAULT_DAEMON_NAME).expect_err("overlong daemon socket path should be rejected");

    assert!(err.contains("Unix socket path"), "{err}");
    assert!(!root.exists(), "socket validation should run before creating daemon directories");
}

#[cfg(feature = "ghostty-vt")]
fn isolated_discovery_command(cleat_bin: &str, home: &std::path::Path, tmpdir: &std::path::Path) -> Command {
    let mut command = Command::new(cleat_bin);
    command
        .env("HOME", home)
        .env("TMPDIR", tmpdir)
        .env_remove("CLEAT_RUNTIME_DIR")
        .env_remove("XDG_RUNTIME_DIR")
        .env_remove("XDG_STATE_HOME");
    command
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn discovered_state_root_survives_legacy_tmpdir_registration_purge() {
    let temp = tempfile::Builder::new().prefix("cleat-154-").tempdir_in("/tmp").expect("short-path tempdir");
    let home = temp.path().join("home");
    let tmpdir = temp.path().join("tmp");
    std::fs::create_dir_all(&home).expect("create isolated home");
    std::fs::create_dir_all(&tmpdir).expect("create isolated tmpdir");

    let cleat_bin = std::env::var("CARGO_BIN_EXE_cleat").expect("cleat bin");
    let mut command = isolated_discovery_command(&cleat_bin, &home, &tmpdir);
    command.args(["--server", "issue154-state-root", "launch", "alpha", "--vt", "ghostty", "--cmd", "cat", "--record", "--json"]);
    let output = command.output().expect("launch through discovered runtime root");
    assert!(output.status.success(), "launch failed: {}", String::from_utf8_lossy(&output.stderr));

    let state_daemon_dir = home.join(".local/state/cleat/issue154-state-root");
    assert!(state_daemon_dir.join("daemon.pid").exists(), "daemon registration should live in persistent state");
    assert!(state_daemon_dir.join("sessions/alpha/session.cast").exists(), "recording should live in persistent state");

    let legacy_root = tmpdir.join(format!("cleat-{}", unsafe { libc::geteuid() }));
    if legacy_root.exists() {
        std::fs::remove_dir_all(&legacy_root).expect("simulate purging the legacy TMPDIR runtime root");
    }

    std::thread::sleep(Duration::from_millis(2_500));

    let mut inspect = isolated_discovery_command(&cleat_bin, &home, &tmpdir);
    inspect.args(["--server", "issue154-state-root", "inspect", "alpha", "--json"]);
    let output = inspect.output().expect("inspect session after legacy TMPDIR purge");
    assert!(output.status.success(), "session did not survive legacy TMPDIR purge: {}", String::from_utf8_lossy(&output.stderr));

    let mut kill = isolated_discovery_command(&cleat_bin, &home, &tmpdir);
    kill.args(["--server", "issue154-state-root", "kill", "alpha"]);
    let output = kill.output().expect("kill surviving test session");
    assert!(output.status.success(), "kill failed: {}", String::from_utf8_lossy(&output.stderr));
}

/// The daemon is our direct child, so poll with waitpid rather than
/// kill(pid, 0): an exited-but-unreaped zombie still answers signals.
fn wait_for_daemon_exit(pid: i32, message: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut status: libc::c_int = 0;
        let rc = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if rc == pid {
            break;
        }
        assert!(rc == 0, "waitpid on session daemon failed: {rc}");
        assert!(Instant::now() < deadline, "{message}");
        std::thread::sleep(Duration::from_millis(100));
    }
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
    assert!(service.session_dir("alpha").exists(), "detach should leave the session directory intact");
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
    assert!(service.session_dir("alpha").exists(), "signal exit should leave the session directory intact");
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
        if service.inspect(&info.id).is_err() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(service.inspect(&info.id).is_err(), "session should be gone after SIGTERM to leader");
    assert!(socket_path.exists(), "daemon socket should linger after session exit");
}

#[test]
fn kill_terminates_background_children_in_leader_process_group() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let pid_file = temp.path().join("child.pid");
    // Non-interactive sh has no job control, so the background sleep stays in
    // the session leader's process group. Ignoring HUP before the fork makes the
    // sleep survive the SIGHUP the kernel sends when the session leader exits,
    // so only a signal aimed at the process group can take it down.
    let cmd = format!("sh -c 'trap \"\" HUP; sleep 100 & echo $! > {}; wait'", pid_file.display());
    let info = service.create(Some("tree".into()), None, None, Some(cmd), false).expect("create session");

    let socket_path = session_socket_path(temp.path(), &info.id);
    wait_for_socket(&socket_path);

    let deadline = Instant::now() + Duration::from_secs(5);
    let child_pid = loop {
        if let Some(pid) = std::fs::read_to_string(&pid_file).ok().and_then(|contents| contents.trim().parse::<i32>().ok()) {
            break pid;
        }
        assert!(Instant::now() < deadline, "timed out waiting for background child pid file");
        std::thread::sleep(Duration::from_millis(20));
    };
    // SAFETY: signal 0 performs existence and permission checks only.
    assert_eq!(unsafe { libc::kill(child_pid, 0) }, 0, "background child should be alive before kill");

    service.kill(&info.id).expect("kill session");

    wait_until("background child to die after cleat kill", || {
        // SAFETY: signal 0 performs existence and permission checks only.
        let rc = unsafe { libc::kill(child_pid, 0) };
        rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    });
}

#[test]
fn short_lived_session_reaps_its_directory_after_child_exit() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, None, Some("printf done; sleep 0.1".into()), false).expect("create alpha");

    let session_dir = service.session_dir("alpha");
    let deadline = Instant::now() + Duration::from_secs(2);
    while session_dir.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }

    assert!(!session_dir.exists(), "session directory should be reaped after child exit");

    let beta = service
        .create(Some("beta".into()), Some(VtEngineKind::Passthrough), None, Some("sleep 30".into()), false)
        .expect("create second session via lingering daemon");
    assert_eq!(beta.id, "beta");
    service.kill("beta").expect("kill second session");
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
    let cast_path = service.session_dir("alpha").join(cleat::recording::CAST_FILE_NAME);
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
