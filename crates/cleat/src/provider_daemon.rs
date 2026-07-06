//! Packet-connection daemon backend for the provider FFI.
//!
//! One multiplexed connection per daemon: channel 0 carries the directory
//! subscription (snapshot + live deltas), non-zero channels carry per-session
//! render packets down and input/resize/ack packets up. Render delivery is
//! ack-gated server-side (one un-acked packet in flight), so a slow consumer
//! receives coalesced state and can never stall the terminal.
//!
//! A dedicated reader thread owns the connection: it (re)connects with
//! backoff, re-opens session channels after a reconnect (the daemon responds
//! with a full render), applies directory deltas, and parks incoming render
//! updates in per-channel slots for the FFI caller to consume.

use std::{
    collections::HashMap,
    io::Write,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
    time::Duration,
};

use http::StatusCode;

use crate::{
    http_uds,
    packet::{
        ControlError, ControlHello, DirectoryDelta, DirectoryEntry, DirectorySnapshot, OpenChannel, PacketFrame, RenderPacket,
        CHANNEL_CONTROL, MSG_CONTROL_DIRECTORY_DELTA, MSG_CONTROL_DIRECTORY_SNAPSHOT, MSG_CONTROL_ERROR, MSG_CONTROL_HELLO,
        MSG_CONTROL_OPEN_CHANNEL, MSG_SESSION_ACK, MSG_SESSION_INPUT, MSG_SESSION_RENDER, MSG_SESSION_RESIZE, PROTOCOL_VERSION,
    },
    platform::ipc::{try_connect_session_stream, SessionStream},
    provider::{
        DirtyState, TerminalCursor, TerminalInputEvent, TerminalRenderUpdate, TerminalRenderUpdateOpKind, TerminalScrollbarState,
        TerminalViewportKind,
    },
    runtime::RuntimeLayout,
    vt,
};

pub(crate) type WakeFn = Arc<dyn Fn() + Send + Sync + 'static>;

const RECONNECT_BACKOFF_START: Duration = Duration::from_millis(100);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(2);
const BACKOFF_POLL_STEP: Duration = Duration::from_millis(10);
/// Input events queued while the connection is down (they flush after the
/// session channels are re-opened). Beyond this the send reports failure.
const MAX_QUEUED_INPUT_FRAMES: usize = 1024;

/// State the last consumed render packet left behind. Used to answer
/// scrollbar/extent queries between packets and to synthesize a clean update
/// when the caller asks for one while nothing is pending.
#[derive(Clone, Debug)]
pub(crate) struct LastKnown {
    pub cols: u16,
    pub rows: u16,
    pub viewport_kind: TerminalViewportKind,
    pub scrollback_offset_rows: u64,
    pub scrollbar: TerminalScrollbarState,
    pub terminal_modes: vt::TerminalModeState,
    pub cursor: TerminalCursor,
    pub render_generation: u64,
}

impl LastKnown {
    fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols,
            rows,
            viewport_kind: TerminalViewportKind::LiveNormal,
            scrollback_offset_rows: 0,
            scrollbar: TerminalScrollbarState::for_live_viewport(TerminalViewportKind::LiveNormal, rows),
            terminal_modes: vt::TerminalModeState::default(),
            cursor: TerminalCursor::default(),
            render_generation: 0,
        }
    }

    pub(crate) fn absorb(&mut self, update: &TerminalRenderUpdate) {
        self.cols = update.cols;
        self.rows = update.rows;
        self.viewport_kind = update.viewport_kind;
        self.scrollback_offset_rows = update.scrollback_offset_rows;
        self.scrollbar = update.scrollbar;
        self.terminal_modes = update.terminal_modes;
        self.cursor = update.cursor;
        self.render_generation = update.render_generation;
    }

    pub(crate) fn clean_update(&self) -> TerminalRenderUpdate {
        TerminalRenderUpdate {
            cols: self.cols,
            rows: self.rows,
            geometry: Default::default(),
            viewport_kind: self.viewport_kind,
            scrollback_offset_rows: self.scrollback_offset_rows,
            scrollbar: self.scrollbar,
            terminal_modes: self.terminal_modes,
            render_generation: self.render_generation,
            cursor: self.cursor,
            dirty: DirtyState::Clean,
            ops: Vec::new(),
            image_resources: Vec::new(),
            image_placements: Vec::new(),
        }
    }
}

/// Per-session-channel state shared between the reader thread and the FFI
/// caller.
pub(crate) struct ChannelSlot {
    pub session_id: String,
    /// Latest un-consumed render packet. The ack-gated protocol guarantees at
    /// most one arrives before we ack, and we ack on consumption.
    pub pending: Option<TerminalRenderUpdate>,
    pub last: LastKnown,
    /// Size the caller wants; re-asserted after every reconnect.
    pub desired_cols: u16,
    pub desired_rows: u16,
    /// Set when the daemon reported an error for this channel (e.g. the
    /// session went away).
    pub closed: Option<String>,
}

impl ChannelSlot {
    pub(crate) fn dirty(&self) -> DirtyState {
        match &self.pending {
            None => DirtyState::Clean,
            Some(update) => update_dirty_state(update),
        }
    }
}

fn update_dirty_state(update: &TerminalRenderUpdate) -> DirtyState {
    let full = update.dirty == DirtyState::Full
        || update.ops.iter().any(|op| op.kind == TerminalRenderUpdateOpKind::FullVisibleReplace)
        || update.ops.iter().any(|op| op.kind == TerminalRenderUpdateOpKind::ScrollCopy);
    if full {
        DirtyState::Full
    } else {
        DirtyState::Partial
    }
}

#[derive(Default)]
pub(crate) struct DirectoryState {
    pub entries: HashMap<String, DirectoryEntry>,
    /// Bumped on every snapshot or delta application; FFI callers poll this to
    /// notice changes and re-read the whole (small) list.
    pub generation: u64,
    /// True once the first snapshot has been received.
    pub populated: bool,
}

impl DirectoryState {
    fn replace(&mut self, snapshot: DirectorySnapshot) {
        self.entries = snapshot.sessions.into_iter().map(|entry| (entry.session_id.clone(), entry)).collect();
        self.generation = self.generation.wrapping_add(1);
        self.populated = true;
    }

    fn apply_delta(&mut self, delta: DirectoryDelta) {
        for entry in delta.upserted {
            self.entries.insert(entry.session_id.clone(), entry);
        }
        for session_id in delta.removed_session_ids {
            self.entries.remove(&session_id);
        }
        self.generation = self.generation.wrapping_add(1);
    }
}

struct ConnectionState {
    connected: bool,
    next_channel: u32,
    channels: HashMap<u32, Arc<Mutex<ChannelSlot>>>,
    directory: DirectoryState,
    queued_input: Vec<PacketFrame>,
}

/// One multiplexed packet connection to a named daemon, shared by every
/// session the provider opens against it.
pub(crate) struct DaemonConnection {
    layout: RuntimeLayout,
    selectors: Vec<String>,
    wake: WakeFn,
    stop: AtomicBool,
    /// Interrupts the reconnect backoff sleep; set when a caller has reason to
    /// believe the daemon just became reachable (e.g. it spawned it).
    retry_now: AtomicBool,
    writer: Mutex<Option<SessionStream>>,
    state: Mutex<ConnectionState>,
    reader: Mutex<Option<JoinHandle<()>>>,
}

impl DaemonConnection {
    pub(crate) fn open(layout: RuntimeLayout, selectors: Vec<String>, wake: WakeFn) -> Arc<Self> {
        let connection = Arc::new(Self {
            layout,
            selectors,
            wake,
            stop: AtomicBool::new(false),
            retry_now: AtomicBool::new(false),
            writer: Mutex::new(None),
            state: Mutex::new(ConnectionState {
                connected: false,
                next_channel: 1,
                channels: HashMap::new(),
                directory: DirectoryState::default(),
                queued_input: Vec::new(),
            }),
            reader: Mutex::new(None),
        });
        let thread_connection = Arc::clone(&connection);
        let handle = std::thread::Builder::new()
            .name(format!("cleat-daemon-io:{}", thread_connection.layout.daemon_name()))
            .spawn(move || thread_connection.reader_loop())
            .expect("spawn daemon connection reader thread");
        *connection.reader.lock().expect("reader handle lock") = Some(handle);
        connection
    }

    pub(crate) fn layout(&self) -> &RuntimeLayout {
        &self.layout
    }

    pub(crate) fn is_connected(&self) -> bool {
        self.state.lock().map(|state| state.connected).unwrap_or(false)
    }

    pub(crate) fn with_directory<R>(&self, read: impl FnOnce(&DirectoryState) -> R) -> R {
        let state = self.state.lock().expect("connection state lock");
        read(&state.directory)
    }

    /// Register a session channel. The open frame is sent immediately when
    /// connected; otherwise (and after every reconnect) the reader thread
    /// replays it, and the daemon answers with a full render packet.
    pub(crate) fn open_session_channel(&self, session_id: String, cols: u16, rows: u16) -> (u32, Arc<Mutex<ChannelSlot>>) {
        let slot = Arc::new(Mutex::new(ChannelSlot {
            session_id: session_id.clone(),
            pending: None,
            last: LastKnown::new(cols, rows),
            desired_cols: cols,
            desired_rows: rows,
            closed: None,
        }));
        let (channel, connected) = {
            let mut state = self.state.lock().expect("connection state lock");
            let channel = state.next_channel;
            state.next_channel = state.next_channel.wrapping_add(1).max(1);
            state.channels.insert(channel, Arc::clone(&slot));
            (channel, state.connected)
        };
        if connected {
            let _ = self.send_open_frame(channel, &session_id, cols, rows);
        } else {
            // The caller may have just spawned the daemon (session create);
            // cut the reconnect backoff short.
            self.retry_now.store(true, Ordering::SeqCst);
        }
        (channel, slot)
    }

    pub(crate) fn close_session_channel(&self, channel: u32) {
        let removed = {
            let mut state = self.state.lock().expect("connection state lock");
            state.channels.remove(&channel).is_some()
        };
        if removed {
            let _ = self.send_frame_result(PacketFrame::new(
                CHANNEL_CONTROL,
                crate::packet::MSG_CONTROL_CLOSE_CHANNEL,
                &crate::packet::CloseChannel { channel, reason: None },
            ));
        }
    }

    pub(crate) fn send_input(&self, channel: u32, event: TerminalInputEvent) -> Result<(), String> {
        let frame = PacketFrame::new(channel, MSG_SESSION_INPUT, &crate::packet::Input { event })
            .map_err(|err| format!("encode packet frame: {err}"))?;
        {
            // Queue while disconnected: the reader flushes the queue right
            // after it re-opens the session channels, so input typed across a
            // brief reconnect is not lost.
            let mut state = self.state.lock().expect("connection state lock");
            if !state.connected {
                if state.queued_input.len() >= MAX_QUEUED_INPUT_FRAMES {
                    return Err("daemon connection is down and the input queue is full".to_string());
                }
                state.queued_input.push(frame);
                return Ok(());
            }
        }
        self.send_frame_result(Ok(frame))
    }

    pub(crate) fn send_resize(&self, channel: u32, cols: u16, rows: u16) -> Result<(), String> {
        self.send_frame_result(PacketFrame::new(channel, MSG_SESSION_RESIZE, &crate::packet::Resize { cols, rows }))
    }

    pub(crate) fn send_ack(&self, channel: u32, generation: u64) -> Result<(), String> {
        self.send_frame_result(PacketFrame::new(channel, MSG_SESSION_ACK, &crate::packet::Ack { generation }))
    }

    fn send_open_frame(&self, channel: u32, session_id: &str, cols: u16, rows: u16) -> Result<(), String> {
        self.send_frame_result(PacketFrame::new(CHANNEL_CONTROL, MSG_CONTROL_OPEN_CHANNEL, &OpenChannel {
            channel,
            session_id: session_id.to_string(),
        }))?;
        // The daemon does not resize on channel open; assert the size this
        // client wants so a session created detached comes up at view size.
        self.send_resize(channel, cols, rows)
    }

    fn send_frame_result(&self, frame: std::io::Result<PacketFrame>) -> Result<(), String> {
        let frame = frame.map_err(|err| format!("encode packet frame: {err}"))?;
        let mut writer = self.writer.lock().expect("connection writer lock");
        let Some(stream) = writer.as_mut() else {
            return Err("daemon connection is down".to_string());
        };
        let mut encoded = Vec::new();
        frame.write(&mut encoded).map_err(|err| format!("encode packet frame: {err}"))?;
        match stream.write_all(&encoded) {
            Ok(()) => Ok(()),
            Err(err) => {
                // The reader thread notices the broken stream and reconnects;
                // drop our writer clone so later sends fail fast.
                *writer = None;
                Err(format!("write packet frame: {err}"))
            }
        }
    }

    pub(crate) fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(mut writer) = self.writer.lock() {
            if let Some(stream) = writer.take() {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        }
        let handle = self.reader.lock().ok().and_then(|mut reader| reader.take());
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }

    fn reader_loop(self: Arc<Self>) {
        let mut backoff = RECONNECT_BACKOFF_START;
        while !self.stop.load(Ordering::SeqCst) {
            match connect_packet_stream(&self.layout, &self.selectors) {
                Ok((mut stream, snapshot)) => {
                    backoff = RECONNECT_BACKOFF_START;
                    if !self.install_connection(&stream, snapshot) {
                        continue;
                    }
                    self.read_until_disconnect(&mut stream);
                    self.teardown_connection();
                }
                Err(_) => {
                    self.sleep_backoff(backoff);
                    backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
                }
            }
        }
    }

    fn install_connection(&self, stream: &SessionStream, snapshot: DirectorySnapshot) -> bool {
        let writer_stream = match stream.try_clone() {
            Ok(writer_stream) => writer_stream,
            Err(_) => return false,
        };
        {
            let mut writer = self.writer.lock().expect("connection writer lock");
            *writer = Some(writer_stream);
        }
        let (reopen, queued_input) = {
            let mut state = self.state.lock().expect("connection state lock");
            state.connected = true;
            state.directory.replace(snapshot);
            let reopen: Vec<(u32, String, u16, u16)> = state
                .channels
                .iter()
                .map(|(channel, slot)| {
                    let slot = slot.lock().expect("channel slot lock");
                    (*channel, slot.session_id.clone(), slot.desired_cols, slot.desired_rows)
                })
                .collect();
            (reopen, std::mem::take(&mut state.queued_input))
        };
        for (channel, session_id, cols, rows) in reopen {
            let _ = self.send_open_frame(channel, &session_id, cols, rows);
        }
        for frame in queued_input {
            let _ = self.send_frame_result(Ok(frame));
        }
        (self.wake)();
        true
    }

    fn read_until_disconnect(&self, stream: &mut SessionStream) {
        loop {
            if self.stop.load(Ordering::SeqCst) {
                return;
            }
            match PacketFrame::read(stream) {
                Ok(frame) => self.dispatch_frame(frame),
                Err(_) => return,
            }
        }
    }

    fn dispatch_frame(&self, frame: PacketFrame) {
        match (frame.channel, frame.msg_type) {
            (CHANNEL_CONTROL, MSG_CONTROL_DIRECTORY_SNAPSHOT) => {
                if let Ok(snapshot) = frame.decode::<DirectorySnapshot>() {
                    let mut state = self.state.lock().expect("connection state lock");
                    state.directory.replace(snapshot);
                    drop(state);
                    (self.wake)();
                }
            }
            (CHANNEL_CONTROL, MSG_CONTROL_DIRECTORY_DELTA) => {
                if let Ok(delta) = frame.decode::<DirectoryDelta>() {
                    let mut state = self.state.lock().expect("connection state lock");
                    state.directory.apply_delta(delta);
                    drop(state);
                    (self.wake)();
                }
            }
            (CHANNEL_CONTROL, MSG_CONTROL_ERROR) => {
                if let Ok(error) = frame.decode::<ControlError>() {
                    let slot = {
                        let state = self.state.lock().expect("connection state lock");
                        state.channels.get(&error.channel).cloned()
                    };
                    if let Some(slot) = slot {
                        slot.lock().expect("channel slot lock").closed = Some(error.message);
                        (self.wake)();
                    }
                }
            }
            (channel, MSG_SESSION_RENDER) if channel != CHANNEL_CONTROL => {
                if let Ok(packet) = frame.decode::<RenderPacket>() {
                    let slot = {
                        let state = self.state.lock().expect("connection state lock");
                        state.channels.get(&channel).cloned()
                    };
                    if let Some(slot) = slot {
                        let mut slot = slot.lock().expect("channel slot lock");
                        slot.pending = Some(packet.update);
                        drop(slot);
                        (self.wake)();
                    }
                }
            }
            _ => {}
        }
    }

    fn teardown_connection(&self) {
        {
            let mut writer = self.writer.lock().expect("connection writer lock");
            *writer = None;
        }
        {
            let mut state = self.state.lock().expect("connection state lock");
            state.connected = false;
        }
        (self.wake)();
    }

    fn sleep_backoff(&self, backoff: Duration) {
        let mut remaining = backoff;
        while !self.stop.load(Ordering::SeqCst) && remaining > Duration::ZERO {
            if self.retry_now.swap(false, Ordering::SeqCst) {
                return;
            }
            let step = remaining.min(BACKOFF_POLL_STEP);
            std::thread::sleep(step);
            remaining = remaining.saturating_sub(step);
        }
    }
}

impl Drop for DaemonConnection {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Client half of the daemon-scoped `/connect` upgrade: HTTP upgrade dance,
/// protocol hello, initial directory snapshot.
pub(crate) fn connect_packet_stream(layout: &RuntimeLayout, selectors: &[String]) -> Result<(SessionStream, DirectorySnapshot), String> {
    let socket_path = layout.socket_path();
    let mut stream = try_connect_session_stream(&socket_path).map_err(|err| format!("connect {}: {err}", socket_path.display()))?;
    let body = serde_json::to_vec(&http_uds::DirectorySubscribeRequest { selectors: selectors.to_vec() })
        .map_err(|err| format!("serialize packet subscribe request: {err}"))?;
    let head = format!(
        "POST /connect HTTP/1.1\r\nHost: cleat\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: Upgrade\r\nUpgrade: cleat-packet/1\r\n\r\n",
        body.len()
    );
    stream.write_all(&[head.as_bytes(), &body].concat()).map_err(|err| format!("write packet upgrade request: {err}"))?;
    let response = http_uds::read_response_head(&mut stream).map_err(|err| format!("read packet upgrade response: {err}"))?;
    if response.status != StatusCode::SWITCHING_PROTOCOLS {
        return Err(format!("unexpected packet response: {}", response.status));
    }

    let hello = PacketFrame::read(&mut stream).map_err(|err| format!("read packet hello: {err}"))?;
    if hello.channel != CHANNEL_CONTROL || hello.msg_type != MSG_CONTROL_HELLO {
        return Err("packet stream did not start with control hello".to_string());
    }
    let hello = hello.decode::<ControlHello>().map_err(|err| format!("decode packet hello: {err}"))?;
    if hello.version != PROTOCOL_VERSION {
        return Err(format!("unsupported packet protocol version {}", hello.version));
    }

    let directory = PacketFrame::read(&mut stream).map_err(|err| format!("read packet directory: {err}"))?;
    if directory.channel != CHANNEL_CONTROL || directory.msg_type != MSG_CONTROL_DIRECTORY_SNAPSHOT {
        return Err("packet stream did not send a directory snapshot after hello".to_string());
    }
    let directory = directory.decode::<DirectorySnapshot>().map_err(|err| format!("decode packet directory: {err}"))?;
    Ok((stream, directory))
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        io::Read,
        os::unix::net::{UnixListener, UnixStream},
        sync::atomic::AtomicUsize,
        time::Instant,
    };

    use super::*;
    use crate::packet::{Ack, Input, Resize};

    fn test_layout() -> (tempfile::TempDir, RuntimeLayout) {
        let temp = tempfile::Builder::new().prefix("cleat-pd-").tempdir_in("/tmp").expect("tempdir");
        let layout = RuntimeLayout::new(temp.path().to_path_buf());
        layout.ensure_daemon_dirs().expect("daemon dirs");
        (temp, layout)
    }

    struct FakeDaemon {
        listener: UnixListener,
    }

    impl FakeDaemon {
        fn bind(layout: &RuntimeLayout) -> Self {
            let _ = std::fs::remove_file(layout.socket_path());
            Self { listener: UnixListener::bind(layout.socket_path()).expect("bind fake daemon socket") }
        }

        /// Accept a connection and drive the server half of the upgrade.
        fn accept(&self, sessions: Vec<DirectoryEntry>) -> UnixStream {
            let (mut stream, _) = self.listener.accept().expect("accept");
            // Consume the upgrade request: head, then Content-Length body.
            let mut buffer = Vec::new();
            let header_end = loop {
                let mut chunk = [0u8; 1024];
                let n = stream.read(&mut chunk).expect("read upgrade request");
                assert!(n > 0, "client hung up during upgrade");
                buffer.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                    break pos + 4;
                }
            };
            let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
            assert!(head.contains("Upgrade: cleat-packet/1"), "missing upgrade token in: {head}");
            let content_length: usize = head
                .lines()
                .find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").map(|value| value.trim().parse().unwrap()))
                .expect("content-length header");
            while buffer.len() < header_end + content_length {
                let mut chunk = [0u8; 1024];
                let n = stream.read(&mut chunk).expect("read upgrade body");
                assert!(n > 0);
                buffer.extend_from_slice(&chunk[..n]);
            }
            stream.write_all(b"HTTP/1.1 101 Switching Protocols\r\n\r\n").expect("write 101");
            PacketFrame::new(CHANNEL_CONTROL, MSG_CONTROL_HELLO, &ControlHello::current())
                .expect("hello frame")
                .write(&mut stream)
                .expect("write hello");
            PacketFrame::new(CHANNEL_CONTROL, MSG_CONTROL_DIRECTORY_SNAPSHOT, &DirectorySnapshot { sessions })
                .expect("snapshot frame")
                .write(&mut stream)
                .expect("write snapshot");
            stream
        }
    }

    fn directory_entry(session_id: &str) -> DirectoryEntry {
        DirectoryEntry {
            session_id: session_id.to_string(),
            tags: vec!["project=uishell".to_string()],
            state: "running".to_string(),
            controller_count: 0,
            watcher_count: 0,
            recreatable: false,
            cols: 80,
            rows: 24,
        }
    }

    fn render_update(generation: u64) -> TerminalRenderUpdate {
        TerminalRenderUpdate { render_generation: generation, cols: 80, rows: 24, dirty: DirtyState::Full, ..Default::default() }
    }

    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "condition not met within deadline");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn connection_opens_channels_streams_renders_and_acks_on_consumption() {
        let (_temp, layout) = test_layout();
        let daemon = FakeDaemon::bind(&layout);
        let wakes = Arc::new(AtomicUsize::new(0));
        let wake_count = Arc::clone(&wakes);
        let connection = DaemonConnection::open(
            layout,
            Vec::new(),
            Arc::new(move || {
                wake_count.fetch_add(1, Ordering::SeqCst);
            }),
        );

        let mut server = daemon.accept(vec![directory_entry("alpha")]);
        wait_until(|| connection.is_connected());
        assert_eq!(connection.with_directory(|directory| directory.entries.len()), 1);

        let (channel, slot) = connection.open_session_channel("alpha".to_string(), 100, 40);
        let open = PacketFrame::read(&mut server).expect("open frame");
        assert_eq!((open.channel, open.msg_type), (CHANNEL_CONTROL, MSG_CONTROL_OPEN_CHANNEL));
        assert_eq!(open.decode::<OpenChannel>().expect("open payload"), OpenChannel { channel, session_id: "alpha".to_string() });
        let resize = PacketFrame::read(&mut server).expect("resize frame");
        assert_eq!((resize.channel, resize.msg_type), (channel, MSG_SESSION_RESIZE));
        assert_eq!(resize.decode::<Resize>().expect("resize payload"), Resize { cols: 100, rows: 40 });

        PacketFrame::new(channel, MSG_SESSION_RENDER, &RenderPacket { update: render_update(7) })
            .expect("render frame")
            .write(&mut server)
            .expect("write render");
        wait_until(|| slot.lock().expect("slot").pending.is_some());
        assert_eq!(slot.lock().expect("slot").dirty(), DirtyState::Full);

        // Consume like the FFI does: take pending, absorb, ack.
        let update = slot.lock().expect("slot").pending.take().expect("pending update");
        slot.lock().expect("slot").last.absorb(&update);
        connection.send_ack(channel, update.render_generation).expect("ack");
        let ack = PacketFrame::read(&mut server).expect("ack frame");
        assert_eq!((ack.channel, ack.msg_type), (channel, MSG_SESSION_ACK));
        assert_eq!(ack.decode::<Ack>().expect("ack payload"), Ack { generation: 7 });

        connection.shutdown();
    }

    #[test]
    fn reconnect_reopens_channels_reasserts_size_and_flushes_queued_input() {
        let (_temp, layout) = test_layout();
        let daemon = FakeDaemon::bind(&layout);
        let connection = DaemonConnection::open(layout, Vec::new(), Arc::new(|| {}));

        let mut server = daemon.accept(vec![directory_entry("alpha")]);
        wait_until(|| connection.is_connected());
        let (channel, slot) = connection.open_session_channel("alpha".to_string(), 80, 24);
        let _open = PacketFrame::read(&mut server).expect("open frame");
        let _resize = PacketFrame::read(&mut server).expect("resize frame");

        // Daemon connection drops (e.g. lid close); input typed while down is
        // queued, and a resize records the new desired size.
        drop(server);
        wait_until(|| !connection.is_connected());
        if let Ok(mut slot) = slot.lock() {
            slot.desired_cols = 120;
            slot.desired_rows = 50;
        }
        connection.send_input(channel, TerminalInputEvent::RawBytes(b"queued".to_vec())).expect("queue input while down");

        let mut server = daemon.accept(vec![directory_entry("alpha")]);
        wait_until(|| connection.is_connected());
        let open = PacketFrame::read(&mut server).expect("reopen frame");
        assert_eq!(open.decode::<OpenChannel>().expect("reopen payload"), OpenChannel { channel, session_id: "alpha".to_string() });
        let resize = PacketFrame::read(&mut server).expect("resize frame");
        assert_eq!(resize.decode::<Resize>().expect("resize payload"), Resize { cols: 120, rows: 50 });
        let input = PacketFrame::read(&mut server).expect("queued input frame");
        assert_eq!((input.channel, input.msg_type), (channel, MSG_SESSION_INPUT));
        assert_eq!(input.decode::<Input>().expect("input payload"), Input { event: TerminalInputEvent::RawBytes(b"queued".to_vec()) });

        connection.shutdown();
    }

    #[test]
    fn directory_deltas_apply_and_bump_generation() {
        let (_temp, layout) = test_layout();
        let daemon = FakeDaemon::bind(&layout);
        let connection = DaemonConnection::open(layout, Vec::new(), Arc::new(|| {}));

        let mut server = daemon.accept(vec![directory_entry("alpha")]);
        wait_until(|| connection.is_connected());
        let first_generation = connection.with_directory(|directory| directory.generation);

        let mut retagged = directory_entry("alpha");
        retagged.tags = vec!["project=uishell".to_string(), "purpose=test".to_string()];
        PacketFrame::new(CHANNEL_CONTROL, MSG_CONTROL_DIRECTORY_DELTA, &DirectoryDelta {
            upserted: vec![retagged, directory_entry("beta")],
            removed_session_ids: Vec::new(),
        })
        .expect("delta frame")
        .write(&mut server)
        .expect("write delta");

        wait_until(|| connection.with_directory(|directory| directory.generation > first_generation));
        connection.with_directory(|directory| {
            assert_eq!(directory.entries.len(), 2);
            assert_eq!(directory.entries["alpha"].tags.len(), 2);
        });

        PacketFrame::new(CHANNEL_CONTROL, MSG_CONTROL_DIRECTORY_DELTA, &DirectoryDelta {
            upserted: Vec::new(),
            removed_session_ids: vec!["alpha".to_string()],
        })
        .expect("remove delta")
        .write(&mut server)
        .expect("write remove delta");

        wait_until(|| connection.with_directory(|directory| !directory.entries.contains_key("alpha")));
        assert_eq!(connection.with_directory(|directory| directory.entries.len()), 1);

        connection.shutdown();
    }
}
