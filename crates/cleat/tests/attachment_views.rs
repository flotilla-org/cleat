//! Contracts for several views on one packet attachment (protocol 16), through
//! a real daemon: one live render stream per attachment, per-view history,
//! roles and size votes, attachment-scoped clipboard, view lifetimes, and
//! unchanged behaviour for peers that did not opt in.
#![cfg(all(unix, feature = "ghostty-vt"))]

use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    sync::Mutex,
    time::{Duration, Instant},
};

use cleat::{
    packet::{
        Ack, ChannelRole, CloseView, ControlHello, Input, OpenChannel, OpenView, PacketFrame, RenderPacket, Resize, RoleRequest, RoleState,
        ViewFrame, Viewport, CHANNEL_CONTROL, MIN_SUPPORTED_PROTOCOL_VERSION, MSG_CONTROL_DIRECTORY_SNAPSHOT, MSG_CONTROL_HELLO,
        MSG_CONTROL_OPEN_CHANNEL, MSG_SESSION_ACK, MSG_SESSION_CLIPBOARD, MSG_SESSION_CLOSE_VIEW, MSG_SESSION_INPUT, MSG_SESSION_OPEN_VIEW,
        MSG_SESSION_RENDER, MSG_SESSION_RENDER_VIRTUAL, MSG_SESSION_RESIZE, MSG_SESSION_ROLE, MSG_SESSION_SIZE_POLICY, MSG_SESSION_VIEW,
        MSG_SESSION_VIEWPORT, MSG_SESSION_VIEW_STATE, PROTOCOL_VERSION, VIEWS_PROTOCOL_VERSION,
    },
    protocol::AttachmentIdentity,
    provider::{TerminalInputEvent, TerminalPasteEvent, TerminalViewportKind, ViewState, ViewStatus, ViewportCommand},
    runtime::{RuntimeLayout, TerminalSize},
    server::SessionService,
    vt::VtEngineKind,
};

// Daemons spawned by these fixtures share process-wide state (environment,
// output admission); run one fixture at a time.
fn fixture_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

const WAIT: Duration = Duration::from_secs(10);

struct Fixture {
    temp: tempfile::TempDir,
    sessions: Vec<String>,
}

impl Fixture {
    fn new() -> Self {
        Self { temp: tempfile::Builder::new().prefix("cleat-views-").tempdir_in("/tmp").unwrap(), sessions: Vec::new() }
    }

    fn service(&self) -> SessionService {
        SessionService::new(RuntimeLayout::new(self.temp.path().to_path_buf()))
    }

    fn create(&mut self, id: &str, command: &str) {
        self.create_sized(id, command, TerminalSize::default());
    }

    fn create_sized(&mut self, id: &str, command: &str, size: TerminalSize) {
        self.service().create_with_size(Some(id.into()), Some(VtEngineKind::Ghostty), None, Some(command.into()), false, size).unwrap();
        self.sessions.push(id.into());
    }

    fn capture(&self, id: &str) -> String {
        self.service().capture(id).unwrap()
    }

    fn size(&self, id: &str) -> (u16, u16) {
        let terminal = self.service().inspect(id).unwrap().terminal;
        (terminal.cols, terminal.rows)
    }

    fn peer(&self, views: bool) -> Peer {
        Peer::connect(&RuntimeLayout::new(self.temp.path().to_path_buf()), views)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for id in &self.sessions {
            let _ = self.service().kill(id);
        }
    }
}

#[track_caller]
fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A received session message: `view` is set when it arrived view-wrapped.
#[derive(Clone, Debug)]
struct Seen {
    channel: u32,
    view: Option<u32>,
    frame: PacketFrame,
}

impl Seen {
    fn is_render(&self) -> bool {
        matches!(self.frame.msg_type, MSG_SESSION_RENDER | MSG_SESSION_RENDER_VIRTUAL)
    }

    fn render(&self) -> RenderPacket {
        RenderPacket::decode_frame(&self.frame).expect("decode render")
    }

    fn role(&self) -> RoleState {
        self.frame.decode().expect("decode role state")
    }

    fn view_state(&self) -> ViewState {
        self.frame.decode().expect("decode view state")
    }
}

/// One packet connection. It acknowledges every render it reads, so streams
/// keep flowing, and keeps what it saw for the test to inspect.
struct Peer {
    stream: UnixStream,
    buffer: Vec<u8>,
    hello: ControlHello,
    seen: Vec<Seen>,
}

impl Peer {
    fn connect(layout: &RuntimeLayout, views: bool) -> Self {
        let mut stream = UnixStream::connect(layout.socket_path()).unwrap();
        let body = if views { r#"{"views":true}"# } else { "" };
        write!(
            stream,
            "POST /connect HTTP/1.1\r\nHost: cleat\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: Upgrade\r\nUpgrade: cleat-packet/1\r\nx-cleat-output-context: {{\"version\":1,\"context\":{{\"kind\":\"external\"}}}}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8];
        while !head.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            head.push(byte[0]);
        }
        assert!(head.starts_with(b"HTTP/1.1 101"), "{}", String::from_utf8_lossy(&head));
        let hello = PacketFrame::read(&mut stream).unwrap();
        assert_eq!((hello.channel, hello.msg_type), (CHANNEL_CONTROL, MSG_CONTROL_HELLO));
        let directory = PacketFrame::read(&mut stream).unwrap();
        assert_eq!(directory.msg_type, MSG_CONTROL_DIRECTORY_SNAPSHOT);
        stream.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
        Self { stream, buffer: Vec::new(), hello: hello.decode().unwrap(), seen: Vec::new() }
    }

    fn send<T: serde::Serialize>(&mut self, channel: u32, msg_type: u8, value: &T) {
        PacketFrame::new(channel, msg_type, value).unwrap().write(&mut self.stream).unwrap();
    }

    fn send_view<T: serde::Serialize>(&mut self, channel: u32, view: u32, msg_type: u8, value: &T) {
        ViewFrame::wrap(view, &PacketFrame::new(channel, msg_type, value).unwrap()).unwrap().write(&mut self.stream).unwrap();
    }

    fn open_channel(&mut self, channel: u32, session_id: &str, role: ChannelRole) {
        self.send(CHANNEL_CONTROL, MSG_CONTROL_OPEN_CHANNEL, &OpenChannel {
            channel,
            session_id: session_id.into(),
            role,
            take: false,
            identity: AttachmentIdentity::default(),
        });
    }

    fn open_view(&mut self, channel: u32, view: u32, role: ChannelRole) {
        self.send(channel, MSG_SESSION_OPEN_VIEW, &OpenView { view, role, take: false });
    }

    fn close_view(&mut self, channel: u32, view: u32) {
        self.send(channel, MSG_SESSION_CLOSE_VIEW, &CloseView { view });
    }

    fn paste(&mut self, channel: u32, view: u32, text: &str) {
        self.send_view(channel, view, MSG_SESSION_INPUT, &Input {
            event: TerminalInputEvent::Paste(TerminalPasteEvent { text: text.into() }),
        });
    }

    fn resize(&mut self, channel: u32, view: u32, cols: u16, rows: u16) {
        self.send_view(channel, view, MSG_SESSION_RESIZE, &Resize { cols, rows });
    }

    fn viewport(&mut self, channel: u32, view: u32, command: ViewportCommand) {
        self.send_view(channel, view, MSG_SESSION_VIEWPORT, &Viewport { command });
    }

    fn role(&mut self, channel: u32, view: u32, role: ChannelRole, take: bool) {
        self.send_view(channel, view, MSG_SESSION_ROLE, &RoleRequest { role, take });
    }

    /// Read whatever arrives within `window`, acknowledging each render.
    fn pump(&mut self, window: Duration) {
        let deadline = Instant::now() + window;
        loop {
            while let Some(frame) = PacketFrame::read_from_buffer(&mut self.buffer).unwrap() {
                let seen = if frame.msg_type == MSG_SESSION_VIEW {
                    let (view, inner) = frame.decode::<ViewFrame>().unwrap().unwrap(frame.channel);
                    Seen { channel: frame.channel, view: Some(view), frame: inner }
                } else {
                    Seen { channel: frame.channel, view: None, frame }
                };
                if seen.is_render() {
                    let ack = Ack { generation: seen.render().update.render_generation };
                    match seen.view {
                        Some(view) => self.send_view(seen.channel, view, MSG_SESSION_ACK, &ack),
                        None => self.send(seen.channel, MSG_SESSION_ACK, &ack),
                    }
                }
                self.seen.push(seen);
            }
            if Instant::now() >= deadline {
                return;
            }
            let mut chunk = [0u8; 16 * 1024];
            match self.stream.read(&mut chunk) {
                Ok(0) => panic!("packet stream closed"),
                Ok(n) => self.buffer.extend_from_slice(&chunk[..n]),
                Err(err) if matches!(err.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(err) => panic!("read packet stream: {err}"),
            }
        }
    }

    /// Wait for, and remove, the first message matching `keep`.
    #[track_caller]
    fn expect(&mut self, what: &str, mut keep: impl FnMut(&Seen) -> bool) -> Seen {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(index) = self.seen.iter().position(&mut keep) {
                return self.seen.remove(index);
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}; saw {:?}", self.summary());
            self.pump(Duration::from_millis(20));
        }
    }

    #[track_caller]
    fn expect_role(&mut self, channel: u32, view: Option<u32>) -> RoleState {
        self.expect("role state", |seen| seen.channel == channel && seen.view == view && seen.frame.msg_type == MSG_SESSION_ROLE).role()
    }

    /// Drain until no render has arrived for `quiet`, then forget what was seen.
    fn settle(&mut self, quiet: Duration) {
        let deadline = Instant::now() + WAIT;
        let mut last = Instant::now();
        while last.elapsed() < quiet && Instant::now() < deadline {
            let before = self.seen.len();
            self.pump(Duration::from_millis(20));
            if self.seen[before..].iter().any(Seen::is_render) {
                last = Instant::now();
            }
        }
        self.seen.clear();
    }

    fn summary(&self) -> Vec<(u32, Option<u32>, u8)> {
        self.seen.iter().map(|seen| (seen.channel, seen.view, seen.frame.msg_type)).collect()
    }
}

fn render_text(packet: &RenderPacket) -> String {
    packet
        .update
        .ops
        .iter()
        .flat_map(|op| &op.rows)
        .flat_map(|row| &row.cells)
        .flat_map(|cell| cell.graphemes.iter().filter_map(|scalar| char::from_u32(*scalar)))
        .collect()
}

/// Open a views attachment on channel 1 with its default view, then the
/// given extra views, and wait for every grant and the initial render.
fn attach_views(peer: &mut Peer, session: &str, default_role: ChannelRole, extra: &[(u32, ChannelRole)]) {
    peer.open_channel(1, session, default_role);
    assert_eq!(peer.expect_role(1, Some(0)).role, default_role);
    peer.expect("initial live render", |seen| seen.channel == 1 && seen.view.is_none() && seen.is_render());
    for (view, role) in extra {
        peer.open_view(1, *view, *role);
        assert_eq!(peer.expect_role(1, Some(*view)).role, *role);
    }
}

const SEEDED_HISTORY: &str = "sh -c 'i=0; while [ $i -lt 60 ]; do echo line-$i; i=$((i+1)); done; exec cat'";

// 1. Live views share their attachment's one render stream: a second view
//    adds no render frames of its own. Channel 2, a one-view attachment of the
//    same session, is the baseline for how many frames the output takes.
#[test]
fn two_live_views_on_one_channel_receive_one_render_stream() {
    let _lock = fixture_lock();
    let mut fixture = Fixture::new();
    fixture.create("alpha", "cat");
    let mut peer = fixture.peer(true);
    assert_eq!(peer.hello.version, PROTOCOL_VERSION);
    attach_views(&mut peer, "alpha", ChannelRole::Controller, &[(1, ChannelRole::Controller)]);
    peer.open_channel(2, "alpha", ChannelRole::Watcher);
    peer.expect("baseline initial render", |seen| seen.channel == 2 && seen.is_render());
    peer.settle(Duration::from_millis(200));

    for index in 0..6 {
        peer.paste(1, index % 2, &format!("shared-{index}\n"));
        for channel in [1, 2] {
            peer.expect("live render of the shared output", |seen| {
                seen.channel == channel && seen.is_render() && render_text(&seen.render()).contains(&format!("shared-{index}"))
            });
        }
    }
    peer.pump(Duration::from_millis(300));
    let renders = |channel| peer.seen.iter().filter(|seen| seen.channel == channel && seen.is_render()).count();
    let (shared, baseline) = (renders(1), renders(2));
    assert!(shared <= baseline + 1, "two views took {shared} extra frames where one view took {baseline}: {:?}", peer.summary());
    assert!(peer.seen.iter().all(|seen| !seen.is_render() || seen.view.is_none()), "a live view received its own render");
    assert!(fixture.capture("alpha").contains("shared-5"));
}

// 2. A view in history gets its own frames while the attachment's other view
//    stays live. Typing in a controller view returns it, and only it, to live.
#[test]
fn a_history_view_renders_alone_and_typing_returns_only_that_view_to_live() {
    let _lock = fixture_lock();
    let mut fixture = Fixture::new();
    fixture.create("alpha", SEEDED_HISTORY);
    wait_until("history seeded", || fixture.capture("alpha").contains("line-59"));
    let mut peer = fixture.peer(true);
    attach_views(&mut peer, "alpha", ChannelRole::Controller, &[(1, ChannelRole::Controller), (2, ChannelRole::Controller)]);
    peer.settle(Duration::from_millis(200));

    for view in [1, 2] {
        peer.viewport(1, view, ViewportCommand::Top);
        let history = peer.expect("history frame", |seen| seen.view == Some(view) && seen.is_render()).render();
        assert_eq!(history.view.status, ViewStatus::History);
        assert_eq!(history.update.viewport_kind, TerminalViewportKind::NormalScrollback);
        assert_eq!(history.update.scrollbar.viewport_top_row, 0);
    }
    peer.pump(Duration::from_millis(200));
    assert!(!peer.seen.iter().any(|seen| seen.is_render() && seen.view == Some(0)), "the live view received a history frame");
    peer.seen.clear();

    // The live view keeps following output on the shared stream.
    peer.paste(1, 0, "from-live-view\n");
    let live = peer
        .expect("live render", |seen| seen.view.is_none() && seen.is_render() && render_text(&seen.render()).contains("from-live-view"));
    assert_eq!(live.render().update.viewport_kind, TerminalViewportKind::LiveNormal);
    peer.pump(Duration::from_millis(300));
    let returned_live = |peer: &Peer, view| {
        peer.seen.iter().any(|seen| {
            seen.view == Some(view) && seen.frame.msg_type == MSG_SESSION_VIEW_STATE && seen.view_state().status == ViewStatus::Live
        })
    };
    assert!(!returned_live(&peer, 1) && !returned_live(&peer, 2), "typing in view 0 returned other views to live");
    peer.seen.clear();

    // Typing in view 1 brings view 1 back to live; view 2 stays in history.
    peer.paste(1, 1, "from-view-one\n");
    let state = peer.expect("view 1 live state", |seen| seen.view == Some(1) && seen.frame.msg_type == MSG_SESSION_VIEW_STATE);
    assert_eq!(state.view_state().status, ViewStatus::Live);
    peer.expect("live render of view 1's input", |seen| {
        seen.view.is_none() && seen.is_render() && render_text(&seen.render()).contains("from-view-one")
    });
    let still = peer.expect("view 2 history frame", |seen| seen.view == Some(2) && seen.is_render()).render();
    assert_eq!(still.update.viewport_kind, TerminalViewportKind::NormalScrollback);
    assert_eq!(still.update.scrollbar.viewport_top_row, 0);
    peer.pump(Duration::from_millis(300));
    assert!(!returned_live(&peer, 2), "view 2 returned to live with view 1");
}

// 3. The grid is the intersection over controller views, across attachments.
//    Watcher views do not vote; a role change re-votes; a pinned size wins.
#[test]
fn grid_size_intersects_controller_views_across_attachments() {
    let _lock = fixture_lock();
    let mut fixture = Fixture::new();
    fixture.create("alpha", "cat");
    let mut first = fixture.peer(true);
    attach_views(&mut first, "alpha", ChannelRole::Controller, &[(1, ChannelRole::Controller), (2, ChannelRole::Watcher)]);
    let mut second = fixture.peer(true);
    attach_views(&mut second, "alpha", ChannelRole::Controller, &[]);

    first.resize(1, 0, 100, 20);
    first.resize(1, 1, 80, 40);
    second.resize(1, 0, 90, 30);
    wait_until("controller views intersect", || fixture.size("alpha") == (80, 20));

    first.resize(1, 2, 10, 5);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(fixture.size("alpha"), (80, 20), "a watcher view voted");

    first.role(1, 1, ChannelRole::Watcher, false);
    wait_until("view 1 stops voting", || fixture.size("alpha") == (90, 20));

    first.send(1, MSG_SESSION_SIZE_POLICY, &Some(Resize { cols: 70, rows: 25 }));
    wait_until("pinned size", || fixture.size("alpha") == (70, 25));
    second.resize(1, 0, 50, 10);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(fixture.size("alpha"), (70, 25), "a controller view overrode the pinned size");
    first.send(1, MSG_SESSION_SIZE_POLICY, &None::<Resize>);
    wait_until("automatic sizing restored", || fixture.size("alpha") == (50, 10));
}

// 4. Taking control on one view demotes every other controller view, on its
//    own attachment and on others.
#[test]
fn take_control_on_a_view_demotes_the_other_controller_views() {
    let _lock = fixture_lock();
    let mut fixture = Fixture::new();
    fixture.create("alpha", "cat");
    let mut first = fixture.peer(true);
    attach_views(&mut first, "alpha", ChannelRole::Controller, &[(1, ChannelRole::Controller)]);
    let mut legacy = fixture.peer(false);
    legacy.open_channel(1, "alpha", ChannelRole::Controller);
    assert_eq!(legacy.expect_role(1, None).role, ChannelRole::Controller);
    first.pump(Duration::from_millis(200));
    first.seen.clear();

    first.role(1, 1, ChannelRole::Controller, true);
    let granted = first.expect_role(1, Some(1));
    assert_eq!(granted.role, ChannelRole::Controller);
    assert!(granted.exclusive.is_some());
    first.expect("view 0 demoted", |seen| {
        seen.view == Some(0) && seen.frame.msg_type == MSG_SESSION_ROLE && seen.role().role == ChannelRole::Watcher
    });
    legacy.expect("other attachment demoted", |seen| {
        seen.view.is_none() && seen.frame.msg_type == MSG_SESSION_ROLE && seen.role().role == ChannelRole::Watcher
    });
    let inspect = fixture.service().inspect("alpha").unwrap();
    assert_eq!(inspect.attachments.iter().filter(|a| a.role == "controller").count(), 1);

    // Only the exclusive view's input reaches the PTY.
    first.paste(1, 0, "demoted-view\n");
    first.paste(1, 1, "exclusive-view\n");
    wait_until("exclusive input", || fixture.capture("alpha").contains("exclusive-view"));
    assert!(!fixture.capture("alpha").contains("demoted-view"));
}

const CLIPBOARD_COMMAND: &str =
    "sh -c 'stty -echo; while IFS= read -r line; do case \"$line\" in copy) printf \"\\033]52;c;aGVsbG8=\\007\";; esac; done'";

// 5. Clipboard effects reach an attachment once, however many controller
//    views it has, and never an attachment with only watcher views.
#[test]
fn clipboard_reaches_an_attachment_once_and_never_a_watching_one() {
    let _lock = fixture_lock();
    let mut fixture = Fixture::new();
    fixture.create("alpha", CLIPBOARD_COMMAND);
    // The watching attachment comes first, so it would be first in line if
    // clipboard delivery ignored roles.
    let mut watching = fixture.peer(true);
    attach_views(&mut watching, "alpha", ChannelRole::Watcher, &[(1, ChannelRole::Watcher)]);
    let mut driving = fixture.peer(true);
    attach_views(&mut driving, "alpha", ChannelRole::Controller, &[(1, ChannelRole::Controller)]);
    driving.settle(Duration::from_millis(200));
    watching.settle(Duration::from_millis(200));

    driving.paste(1, 1, "copy\n");
    driving.expect("clipboard event", |seen| seen.frame.msg_type == MSG_SESSION_CLIPBOARD);
    driving.pump(Duration::from_millis(500));
    watching.pump(Duration::from_millis(100));
    let copies = |peer: &Peer| peer.seen.iter().filter(|seen| seen.frame.msg_type == MSG_SESSION_CLIPBOARD).count();
    assert_eq!(copies(&driving), 0, "the attachment received one copy per controller view");
    assert_eq!(copies(&watching), 0, "a watching attachment received the clipboard");
}

// 6. Closing a view leaves its attachment's other views working; closing the
//    last one closes nothing else on the connection.
#[test]
fn closing_views_leaves_the_rest_of_the_attachment_and_connection_working() {
    let _lock = fixture_lock();
    let mut fixture = Fixture::new();
    fixture.create("alpha", "cat");
    fixture.create("beta", "cat");
    let mut peer = fixture.peer(true);
    attach_views(&mut peer, "alpha", ChannelRole::Controller, &[(1, ChannelRole::Controller), (2, ChannelRole::Controller)]);
    peer.open_channel(2, "beta", ChannelRole::Controller);
    assert_eq!(peer.expect_role(2, Some(0)).role, ChannelRole::Controller);
    peer.settle(Duration::from_millis(200));

    peer.close_view(1, 1);
    peer.paste(1, 2, "after-close\n");
    peer.expect("live render after a view closed", |seen| {
        seen.channel == 1 && seen.view.is_none() && seen.is_render() && render_text(&seen.render()).contains("after-close")
    });
    peer.resize(1, 0, 70, 20);
    peer.resize(1, 2, 75, 22);
    wait_until("remaining views still vote", || fixture.size("alpha") == (70, 20));

    peer.close_view(1, 0);
    peer.close_view(1, 2);
    wait_until("no controller views remain", || fixture.service().inspect("alpha").unwrap().attachments.is_empty());
    peer.settle(Duration::from_millis(100));
    fixture.service().send_keys("alpha", b"while-viewless\n").unwrap();
    wait_until("output while viewless", || fixture.capture("alpha").contains("while-viewless"));
    // The other channel on the connection is untouched.
    peer.paste(2, 0, "beta-still-open\n");
    peer.expect("beta render", |seen| seen.channel == 2 && seen.is_render() && render_text(&seen.render()).contains("beta-still-open"));
    assert!(!peer.seen.iter().any(|seen| seen.channel == 1 && seen.is_render()), "a viewless attachment kept rendering");
    // The viewless attachment stays open and can show the session again.
    peer.open_view(1, 3, ChannelRole::Watcher);
    assert_eq!(peer.expect_role(1, Some(3)).role, ChannelRole::Watcher);
    peer.expect("live stream resumes", |seen| seen.channel == 1 && seen.view.is_none() && seen.is_render());
}

// A refused view closes alone: a reserved id or one view too many is closed
// by the daemon, a reused id is ignored, and the attachment carries on.
#[test]
fn refused_views_close_alone() {
    let _lock = fixture_lock();
    let mut fixture = Fixture::new();
    fixture.create("alpha", "cat");
    let mut peer = fixture.peer(true);
    attach_views(&mut peer, "alpha", ChannelRole::Controller, &[]);
    for view in 1..64 {
        peer.open_view(1, view, ChannelRole::Watcher);
    }
    for view in 1..64 {
        peer.expect_role(1, Some(view));
    }
    for view in [64, u32::MAX] {
        peer.open_view(1, view, ChannelRole::Controller);
        let refused = peer.expect("refused view", |seen| seen.view.is_none() && seen.frame.msg_type == MSG_SESSION_CLOSE_VIEW);
        assert_eq!(refused.frame.decode::<CloseView>().unwrap(), CloseView { view });
    }
    peer.open_view(1, 1, ChannelRole::Controller);
    peer.pump(Duration::from_millis(300));
    let attachments = fixture.service().inspect("alpha").unwrap().attachments;
    assert_eq!(attachments.len(), 64);
    assert_eq!(attachments.iter().filter(|a| a.role == "controller").count(), 1, "a reused view id was granted again");
    peer.paste(1, 0, "still-attached\n");
    peer.expect("live render", |seen| seen.channel == 1 && seen.is_render() && render_text(&seen.render()).contains("still-attached"));
}

// 7. A peer that did not opt in, as every peer before protocol 16, keeps one
//    implicit view per channel and today's unwrapped messages.
#[test]
fn a_previous_protocol_peer_keeps_one_view_per_channel() {
    let _lock = fixture_lock();
    let mut fixture = Fixture::new();
    fixture.create("alpha", SEEDED_HISTORY);
    wait_until("history seeded", || fixture.capture("alpha").contains("line-59"));
    let mut peer = fixture.peer(false);
    assert!(peer.hello.version < VIEWS_PROTOCOL_VERSION);
    assert_eq!(peer.hello.min_supported_version, MIN_SUPPORTED_PROTOCOL_VERSION);
    peer.open_channel(1, "alpha", ChannelRole::Controller);
    assert_eq!(peer.expect_role(1, None).role, ChannelRole::Controller);
    peer.expect("initial render", |seen| seen.view.is_none() && seen.is_render());
    peer.settle(Duration::from_millis(200));

    // View messages mean nothing to such a channel.
    peer.open_view(1, 1, ChannelRole::Controller);
    peer.send_view(1, 1, MSG_SESSION_RESIZE, &Resize { cols: 20, rows: 5 });
    peer.send(1, MSG_SESSION_RESIZE, &Resize { cols: 100, rows: 30 });
    wait_until("plain resize applies", || fixture.size("alpha") == (100, 30));
    assert_eq!(fixture.service().inspect("alpha").unwrap().attachments.len(), 1);
    peer.settle(Duration::from_millis(200));

    // History, typing back to live and roles travel unwrapped on the channel.
    peer.send(1, MSG_SESSION_VIEWPORT, &Viewport { command: ViewportCommand::Top });
    let history = peer.expect("history frame", |seen| seen.is_render()).render();
    assert_eq!(history.update.viewport_kind, TerminalViewportKind::NormalScrollback);
    peer.send(1, MSG_SESSION_INPUT, &Input { event: TerminalInputEvent::Paste(TerminalPasteEvent { text: "back-live\n".into() }) });
    let live = peer.expect("live frame", |seen| seen.is_render() && seen.render().update.viewport_kind == TerminalViewportKind::LiveNormal);
    assert_eq!(live.view, None);
    peer.send(1, MSG_SESSION_ROLE, &RoleRequest { role: ChannelRole::Watcher, take: false });
    assert_eq!(peer.expect_role(1, None).role, ChannelRole::Watcher);
    peer.pump(Duration::from_millis(200));
    assert!(peer.seen.iter().all(|seen| seen.view.is_none()), "a previous-protocol peer received wrapped messages");
}

mod c_api {
    use cleat::provider_ffi::*;

    use super::*;

    /// Text and viewport kind of the view's next render update, if any.
    fn next_update(view: *mut CleatView) -> Option<(String, u32)> {
        unsafe {
            let mut update = CleatRenderUpdate::default();
            assert!(cleat_view_render_update(view, &mut update));
            let mut text = String::new();
            if update.op_count > 0 {
                for op in std::slice::from_raw_parts(update.ops, update.op_count) {
                    if op.cell_count > 0 {
                        for cell in std::slice::from_raw_parts(op.cells, op.cell_count) {
                            if cell.grapheme_count > 0 {
                                text.extend(
                                    std::slice::from_raw_parts(cell.graphemes, cell.grapheme_count)
                                        .iter()
                                        .filter_map(|c| char::from_u32(*c)),
                                );
                            }
                        }
                    }
                }
            }
            let changed = update.dirty != CleatDirtyState::Clean;
            let kind = update.viewport_kind;
            cleat_view_release_render_update(view, &mut update);
            changed.then_some((text, kind))
        }
    }

    #[track_caller]
    fn wait_for_update(view: *mut CleatView, what: &str, mut keep: impl FnMut(&str, u32) -> bool) {
        wait_until(what, || next_update(view).is_some_and(|(text, kind)| keep(&text, kind)));
    }

    fn view_desc(cols: u16, rows: u16, role: u32) -> CleatViewDesc {
        CleatViewDesc { cols, rows, cell_width_px: 8.0, cell_height_px: 16.0, role }
    }

    // A host shows one session in two places through one session handle: two
    // views, one attachment, one update stream, sized by both views' votes.
    #[test]
    fn two_views_on_one_session_handle() {
        let _lock = fixture_lock();
        let mut fixture = Fixture::new();
        let root = fixture.temp.path().to_str().unwrap().to_string();
        unsafe {
            let provider = cleat_provider_open(&CleatProviderDesc {
                abi_version: CLEAT_PROVIDER_ABI_VERSION,
                backend: CLEAT_PROVIDER_BACKEND_DAEMON,
                runtime_root: root.as_ptr(),
                runtime_root_len: root.len(),
                ..Default::default()
            });
            assert!(!provider.is_null());
            let (id, command) = (b"alpha", SEEDED_HISTORY.as_bytes());
            let session = cleat_session_create(provider, &CleatSessionDesc {
                cols: 80,
                rows: 24,
                vt_engine: CLEAT_PROVIDER_VT_GHOSTTY,
                id: id.as_ptr(),
                id_len: id.len(),
                command: command.as_ptr(),
                command_len: command.len(),
                role: CLEAT_ROLE_WATCHER,
                ..Default::default()
            });
            assert!(!session.is_null());
            fixture.sessions.push("alpha".into());
            wait_until("history seeded", || fixture.capture("alpha").contains("line-59"));
            wait_until("default view grant", || cleat_session_role(session) == CLEAT_ROLE_WATCHER);
            assert!(cleat_session_close_default_view(session));
            assert_eq!(cleat_session_role(session), CLEAT_ROLE_UNKNOWN);

            let wide = cleat_session_open_view(session, &view_desc(100, 30, CLEAT_ROLE_CONTROLLER));
            let narrow = cleat_session_open_view(session, &view_desc(70, 20, CLEAT_ROLE_CONTROLLER));
            assert!(!wide.is_null() && !narrow.is_null());
            for view in [wide, narrow] {
                wait_until("view grant", || cleat_view_role(view) == CLEAT_ROLE_CONTROLLER);
                wait_until("view streams", || cleat_view_connection_state(view) == CLEAT_SESSION_STREAMING);
            }
            wait_until("views intersect", || fixture.size("alpha") == (70, 20));
            assert_eq!(fixture.service().inspect("alpha").unwrap().attachments.len(), 2);

            let text = b"from-the-wide-view\n";
            assert!(cleat_view_write_bytes(wide, text.as_ptr(), text.len()));
            for view in [wide, narrow] {
                wait_for_update(view, "both views show the output", |text, _| text.contains("from-the-wide-view"));
            }

            // Scrolling one view leaves the other live.
            let top = CleatViewportCommand { kind: CLEAT_VIEWPORT_COMMAND_TOP, delta_rows: 0 };
            assert!(cleat_view_scroll_viewport(narrow, &top, std::ptr::null_mut()));
            wait_for_update(narrow, "history frame", |_, kind| kind == CLEAT_VIEWPORT_NORMAL_SCROLLBACK);
            let text = b"while-narrow-browses\n";
            assert!(cleat_view_write_bytes(wide, text.as_ptr(), text.len()));
            wait_for_update(wide, "live output", |text, kind| kind == CLEAT_VIEWPORT_LIVE_NORMAL && text.contains("while-narrow-browses"));

            // Typing in the browsing view brings it back to the live grid.
            let text = b"narrow-types\n";
            assert!(cleat_view_write_bytes(narrow, text.as_ptr(), text.len()));
            wait_for_update(narrow, "back to live", |text, kind| kind == CLEAT_VIEWPORT_LIVE_NORMAL && text.contains("narrow-types"));

            cleat_view_destroy(narrow);
            wait_until("the closed view stops voting", || fixture.size("alpha") == (100, 30));
            cleat_view_destroy(wide);
            wait_until("no views remain", || fixture.service().inspect("alpha").unwrap().attachments.is_empty());
            cleat_session_destroy(session);
            cleat_provider_close(provider);
        }
    }
}
