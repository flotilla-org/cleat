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
//!
//! The connection reaches its daemon over a [`Transport`]: the daemon socket
//! under a local runtime layout, an explicit Unix socket (a forwarded or
//! remote daemon), or a command whose stdio carries the packet stream, such
//! as `ssh host cleat proxy --daemon work`. A command transport is never
//! respawned: when it fails or exits, the disconnect and the command's reason
//! are reported through [`TransportStatus`] and the connection stays down.
//!
//! A channel whose session is transferred to another daemon is redirected:
//! the daemon sends the new address, then closes the channel. The connection
//! re-homes the channel's slot on a follower connection to the new daemon,
//! reopening it with the role it last held, and routes the caller's
//! operations there. The caller's channel id and slot stay valid.

use std::{
    collections::{BTreeMap, HashMap},
    io::Write,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use http::StatusCode;

use crate::{
    http_uds,
    packet::{
        ChannelRedirect, ChannelRole, ControlError, ControlHello, DirectoryDelta, DirectoryEntry, DirectorySnapshot, OpenChannel,
        PacketFrame, RenderPacket, SessionRedirect, CHANNEL_CONTROL, MSG_CONTROL_CLOSE_CHANNEL, MSG_CONTROL_DIRECTORY_DELTA,
        MSG_CONTROL_DIRECTORY_SNAPSHOT, MSG_CONTROL_ERROR, MSG_CONTROL_HELLO, MSG_CONTROL_OPEN_CHANNEL, MSG_CONTROL_REDIRECT,
        MSG_SESSION_ACK, MSG_SESSION_INPUT, MSG_SESSION_RENDER, MSG_SESSION_RESIZE, MSG_SESSION_ROLE, PROTOCOL_VERSION,
    },
    platform::ipc::{shutdown_stream, try_connect_session_stream, SessionStream},
    protocol::AttachmentIdentity,
    provider::{
        DirtyState, TerminalCursor, TerminalInputEvent, TerminalRenderUpdate, TerminalRenderUpdateOpKind, TerminalResizeEvent,
        TerminalScrollbarState, TerminalViewportKind,
    },
    runtime::RuntimeLayout,
    vt,
};

pub(crate) type WakeFn = Arc<dyn Fn() + Send + Sync + 'static>;

const RECONNECT_BACKOFF_START: Duration = Duration::from_millis(100);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(2);
const BACKOFF_POLL_STEP: Duration = Duration::from_millis(10);
/// A panic in one client callback must not turn every later provider call into
/// another panic. The protected values use exception-safe standard collection
/// operations, so retain their state and let the normal disconnected paths
/// report transport failures.
fn recover_lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

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
    pub virtual_placements: Vec<crate::provider::TerminalVirtualPlacement>,
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
            virtual_placements: Vec::new(),
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
        self.virtual_placements = update.virtual_placements.clone();
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
            virtual_placements: self.virtual_placements.clone(),
        }
    }
}

/// Per-session-channel (attachment) state shared between the reader thread
/// and the FFI caller. An attachment holds one or more views; the default
/// view opens with the channel.
///
/// Lock order: the connection-state mutex is always taken BEFORE a channel
/// slot (`install_connection`, `request_role`). Never call a connection
/// method that takes the state lock while holding a slot guard.
pub(crate) struct ChannelSlot {
    pub session_id: String,
    pub clipboard: crate::clipboard::ClipboardQueue,
    pub clipboard_upstream_dropped: u64,
    images: crate::image_delivery::ImageReceiver,
    pub identity: AttachmentIdentity,
    /// Set when the daemon reported an error for this channel (e.g. the
    /// session went away).
    pub closed: Option<String>,
    /// The current connection speaks the views protocol: per-view messages
    /// are wrapped and live views share the attachment's live stream.
    views_protocol: bool,
    /// Views protocol: the latest live render nobody has consumed yet. The
    /// first live view to consume it acks it.
    live_pending: Option<crate::image_delivery::RenderBundle>,
    /// Views protocol: the attachment's live frame, so a view that is behind
    /// (or new) catches up with one coalesced update.
    live: crate::render_cache::PacketRenderCache,
    pub views: BTreeMap<u32, ViewSlot>,
    next_view: u32,
}

/// One view of an attachment: its own role, size, history position and
/// presented frame.
pub(crate) struct ViewSlot {
    /// Latest un-consumed render addressed to this view: a legacy channel's
    /// render, or a history frame. The ack-gated protocol guarantees at most
    /// one arrives before we ack, and we ack on consumption.
    pub pending: Option<TerminalRenderUpdate>,
    pub pending_images: Vec<crate::image_delivery::Image>,
    pub pending_links: Vec<crate::provider::TerminalViewLink>,
    pub view_state: crate::provider::ViewState,
    pub role_state: Option<crate::packet::RoleState>,
    pub last: LastKnown,
    /// Size the caller wants; re-asserted after every reconnect.
    pub desired_geometry: TerminalResizeEvent,
    /// Role the caller wants; requested on every (re)open with take=false —
    /// control lost across a disconnect is re-taken explicitly, not silently.
    pub desired_role: ChannelRole,
    /// Role the daemon granted (None until the first RoleState arrives).
    pub granted_role: Option<ChannelRole>,
    /// Views protocol: showing a history frame rather than the live stream.
    history: bool,
    /// Views protocol: the live frame generation this view last presented.
    live_generation: u64,
    /// Why this view cannot be served, e.g. the daemon predates views.
    pub closed: Option<String>,
}

impl ViewSlot {
    fn new(geometry: TerminalResizeEvent, role: ChannelRole) -> Self {
        Self {
            pending: None,
            pending_images: Vec::new(),
            pending_links: Vec::new(),
            view_state: Default::default(),
            role_state: None,
            last: LastKnown::new(geometry.cols, geometry.rows),
            desired_geometry: geometry,
            desired_role: role,
            granted_role: None,
            history: false,
            live_generation: 0,
            closed: None,
        }
    }

    /// The grant and anything in flight died with the connection or host.
    fn reset(&mut self) {
        self.granted_role = None;
        self.pending = None;
        self.pending_images.clear();
        self.pending_links.clear();
        self.history = false;
        self.live_generation = 0;
    }
}

/// Which render credit a consumed render releases.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RenderAck {
    /// The channel's unwrapped stream: a views attachment's live stream, or a
    /// legacy channel's one stream.
    Channel(u64),
    /// A views-protocol view's history frame.
    View(u32, u64),
}

/// A render taken for presentation by one view.
pub(crate) struct ConsumedRender {
    pub update: TerminalRenderUpdate,
    pub images: Vec<crate::image_delivery::Image>,
    pub links: Vec<crate::provider::TerminalViewLink>,
    pub ack: Option<RenderAck>,
}

impl ChannelSlot {
    fn new(session_id: String, identity: AttachmentIdentity, geometry: TerminalResizeEvent, role: ChannelRole) -> Self {
        Self {
            session_id,
            clipboard: Default::default(),
            clipboard_upstream_dropped: 0,
            images: Default::default(),
            identity,
            closed: None,
            views_protocol: false,
            live_pending: None,
            live: Default::default(),
            views: BTreeMap::from([(crate::packet::DEFAULT_VIEW, ViewSlot::new(geometry, role))]),
            next_view: crate::packet::DEFAULT_VIEW + 1,
        }
    }

    pub(crate) fn view(&self, view: u32) -> Option<&ViewSlot> {
        self.views.get(&view)
    }

    pub(crate) fn view_mut(&mut self, view: u32) -> Option<&mut ViewSlot> {
        self.views.get_mut(&view)
    }

    pub(crate) fn has_controller(&self) -> bool {
        self.views.values().any(|view| view.granted_role == Some(ChannelRole::Controller))
    }

    /// A view's reason it cannot be served: its own, or the attachment's.
    pub(crate) fn view_closed(&self, view: u32) -> Option<&str> {
        self.closed.as_deref().or_else(|| self.views.get(&view).map_or(Some("view is closed"), |view| view.closed.as_deref()))
    }

    /// Whether `view` has presented, or can present, a frame.
    pub(crate) fn view_streaming(&self, view: u32) -> bool {
        let Some(slot) = self.views.get(&view) else {
            return false;
        };
        slot.pending.is_some()
            || slot.last.render_generation > 0
            || (self.views_protocol && !slot.history && (self.live_pending.is_some() || self.live.latest_generation().is_some()))
    }

    pub(crate) fn dirty(&self, view: u32) -> DirtyState {
        let Some(slot) = self.views.get(&view) else {
            return DirtyState::Clean;
        };
        if let Some(update) = &slot.pending {
            return update_dirty_state(update);
        }
        if !self.views_protocol || slot.history {
            return DirtyState::Clean;
        }
        let cached = self.live.latest_generation();
        match &self.live_pending {
            Some(pending) if cached.is_none_or(|generation| generation == slot.live_generation) => {
                update_dirty_state(&pending.packet.update)
            }
            Some(_) => DirtyState::Full,
            None => match cached {
                Some(generation) if generation > slot.live_generation => {
                    if self.live.full_since(slot.live_generation) {
                        DirtyState::Full
                    } else {
                        DirtyState::Partial
                    }
                }
                _ => DirtyState::Clean,
            },
        }
    }

    /// Take the next render for `view`, or None when it has nothing new. A
    /// view that is in step with the live stream gets the daemon's update as
    /// sent; one that is behind gets a coalesced catch-up from the live frame.
    pub(crate) fn consume(&mut self, view: u32) -> Option<ConsumedRender> {
        let views_protocol = self.views_protocol;
        let slot = self.views.get_mut(&view)?;
        if let Some(update) = slot.pending.take() {
            let ack =
                if views_protocol { RenderAck::View(view, update.render_generation) } else { RenderAck::Channel(update.render_generation) };
            let images = std::mem::take(&mut slot.pending_images);
            let links = std::mem::take(&mut slot.pending_links);
            return Some(slot.present(update, images, links, Some(ack), views_protocol));
        }
        if !views_protocol || slot.history {
            return None;
        }
        let mut ack = None;
        let mut in_step = None;
        if let Some(pending) = self.live_pending.take() {
            ack = Some(RenderAck::Channel(pending.packet.update.render_generation));
            if self.live.latest_generation().is_none_or(|generation| generation == slot.live_generation) {
                in_step = Some(pending.packet.update.clone());
            }
            self.live.store(pending);
        }
        let latest = self.live.latest_generation()?;
        let update = match in_step {
            Some(update) => update,
            None if latest > slot.live_generation => self.live.since(slot.live_generation)?.packet.update,
            None => return None,
        };
        slot.live_generation = latest;
        let bundle = self.live.latest().expect("live frame stored above");
        let (images, links) = (bundle.images.clone(), bundle.packet.links.clone());
        Some(slot.present(update, images, links, ack, true))
    }

    /// Forget everything delivered over a connection that is gone.
    fn reset_for_reopen(&mut self) {
        self.clipboard.clear();
        self.images = Default::default();
        self.live_pending = None;
        self.live = Default::default();
        for view in self.views.values_mut() {
            view.reset();
        }
    }
}

impl ViewSlot {
    fn present(
        &mut self,
        mut update: TerminalRenderUpdate,
        images: Vec<crate::image_delivery::Image>,
        links: Vec<crate::provider::TerminalViewLink>,
        ack: Option<RenderAck>,
        monotonic: bool,
    ) -> ConsumedRender {
        // A view switching between history frames and the live stream must
        // not see its render generation go backwards.
        if monotonic && self.last.render_generation > 0 {
            update.render_generation = update.render_generation.max(self.last.render_generation.saturating_add(1));
        }
        self.last.absorb(&update);
        ConsumedRender { update, images, links, ack }
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
    pub daemon: Option<crate::packet::DirectoryDaemon>,
    pub entries: HashMap<String, DirectoryEntry>,
    /// Bumped on every snapshot or delta application; FFI callers poll this to
    /// notice changes and re-read the whole (small) list.
    pub generation: u64,
    /// True once the first snapshot has been received.
    pub populated: bool,
}

impl DirectoryState {
    fn replace(&mut self, snapshot: DirectorySnapshot) {
        self.daemon = snapshot.daemon;
        self.entries = snapshot.sessions.into_iter().map(|entry| (entry.session_id.clone(), entry)).collect();
        self.generation = self.generation.wrapping_add(1);
        self.populated = true;
    }

    fn apply_delta(&mut self, delta: DirectoryDelta) {
        if let Some(daemon) = delta.daemon {
            self.daemon = Some(daemon);
        }
        for entry in delta.upserted {
            self.entries.insert(entry.session_id.clone(), entry);
        }
        for session_id in delta.removed_session_ids {
            self.entries.remove(&session_id);
        }
        self.generation = self.generation.wrapping_add(1);
    }
}

/// How a [`DaemonConnection`] reaches its daemon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Transport {
    /// The daemon socket under the connection's runtime layout. Reconnects
    /// with backoff and follows transferred sessions to other local daemons.
    Runtime,
    /// An explicit Unix socket for a forwarded or remote daemon. Reconnects
    /// with backoff; never discovers, starts or recovers a local daemon.
    UnixSocket(PathBuf),
    /// A command whose stdin and stdout carry the packet stream. It is
    /// spawned once; a failure or exit is reported, not retried.
    Command(Vec<String>),
}

impl Transport {
    /// A transport to a daemon this process cannot manage: sessions can be
    /// attached but not created, ended, transferred or followed.
    pub(crate) fn is_remote(&self) -> bool {
        !matches!(self, Transport::Runtime)
    }

    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Transport::Runtime => "runtime",
            Transport::UnixSocket(_) => "unix_socket",
            Transport::Command(_) => "command",
        }
    }
}

/// What the host can learn about the transport: whether it is up, why it is
/// not, and the daemon's output-admission warning from the latest connect.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub(crate) struct TransportStatus {
    pub kind: &'static str,
    pub connected: bool,
    /// Why the transport is down: the connect error, or the command's own
    /// reason (its last stderr line) when it exited.
    pub error: Option<String>,
    /// The daemon's output-admission warning, e.g. that cycle protection does
    /// not cover remote relationships. Never part of the rendered content.
    pub warning: Option<String>,
    /// False once a command transport has failed: it is not respawned.
    pub retrying: bool,
}

/// A spawned command transport and the stderr it has written so far.
struct CommandChild {
    child: std::process::Child,
    stderr: Arc<Mutex<String>>,
    stderr_reader: Option<JoinHandle<()>>,
}

/// Keep the end of a command's stderr: enough for its last reason line.
#[cfg(unix)]
const COMMAND_STDERR_LIMIT: usize = 4096;
/// How long a failed command gets to exit and finish its stderr before the
/// reason is read.
const COMMAND_EXIT_GRACE: Duration = Duration::from_secs(2);
/// How long a stopped command's stderr gets to drain. A descendant such as an
/// ssh ControlMaster can hold the pipe open indefinitely, so the reader is
/// detached after this rather than joined.
const COMMAND_STDERR_DRAIN: Duration = Duration::from_millis(200);

impl CommandChild {
    #[cfg(unix)]
    fn spawn(argv: &[String]) -> Result<(SessionStream, Self), String> {
        use std::{io::Read, os::fd::OwnedFd, process::Stdio};
        let (program, args) = argv.split_first().ok_or("command transport needs a program")?;
        let (ours, theirs) = SessionStream::pair().map_err(|err| format!("create command transport socket: {err}"))?;
        let stdin = OwnedFd::from(theirs.try_clone().map_err(|err| format!("create command transport socket: {err}"))?);
        let mut child = std::process::Command::new(program)
            .args(args)
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(OwnedFd::from(theirs)))
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| format!("spawn {program}: {err}"))?;
        let stderr = Arc::new(Mutex::new(String::new()));
        let mut pipe = child.stderr.take().expect("piped stderr");
        let sink = Arc::clone(&stderr);
        let stderr_reader = std::thread::Builder::new()
            .name("cleat-transport-stderr".into())
            .spawn(move || {
                let mut buffer = [0u8; 1024];
                while let Ok(read) = pipe.read(&mut buffer) {
                    if read == 0 {
                        break;
                    }
                    let mut text = recover_lock(&sink);
                    text.push_str(&String::from_utf8_lossy(&buffer[..read]));
                    if text.len() > COMMAND_STDERR_LIMIT {
                        let mut cut = text.len() - COMMAND_STDERR_LIMIT;
                        while !text.is_char_boundary(cut) {
                            cut += 1;
                        }
                        text.drain(..cut);
                    }
                }
            })
            .map_err(|err| format!("spawn command transport stderr reader: {err}"))?;
        Ok((ours, Self { child, stderr, stderr_reader: Some(stderr_reader) }))
    }

    #[cfg(not(unix))]
    fn spawn(_argv: &[String]) -> Result<(SessionStream, Self), String> {
        Err("command transports are unsupported on this platform".into())
    }

    /// Why the command stopped carrying the stream: its last stderr line, or
    /// its exit status. Waits briefly for it to exit on its own, then kills it.
    fn reason(&mut self) -> String {
        let deadline = Instant::now() + COMMAND_EXIT_GRACE;
        let status = loop {
            match self.child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) if Instant::now() < deadline => std::thread::sleep(BACKOFF_POLL_STEP),
                Ok(None) => break Err("transport command stopped carrying the stream".to_string()),
                Err(err) => break Err(format!("transport command status unavailable: {err}")),
            }
        };
        self.stop();
        let stderr = recover_lock(&self.stderr).clone();
        if let Some(line) = stderr.lines().rev().map(str::trim).find(|line| !line.is_empty()) {
            return line.to_string();
        }
        match status {
            Ok(status) => format!("transport command exited ({status})"),
            Err(reason) => reason,
        }
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.stderr_reader.take() {
            let deadline = Instant::now() + COMMAND_STDERR_DRAIN;
            while !reader.is_finished() && Instant::now() < deadline {
                std::thread::sleep(BACKOFF_POLL_STEP);
            }
            if reader.is_finished() {
                let _ = reader.join();
            }
        }
    }
}

struct ConnectionState {
    connected: bool,
    next_channel: u32,
    channels: HashMap<u32, Arc<Mutex<ChannelSlot>>>,
    directory: DirectoryState,
    /// Redirects received for channels whose close has not arrived yet.
    pending_redirects: HashMap<u32, SessionRedirect>,
    /// The current connection speaks the views protocol.
    views: bool,
}

fn views_unsupported() -> String {
    format!("the daemon predates views (protocol {}); open another session handle instead", crate::packet::VIEWS_PROTOCOL_VERSION)
}

/// Where a redirected channel lives now.
#[derive(Clone)]
struct ChannelRoute {
    connection: Arc<DaemonConnection>,
    channel: u32,
}

/// One multiplexed packet connection to a named daemon, shared by every
/// session the provider opens against it.
pub(crate) struct DaemonConnection {
    layout: RuntimeLayout,
    transport: Transport,
    status: Mutex<TransportStatus>,
    command: Mutex<Option<CommandChild>>,
    selectors: Vec<String>,
    wake: WakeFn,
    stop: AtomicBool,
    /// Interrupts the reconnect backoff sleep; set when a caller has reason to
    /// believe the daemon just became reachable (e.g. it spawned it).
    retry_now: AtomicBool,
    writer: Mutex<Option<SessionStream>>,
    state: Mutex<ConnectionState>,
    reader: Mutex<Option<JoinHandle<()>>>,
    /// Redirected channels, by the caller's channel id. Never held across a
    /// call into the routed connection.
    routes: Mutex<HashMap<u32, ChannelRoute>>,
    /// One connection per daemon that redirected channels moved to.
    followers: Mutex<HashMap<(std::path::PathBuf, String), Arc<DaemonConnection>>>,
}

impl DaemonConnection {
    pub(crate) fn open(layout: RuntimeLayout, selectors: Vec<String>, wake: WakeFn) -> Arc<Self> {
        Self::open_with_transport(layout, Transport::Runtime, selectors, wake)
    }

    /// Open a connection over `transport`. For a remote transport, `layout`
    /// only names the connection; nothing is read from or started under it.
    pub(crate) fn open_with_transport(layout: RuntimeLayout, transport: Transport, selectors: Vec<String>, wake: WakeFn) -> Arc<Self> {
        let status = TransportStatus { kind: transport.kind(), retrying: true, ..TransportStatus::default() };
        let connection = Arc::new(Self {
            layout,
            transport,
            status: Mutex::new(status),
            command: Mutex::new(None),
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
                pending_redirects: HashMap::new(),
                views: false,
            }),
            reader: Mutex::new(None),
            routes: Mutex::new(HashMap::new()),
            followers: Mutex::new(HashMap::new()),
        });
        let thread_connection = Arc::clone(&connection);
        let handle = std::thread::Builder::new()
            .name(format!("cleat-daemon-io:{}", thread_connection.layout.daemon_name()))
            .spawn(move || thread_connection.reader_loop())
            .expect("spawn daemon connection reader thread");
        *recover_lock(&connection.reader) = Some(handle);
        connection
    }

    pub(crate) fn layout(&self) -> &RuntimeLayout {
        &self.layout
    }

    pub(crate) fn transport(&self) -> &Transport {
        &self.transport
    }

    pub(crate) fn transport_status(&self) -> TransportStatus {
        recover_lock(&self.status).clone()
    }

    pub(crate) fn is_connected(&self) -> bool {
        self.state.lock().map(|state| state.connected).unwrap_or(false)
    }

    /// Connectivity of the daemon that currently hosts `channel`'s session.
    pub(crate) fn channel_connected(&self, channel: u32) -> bool {
        match self.route(channel) {
            Some(route) => route.connection.channel_connected(route.channel),
            None => self.is_connected(),
        }
    }

    pub(crate) fn channel_layout(&self, channel: u32) -> RuntimeLayout {
        match self.route(channel) {
            Some(route) => route.connection.channel_layout(route.channel),
            None => {
                let generation = self.with_directory(|directory| directory.daemon.as_ref().and_then(|daemon| daemon.generation));
                match generation {
                    Some(generation) => self
                        .layout
                        .clone()
                        .with_daemon(format!("{}@{generation}", self.layout.logical_name()))
                        .unwrap_or_else(|_| self.layout.clone()),
                    None => self.layout.resolved().unwrap_or_else(|_| self.layout.clone()),
                }
            }
        }
    }

    fn route(&self, channel: u32) -> Option<ChannelRoute> {
        recover_lock(&self.routes).get(&channel).cloned()
    }

    pub(crate) fn with_directory<R>(&self, read: impl FnOnce(&DirectoryState) -> R) -> R {
        let state = recover_lock(&self.state);
        read(&state.directory)
    }

    /// Register a session channel. The open frame is sent immediately when
    /// connected; otherwise (and after every reconnect) the reader thread
    /// replays it, and the daemon answers with a role grant + full render.
    pub(crate) fn open_session_channel(
        &self,
        session_id: String,
        geometry: TerminalResizeEvent,
        role: ChannelRole,
        identity: AttachmentIdentity,
    ) -> (u32, Arc<Mutex<ChannelSlot>>) {
        let slot = Arc::new(Mutex::new(ChannelSlot::new(session_id, identity, geometry, role)));
        let channel = self.register_slot(Arc::clone(&slot));
        (channel, slot)
    }

    /// Give `slot` a channel on this connection and open it (now, or on the
    /// next connect) with its views' desired geometry and roles.
    fn register_slot(&self, slot: Arc<Mutex<ChannelSlot>>) -> u32 {
        let (channel, connected) = {
            let mut state = recover_lock(&self.state);
            let channel = state.next_channel;
            state.next_channel = state.next_channel.wrapping_add(1).max(1);
            state.channels.insert(channel, Arc::clone(&slot));
            (channel, state.connected)
        };
        if connected {
            self.reopen_channel(channel, &slot);
        } else {
            // The caller may have just spawned the daemon (session create);
            // cut the reconnect backoff short.
            self.retry_now.store(true, Ordering::SeqCst);
        }
        channel
    }

    /// Follow a transferred session: re-home the channel's slot on the new
    /// daemon, re-requesting the roles its views last held.
    fn follow_redirect(&self, channel: u32, slot: Arc<Mutex<ChannelSlot>>, redirect: SessionRedirect) {
        if self.transport.is_remote() {
            recover_lock(&slot).closed =
                Some(format!("session {} moved to daemon {}; remote transports do not follow it", redirect.session_id, redirect.daemon));
            return;
        }
        if let Some(reason) = redirect.current_incompatibility() {
            recover_lock(&slot).closed = Some(reason);
            return;
        }
        let layout = match RuntimeLayout::new(redirect.runtime_root.clone().into()).with_daemon(redirect.daemon.clone()) {
            Ok(layout) => layout,
            Err(err) => {
                recover_lock(&slot).closed = Some(format!("session {} moved to an invalid address: {err}", redirect.session_id));
                return;
            }
        };
        {
            let mut slot = recover_lock(&slot);
            // The grant died with the old host; the role it held is re-requested.
            for view in slot.views.values_mut() {
                if let Some(role) = view.granted_role {
                    view.desired_role = role;
                }
            }
            slot.reset_for_reopen();
        }
        let follower = {
            let mut followers = recover_lock(&self.followers);
            let key = (layout.root().to_path_buf(), layout.daemon_name().to_string());
            Arc::clone(followers.entry(key).or_insert_with(|| DaemonConnection::open(layout, Vec::new(), Arc::clone(&self.wake))))
        };
        recover_lock(&self.state).channels.remove(&channel);
        let routed = follower.register_slot(slot);
        recover_lock(&self.routes).insert(channel, ChannelRoute { connection: follower, channel: routed });
    }

    /// Open another view on an attachment. The view is opened now when the
    /// daemon serves views, or on the next connect; on a daemon that predates
    /// views it reports why it is closed.
    pub(crate) fn open_view(&self, channel: u32, geometry: TerminalResizeEvent, role: ChannelRole) -> Option<u32> {
        if let Some(route) = self.route(channel) {
            return route.connection.open_view(route.channel, geometry, role);
        }
        let (slot, connected, views) = {
            let state = recover_lock(&self.state);
            (state.channels.get(&channel).cloned()?, state.connected, state.views)
        };
        let view = {
            let mut slot = recover_lock(&slot);
            if slot.closed.is_some() {
                return None;
            }
            let view = slot.next_view;
            slot.next_view = slot.next_view.checked_add(1)?;
            let mut view_slot = ViewSlot::new(geometry, role);
            if connected && !views {
                view_slot.closed = Some(views_unsupported());
            }
            slot.views.insert(view, view_slot);
            view
        };
        if connected && views {
            let _ = self.send_open_view(channel, view, geometry, role);
        }
        Some(view)
    }

    /// Close one view; the attachment and its other views stay open. The
    /// default view of a daemon that predates views cannot be closed.
    pub(crate) fn close_view(&self, channel: u32, view: u32) -> Result<(), String> {
        if let Some(route) = self.route(channel) {
            return route.connection.close_view(route.channel, view);
        }
        let (slot, connected, views) = {
            let state = recover_lock(&self.state);
            (state.channels.get(&channel).cloned().ok_or("session channel is closed")?, state.connected, state.views)
        };
        {
            let mut slot = recover_lock(&slot);
            if connected && !views && view == crate::packet::DEFAULT_VIEW {
                return Err(views_unsupported());
            }
            slot.views.remove(&view).ok_or("view is closed")?;
        }
        if connected && views {
            self.send_frame_result(PacketFrame::new(channel, crate::packet::MSG_SESSION_CLOSE_VIEW, &crate::packet::CloseView { view }))?;
        }
        Ok(())
    }

    /// Request a role change (take=true requests exclusive control and demotes
    /// the other controllers). The grant arrives asynchronously as a RoleState.
    pub(crate) fn request_role(&self, channel: u32, view: u32, role: ChannelRole, take: bool) -> Result<(), String> {
        if let Some(route) = self.route(channel) {
            return route.connection.request_role(route.channel, view, role, take);
        }
        {
            let state = recover_lock(&self.state);
            if let Some(slot) = state.channels.get(&channel) {
                let mut slot = recover_lock(slot);
                if let Some(reason) = slot.view_closed(view) {
                    return Err(reason.to_string());
                }
                if let Some(view) = slot.view_mut(view) {
                    view.desired_role = role;
                }
            }
        }
        self.send_view_frame(view, PacketFrame::new(channel, MSG_SESSION_ROLE, &crate::packet::RoleRequest { role, take }))
    }

    pub(crate) fn close_session_channel(&self, channel: u32) {
        let route = recover_lock(&self.routes).remove(&channel);
        if let Some(route) = route {
            return route.connection.close_session_channel(route.channel);
        }
        let removed = {
            let mut state = recover_lock(&self.state);
            state.channels.remove(&channel)
        };
        if let Some(slot) = removed {
            // Views opened on this attachment outlive nothing: they report closed.
            recover_lock(&slot).closed.get_or_insert_with(|| "session handle was destroyed".into());
            let _ = self.send_frame_result(PacketFrame::new(
                CHANNEL_CONTROL,
                crate::packet::MSG_CONTROL_CLOSE_CHANNEL,
                &crate::packet::CloseChannel { channel, reason: None },
            ));
        }
    }

    pub(crate) fn send_input(&self, channel: u32, view: u32, event: TerminalInputEvent) -> Result<(), String> {
        if let Some(route) = self.route(channel) {
            return route.connection.send_input(route.channel, view, event);
        }
        let state = recover_lock(&self.state);
        if !state.connected {
            return Err("daemon connection is down; input was not sent".into());
        }
        let slot = state.channels.get(&channel).ok_or("session channel is closed")?;
        let slot = recover_lock(slot);
        if let Some(reason) = slot.view_closed(view) {
            return Err(reason.to_string());
        }
        if !matches!(
            event,
            TerminalInputEvent::Focus(_)
                | TerminalInputEvent::Resize(_)
                | TerminalInputEvent::Mouse(crate::provider::TerminalMouseEvent {
                    kind: crate::provider::TerminalMouseEventKind::Wheel,
                    ..
                })
        ) && slot.view(view).and_then(|view| view.granted_role) != Some(ChannelRole::Controller)
        {
            return Err("session is not driving; input was not sent".into());
        }
        drop(slot);
        drop(state);
        self.send_view_frame(view, PacketFrame::new(channel, MSG_SESSION_INPUT, &crate::packet::Input { event }))
    }

    pub(crate) fn send_resize(&self, channel: u32, view: u32, cols: u16, rows: u16) -> Result<(), String> {
        if let Some(route) = self.route(channel) {
            return route.connection.send_resize(route.channel, view, cols, rows);
        }
        self.send_view_frame(view, PacketFrame::new(channel, MSG_SESSION_RESIZE, &crate::packet::Resize { cols, rows }))
    }

    pub(crate) fn size_policy(&self, channel: u32, fixed: Option<crate::packet::Resize>) -> Result<(), String> {
        if let Some(route) = self.route(channel) {
            return route.connection.size_policy(route.channel, fixed);
        }
        self.send_frame_result(PacketFrame::new(channel, crate::packet::MSG_SESSION_SIZE_POLICY, &fixed))
    }

    pub(crate) fn send_viewport(&self, channel: u32, view: u32, command: crate::provider::ViewportCommand) -> Result<(), String> {
        if let Some(route) = self.route(channel) {
            return route.connection.send_viewport(route.channel, view, command);
        }
        self.send_view_frame(view, PacketFrame::new(channel, crate::packet::MSG_SESSION_VIEWPORT, &crate::packet::Viewport { command }))
    }

    pub(crate) fn send_ack(&self, channel: u32, ack: RenderAck) -> Result<(), String> {
        if let Some(route) = self.route(channel) {
            return route.connection.send_ack(route.channel, ack);
        }
        match ack {
            RenderAck::Channel(generation) => {
                self.send_frame_result(PacketFrame::new(channel, MSG_SESSION_ACK, &crate::packet::Ack { generation }))
            }
            RenderAck::View(view, generation) => {
                self.send_view_frame(view, PacketFrame::new(channel, MSG_SESSION_ACK, &crate::packet::Ack { generation }))
            }
        }
    }

    /// Send a per-view message: wrapped for views, as-is for the default view
    /// of a daemon that predates them.
    fn send_view_frame(&self, view: u32, frame: std::io::Result<PacketFrame>) -> Result<(), String> {
        let views = recover_lock(&self.state).views;
        if views {
            let frame = frame.map_err(|err| format!("encode packet frame: {err}"))?;
            self.send_frame_result(crate::packet::ViewFrame::wrap(view, &frame))
        } else if view == crate::packet::DEFAULT_VIEW {
            self.send_frame_result(frame)
        } else {
            Err(views_unsupported())
        }
    }

    /// (Re)open a channel with its views. The default view opens with the
    /// channel; a closed default view opens as a watcher and closes at once.
    fn reopen_channel(&self, channel: u32, slot: &Arc<Mutex<ChannelSlot>>) {
        let views = recover_lock(&self.state).views;
        let (session_id, identity, default, others) = {
            let mut slot = recover_lock(slot);
            slot.views_protocol = views;
            let default = slot.view(crate::packet::DEFAULT_VIEW).map(|view| (view.desired_geometry, view.desired_role));
            let mut others = Vec::new();
            for (id, view) in slot.views.iter_mut().filter(|(id, _)| **id != crate::packet::DEFAULT_VIEW) {
                if views {
                    view.closed = None;
                    others.push((*id, view.desired_geometry, view.desired_role));
                } else {
                    view.closed = Some(views_unsupported());
                }
            }
            (slot.session_id.clone(), slot.identity.clone(), default, others)
        };
        let role = default.map_or(ChannelRole::Watcher, |(_, role)| role);
        let opened = self.send_frame_result(PacketFrame::new(CHANNEL_CONTROL, MSG_CONTROL_OPEN_CHANNEL, &OpenChannel {
            channel,
            session_id,
            role,
            take: false,
            identity,
        }));
        if opened.is_err() {
            return;
        }
        match default {
            // The daemon does not resize on channel open; assert the size this
            // client wants so a session created detached comes up at view size.
            // Watcher geometry is retained for promotion but does not size the PTY.
            // Use the existing input event to carry pixels without a protocol bump.
            Some((geometry, _)) => {
                let _ = self.send_view_frame(
                    crate::packet::DEFAULT_VIEW,
                    PacketFrame::new(channel, MSG_SESSION_INPUT, &crate::packet::Input { event: TerminalInputEvent::Resize(geometry) }),
                );
            }
            None if views => {
                let _ =
                    self.send_frame_result(PacketFrame::new(channel, crate::packet::MSG_SESSION_CLOSE_VIEW, &crate::packet::CloseView {
                        view: crate::packet::DEFAULT_VIEW,
                    }));
            }
            None => {}
        }
        for (view, geometry, role) in others {
            let _ = self.send_open_view(channel, view, geometry, role);
        }
    }

    fn send_open_view(&self, channel: u32, view: u32, geometry: TerminalResizeEvent, role: ChannelRole) -> Result<(), String> {
        self.send_frame_result(PacketFrame::new(channel, crate::packet::MSG_SESSION_OPEN_VIEW, &crate::packet::OpenView {
            view,
            role,
            take: false,
        }))?;
        self.send_view_frame(
            view,
            PacketFrame::new(channel, MSG_SESSION_INPUT, &crate::packet::Input { event: TerminalInputEvent::Resize(geometry) }),
        )
    }

    fn send_frame_result(&self, frame: std::io::Result<PacketFrame>) -> Result<(), String> {
        let frame = frame.map_err(|err| format!("encode packet frame: {err}"))?;
        let mut writer = recover_lock(&self.writer);
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
        let followers: Vec<_> = recover_lock(&self.followers).drain().map(|(_, follower)| follower).collect();
        for follower in followers {
            follower.shutdown();
        }
        self.stop.store(true, Ordering::SeqCst);
        if let Some(stream) = recover_lock(&self.writer).take() {
            shutdown_stream(&stream);
        }
        if let Some(command) = recover_lock(&self.command).as_mut() {
            // Unblocks a reader still waiting on the command's stream.
            let _ = command.child.kill();
        }
        let handle = recover_lock(&self.reader).take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
        if let Some(mut command) = recover_lock(&self.command).take() {
            command.stop();
        }
    }

    fn reader_loop(self: Arc<Self>) {
        let mut backoff = RECONNECT_BACKOFF_START;
        while !self.stop.load(Ordering::SeqCst) {
            match self.connect() {
                Ok(Connected { mut stream, directory, warning, views }) => {
                    backoff = RECONNECT_BACKOFF_START;
                    {
                        let mut status = recover_lock(&self.status);
                        status.error = None;
                        // Local hosts have always seen admission warnings on
                        // stderr; remote ones read them from the status.
                        if let (Transport::Runtime, Some(warning)) = (&self.transport, &warning) {
                            eprintln!("cleat: warning: {warning}");
                        }
                        status.warning = warning;
                    }
                    if !self.install_connection(&stream, directory, views) {
                        continue;
                    }
                    self.read_until_disconnect(&mut stream);
                    self.teardown_connection();
                    if let Transport::Command(_) = self.transport {
                        let reason = self.command_reason().unwrap_or_else(|| "transport command closed the stream".into());
                        self.give_up(reason);
                        return;
                    }
                }
                Err(err) => {
                    if let Transport::Command(_) = self.transport {
                        // The command's own explanation beats a generic EOF.
                        let reason = self.command_reason().unwrap_or(err);
                        self.give_up(reason);
                        return;
                    }
                    recover_lock(&self.status).error = Some(err);
                    self.sleep_backoff(backoff);
                    backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
                }
            }
        }
    }

    fn connect(&self) -> Result<Connected, String> {
        match &self.transport {
            Transport::Runtime => connect_declared(
                try_connect_session_stream(&self.layout.socket_path()),
                &self.layout.socket_path().display().to_string(),
                &self.selectors,
                &crate::output_admission::client_header()?,
            ),
            Transport::UnixSocket(path) => connect_declared(
                try_connect_session_stream(path),
                &path.display().to_string(),
                &self.selectors,
                &crate::output_admission::remote_client_header()?,
            ),
            Transport::Command(argv) => {
                let (stream, command) = CommandChild::spawn(argv)?;
                *recover_lock(&self.command) = Some(command);
                connect_declared(Ok(stream), &argv.join(" "), &self.selectors, &crate::output_admission::remote_client_header()?)
            }
        }
    }

    fn command_reason(&self) -> Option<String> {
        recover_lock(&self.command).take().map(|mut command| command.reason())
    }

    /// A command transport failed: report why and stay disconnected.
    fn give_up(&self, reason: String) {
        {
            let mut status = recover_lock(&self.status);
            status.error = Some(reason);
            status.retrying = false;
        }
        (self.wake)();
    }

    fn install_connection(&self, stream: &SessionStream, snapshot: DirectorySnapshot, views: bool) -> bool {
        let writer_stream = match stream.try_clone() {
            Ok(writer_stream) => writer_stream,
            Err(_) => return false,
        };
        {
            let mut writer = recover_lock(&self.writer);
            *writer = Some(writer_stream);
        }
        recover_lock(&self.status).connected = true;
        let reopen: Vec<(u32, Arc<Mutex<ChannelSlot>>)> = {
            let mut state = recover_lock(&self.state);
            state.connected = true;
            state.views = views;
            state.directory.replace(snapshot);
            state
                .channels
                .iter()
                .filter(|(_, slot)| {
                    let mut slot = recover_lock(slot);
                    // A closed channel (session gone) stays closed; reopening
                    // it would just re-error on every reconnect until the FFI
                    // caller destroys the session.
                    if slot.closed.is_some() {
                        return false;
                    }
                    // the old grants died with the connection
                    slot.reset_for_reopen();
                    true
                })
                .map(|(channel, slot)| (*channel, Arc::clone(slot)))
                .collect()
        };
        for (channel, slot) in reopen {
            self.reopen_channel(channel, &slot);
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

    /// Close the failed asset channel on both peers, releasing pending transfers.
    fn fail_image_channel(&self, channel: u32, message: String) {
        let slot = recover_lock(&self.state).channels.get(&channel).cloned();
        if let Some(slot) = slot {
            let mut slot = recover_lock(&slot);
            slot.closed = Some(message.clone());
            slot.reset_for_reopen();
        }
        let _ = self.send_frame_result(PacketFrame::new(CHANNEL_CONTROL, MSG_CONTROL_CLOSE_CHANNEL, &crate::packet::CloseChannel {
            channel,
            reason: Some(message),
        }));
        (self.wake)();
    }

    fn dispatch_frame(&self, frame: PacketFrame) {
        match (frame.channel, frame.msg_type) {
            (CHANNEL_CONTROL, MSG_CONTROL_DIRECTORY_SNAPSHOT) => {
                if let Ok(snapshot) = frame.decode::<DirectorySnapshot>() {
                    let mut state = recover_lock(&self.state);
                    state.directory.replace(snapshot);
                    drop(state);
                    (self.wake)();
                }
            }
            (CHANNEL_CONTROL, MSG_CONTROL_DIRECTORY_DELTA) => {
                if let Ok(delta) = frame.decode::<DirectoryDelta>() {
                    let mut state = recover_lock(&self.state);
                    state.directory.apply_delta(delta);
                    drop(state);
                    (self.wake)();
                }
            }
            (CHANNEL_CONTROL, MSG_CONTROL_REDIRECT) => {
                if let Ok(redirect) = frame.decode::<ChannelRedirect>() {
                    recover_lock(&self.state).pending_redirects.insert(redirect.channel, redirect.redirect);
                }
            }
            (CHANNEL_CONTROL, MSG_CONTROL_ERROR) => {
                if let Ok(error) = frame.decode::<ControlError>() {
                    let (slot, redirect) = {
                        let mut state = recover_lock(&self.state);
                        let redirect = state.pending_redirects.remove(&error.channel);
                        (state.channels.get(&error.channel).cloned(), redirect)
                    };
                    if let Some(slot) = slot {
                        match redirect {
                            Some(redirect) => self.follow_redirect(error.channel, slot, redirect),
                            None => {
                                let mut slot = recover_lock(&slot);
                                slot.clipboard.clear();
                                slot.closed = Some(error.message);
                            }
                        }
                        (self.wake)();
                    }
                }
            }
            (channel, crate::packet::MSG_SESSION_IMAGE_FILE) if channel != CHANNEL_CONTROL => {
                let result = (|| {
                    let file = frame.decode::<crate::packet::ImageFile>().map_err(|e| format!("invalid image file offer: {e}"))?;
                    let slot = recover_lock(&self.state).channels.get(&channel).cloned().ok_or("image offer for unknown channel")?;
                    let acquired = {
                        let mut slot = recover_lock(&slot);
                        if slot.closed.is_some() {
                            return Err("image offer for closed channel".into());
                        }
                        slot.images.file(&file)
                    };
                    self.send_frame_result(PacketFrame::new(
                        channel,
                        crate::packet::MSG_SESSION_IMAGE_FILE_RESULT,
                        &crate::packet::ImageFileResult { image_id: file.image_id, generation: file.generation, acquired },
                    ))
                })();
                if let Err(error) = result {
                    self.fail_image_channel(channel, error);
                }
            }
            (channel, crate::packet::MSG_SESSION_IMAGE) if channel != CHANNEL_CONTROL => {
                let result = (|| {
                    let chunk = frame.decode().map_err(|e| format!("invalid image chunk: {e}"))?;
                    let slot = recover_lock(&self.state).channels.get(&channel).cloned().ok_or("image chunk for unknown channel")?;
                    let mut slot = recover_lock(&slot);
                    if slot.closed.is_some() {
                        return Err("image chunk for closed channel".into());
                    }
                    slot.images.chunk(chunk)
                })();
                if let Err(error) = result {
                    self.fail_image_channel(channel, error);
                }
            }
            (channel, crate::packet::MSG_SESSION_CLIPBOARD_LOSS) if channel != CHANNEL_CONTROL => {
                if let Ok(loss) = frame.decode::<crate::packet::ClipboardLoss>() {
                    let slot = recover_lock(&self.state).channels.get(&channel).cloned();
                    if let Some(slot) = slot {
                        recover_lock(&slot).clipboard_upstream_dropped = loss.dropped;
                        (self.wake)();
                    }
                }
            }
            (channel, crate::packet::MSG_SESSION_CLIPBOARD) if channel != CHANNEL_CONTROL => {
                if let Ok(event) = frame.decode::<crate::clipboard::ClipboardEvent>() {
                    let slot = recover_lock(&self.state).channels.get(&channel).cloned();
                    if let Some(slot) = slot {
                        let mut slot = recover_lock(&slot);
                        // Clipboard effects belong to the attachment: one copy
                        // while any of its views drives.
                        if slot.closed.is_none() && slot.has_controller() {
                            slot.clipboard.push(event);
                            drop(slot);
                            (self.wake)();
                        }
                    }
                }
            }
            (channel, crate::packet::MSG_SESSION_VIEW) if channel != CHANNEL_CONTROL => {
                if let Ok(wrapped) = frame.decode::<crate::packet::ViewFrame>() {
                    let (view, frame) = wrapped.unwrap(channel);
                    self.dispatch_view_frame(view, frame, true);
                }
            }
            (channel, _) if channel != CHANNEL_CONTROL => self.dispatch_view_frame(crate::packet::DEFAULT_VIEW, frame, false),
            _ => {}
        }
    }

    /// Session-channel messages for one view. An unwrapped render on a views
    /// attachment is its shared live stream; everything else names a view.
    fn dispatch_view_frame(&self, view: u32, frame: PacketFrame, wrapped: bool) {
        let channel = frame.channel;
        let Some(slot) = recover_lock(&self.state).channels.get(&channel).cloned() else {
            return;
        };
        match frame.msg_type {
            MSG_SESSION_RENDER | crate::packet::MSG_SESSION_RENDER_VIRTUAL => {
                let Ok(packet) = RenderPacket::decode_frame(&frame) else {
                    return;
                };
                let mut slot = recover_lock(&slot);
                if slot.closed.is_some() {
                    return;
                }
                let images = match slot.images.commit(&packet.update.image_resources) {
                    Ok(images) => images,
                    Err(error) => {
                        drop(slot);
                        self.fail_image_channel(channel, error);
                        return;
                    }
                };
                if slot.views_protocol && !wrapped {
                    for view in slot.views.values_mut().filter(|view| !view.history) {
                        view.view_state = packet.view.clone();
                    }
                    slot.live_pending = Some(crate::image_delivery::RenderBundle { packet, images });
                } else if let Some(view) = slot.view_mut(view) {
                    view.history = wrapped && packet.view.status == crate::provider::ViewStatus::History;
                    view.pending_images = images;
                    view.pending_links = packet.links;
                    view.view_state = packet.view;
                    view.pending = Some(packet.update);
                }
                drop(slot);
                (self.wake)();
            }
            crate::packet::MSG_SESSION_CLOSE_VIEW => {
                if let Ok(close) = frame.decode::<crate::packet::CloseView>() {
                    if let Some(view) = recover_lock(&slot).view_mut(close.view) {
                        view.closed = Some("the daemon refused this view (an attachment holds at most 64 views)".into());
                        view.reset();
                    }
                    (self.wake)();
                }
            }
            crate::packet::MSG_SESSION_VIEW_STATE => {
                if let Ok(state) = frame.decode::<crate::provider::ViewState>() {
                    let mut slot = recover_lock(&slot);
                    if let Some(view) = slot.view_mut(view) {
                        // A views attachment sends a view back to its live
                        // stream with a live state: drop the history frame and
                        // catch up from the live frame.
                        if wrapped && state.status == crate::provider::ViewStatus::Live && view.history {
                            view.history = false;
                            view.live_generation = 0;
                            view.pending = None;
                            view.pending_images.clear();
                            view.pending_links.clear();
                        }
                        view.view_state = state;
                    }
                    drop(slot);
                    (self.wake)();
                }
            }
            MSG_SESSION_ROLE => {
                if let Ok(role_state) = frame.decode::<crate::packet::RoleState>() {
                    let mut slot = recover_lock(&slot);
                    if let Some(view) = slot.view_mut(view) {
                        view.granted_role = Some(role_state.role);
                        view.desired_role = role_state.role;
                        view.role_state = Some(role_state);
                    }
                    if !slot.has_controller() {
                        slot.clipboard.clear();
                    }
                    drop(slot);
                    (self.wake)();
                }
            }
            _ => {}
        }
    }

    fn teardown_connection(&self) {
        {
            let mut writer = recover_lock(&self.writer);
            *writer = None;
        }
        {
            let mut status = recover_lock(&self.status);
            status.connected = false;
            status.error = Some("connection closed".into());
        }
        {
            let mut state = recover_lock(&self.state);
            state.connected = false;
            for slot in state.channels.values() {
                let mut slot = recover_lock(slot);
                slot.clipboard.clear();
                for view in slot.views.values_mut() {
                    view.granted_role = None;
                }
            }
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

/// A packet stream after the upgrade, hello and first directory snapshot.
struct Connected {
    stream: SessionStream,
    directory: DirectorySnapshot,
    /// The daemon's output-admission warning, for the caller to surface.
    warning: Option<String>,
    /// The daemon accepted this client's views opt-in.
    views: bool,
}

/// Client half of the daemon-scoped `/connect` upgrade: HTTP upgrade dance,
/// protocol hello, initial directory snapshot.
pub(crate) fn connect_packet_stream(layout: &RuntimeLayout, selectors: &[String]) -> Result<(SessionStream, DirectorySnapshot), String> {
    connect_packet_endpoint(&layout.socket_path(), selectors, &crate::output_admission::client_header()?)
}

/// Connect without runtime discovery, session metadata, startup, or recovery.
/// A command-line client: the admission warning goes to stderr.
pub(crate) fn connect_packet_endpoint(
    socket_path: &std::path::Path,
    selectors: &[String],
    output_context: &str,
) -> Result<(SessionStream, DirectorySnapshot), String> {
    let connected = upgrade_packet_stream(
        try_connect_session_stream(socket_path).map_err(|err| format!("connect {}: {err}", socket_path.display()))?,
        selectors,
        output_context,
        false,
        false,
    )?;
    if let Some(warning) = &connected.warning {
        eprintln!("cleat: warning: {warning}");
    }
    Ok((connected.stream, connected.directory))
}

/// Provider connections declare virtual placements and views, and keep the
/// admission warning for [`TransportStatus`] instead of printing it.
fn connect_declared(
    stream: std::io::Result<SessionStream>,
    endpoint: &str,
    selectors: &[String],
    output_context: &str,
) -> Result<Connected, String> {
    let stream = stream.map_err(|err| format!("connect {endpoint}: {err}"))?;
    upgrade_packet_stream(stream, selectors, output_context, true, true)
}

fn upgrade_packet_stream(
    mut stream: SessionStream,
    selectors: &[String],
    output_context: &str,
    virtual_placements: bool,
    views: bool,
) -> Result<Connected, String> {
    let body = serde_json::to_vec(&http_uds::PacketSubscribeRequest {
        recognizer_roles: false,
        program_status: false,
        selectors: selectors.to_vec(),
        screen_activity_stable_ms: None,
        virtual_placements,
        views,
    })
    .map_err(|err| format!("serialize packet subscribe request: {err}"))?;
    let head = format!(
        "POST /connect HTTP/1.1\r\nHost: cleat\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: Upgrade\r\nUpgrade: cleat-packet/1\r\nx-cleat-output-context: {output_context}\r\n\r\n",
        body.len()
    );
    stream.write_all(&[head.as_bytes(), &body].concat()).map_err(|err| format!("write packet upgrade request: {err}"))?;
    let response = http_uds::read_response_head(&mut stream).map_err(|err| format!("read packet upgrade response: {err}"))?;
    if response.status != StatusCode::SWITCHING_PROTOCOLS {
        return Err(http_uds::upgrade_error(&mut stream, response.status));
    }
    let warning = response.output_warning.clone();

    let hello = PacketFrame::read(&mut stream).map_err(|err| format!("read packet hello: {err}"))?;
    if hello.channel != CHANNEL_CONTROL || hello.msg_type != MSG_CONTROL_HELLO {
        return Err("packet stream did not start with control hello".to_string());
    }
    let hello = hello.decode::<ControlHello>().map_err(|err| format!("decode packet hello: {err}"))?;
    if !hello.compatible_with_current() {
        return Err(format!(
            "incompatible packet protocol: daemon supports {}..={}, client speaks {}",
            hello.min_supported_version, hello.version, PROTOCOL_VERSION
        ));
    }

    let directory = PacketFrame::read(&mut stream).map_err(|err| format!("read packet directory: {err}"))?;
    if directory.channel != CHANNEL_CONTROL || directory.msg_type != MSG_CONTROL_DIRECTORY_SNAPSHOT {
        return Err("packet stream did not send a directory snapshot after hello".to_string());
    }
    let directory = directory.decode::<DirectorySnapshot>().map_err(|err| format!("decode packet directory: {err}"))?;
    let views = views && hello.version >= crate::packet::VIEWS_PROTOCOL_VERSION;
    Ok(Connected { stream, directory, warning, views })
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use std::{io::Read, sync::atomic::AtomicUsize, time::Instant};

    use super::*;
    use crate::{
        packet::{Ack, Input},
        platform::ipc::{bind_session_listener, SessionListener},
    };

    fn test_layout() -> (tempfile::TempDir, RuntimeLayout) {
        #[cfg(unix)]
        let temp = tempfile::Builder::new().prefix("cleat-pd-").tempdir_in("/tmp").expect("tempdir");
        #[cfg(windows)]
        let temp = tempfile::Builder::new().prefix("cleat-pd-").tempdir().expect("tempdir");
        let layout = RuntimeLayout::new(temp.path().to_path_buf());
        layout.ensure_daemon_dirs().expect("daemon dirs");
        (temp, layout)
    }

    struct FakeDaemon {
        listener: SessionListener,
        hello: ControlHello,
    }

    impl FakeDaemon {
        fn bind(layout: &RuntimeLayout) -> Self {
            let _ = std::fs::remove_file(layout.socket_path());
            // A daemon before views, unless a test asks for one with views.
            let hello = ControlHello {
                version: crate::packet::VIEWS_PROTOCOL_VERSION - 1,
                min_supported_version: crate::packet::MIN_SUPPORTED_PROTOCOL_VERSION,
            };
            Self { listener: bind_session_listener(&layout.socket_path()).expect("bind fake daemon socket"), hello }
        }

        /// Accept a connection and drive the server half of the upgrade.
        fn accept(&self, sessions: Vec<DirectoryEntry>) -> SessionStream {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match self.listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(err) => panic!("accept: {err}"),
                }
            };
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
            stream.write_all(b"HTTP/1.1 101 Switching Protocols\r\nx-cleat-output-admission: 1\r\n\r\n").expect("write 101");
            PacketFrame::new(CHANNEL_CONTROL, MSG_CONTROL_HELLO, &self.hello)
                .expect("hello frame")
                .write(&mut stream)
                .expect("write hello");
            PacketFrame::new(CHANNEL_CONTROL, MSG_CONTROL_DIRECTORY_SNAPSHOT, &DirectorySnapshot { daemon: None, sessions })
                .expect("snapshot frame")
                .write(&mut stream)
                .expect("write snapshot");
            stream
        }
    }

    fn geometry(cols: u16, rows: u16) -> TerminalResizeEvent {
        TerminalResizeEvent { cols, rows, cell_width_px: 9.0, cell_height_px: 18.0 }
    }

    fn directory_entry(session_id: &str) -> DirectoryEntry {
        DirectoryEntry {
            session_id: session_id.to_string(),
            tags: vec!["project=uishell".to_string()],
            state: "running".to_string(),
            controller_count: 0,
            watcher_count: 0,
            controller: None,
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
    fn shutdown_joins_reader_while_daemon_keeps_connection_open() {
        for _ in 0..20 {
            let (_temp, layout) = test_layout();
            let daemon = FakeDaemon::bind(&layout);
            let connection = DaemonConnection::open(layout, Vec::new(), Arc::new(|| {}));
            let server = daemon.accept(Vec::new());
            wait_until(|| connection.is_connected());
            let (done, completed) = std::sync::mpsc::channel();
            let shutdown = std::thread::spawn(move || {
                connection.shutdown();
                done.send(()).unwrap();
            });
            let result = completed.recv_timeout(Duration::from_secs(5));
            // Keep the peer alive until after the deadline; peer closure must
            // not be what wakes the connection's reader.
            drop(server);
            shutdown.join().expect("shutdown thread");
            result.expect("shutdown must wake and join the reader");
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

        let (channel, slot) =
            connection.open_session_channel("alpha".to_string(), geometry(100, 40), ChannelRole::Controller, AttachmentIdentity::default());
        let open = PacketFrame::read(&mut server).expect("open frame");
        assert_eq!((open.channel, open.msg_type), (CHANNEL_CONTROL, MSG_CONTROL_OPEN_CHANNEL));
        assert_eq!(open.decode::<OpenChannel>().expect("open payload"), OpenChannel {
            channel,
            session_id: "alpha".to_string(),
            role: ChannelRole::Controller,
            take: false,
            identity: AttachmentIdentity::default(),
        });
        let resize = PacketFrame::read(&mut server).expect("resize frame");
        assert_eq!((resize.channel, resize.msg_type), (channel, MSG_SESSION_INPUT));
        assert_eq!(resize.decode::<Input>().expect("resize payload").event, TerminalInputEvent::Resize(geometry(100, 40)));

        PacketFrame::new(channel, MSG_SESSION_RENDER, &RenderPacket::live(render_update(7)))
            .expect("render frame")
            .write(&mut server)
            .expect("write render");
        wait_until(|| slot.lock().expect("slot").view_mut(0).unwrap().pending.is_some());
        assert_eq!(slot.lock().expect("slot").dirty(0), DirtyState::Full);

        // Consume like the FFI does: take pending, absorb, ack.
        let update = slot.lock().expect("slot").view_mut(0).unwrap().pending.take().expect("pending update");
        slot.lock().expect("slot").view_mut(0).unwrap().last.absorb(&update);
        connection.send_ack(channel, RenderAck::Channel(update.render_generation)).expect("ack");
        let ack = PacketFrame::read(&mut server).expect("ack frame");
        assert_eq!((ack.channel, ack.msg_type), (channel, MSG_SESSION_ACK));
        assert_eq!(ack.decode::<Ack>().expect("ack payload"), Ack { generation: 7 });

        connection.send_viewport(channel, 0, crate::provider::ViewportCommand::DeltaRows(-12)).expect("viewport");
        let viewport = PacketFrame::read(&mut server).expect("viewport frame");
        assert_eq!((viewport.channel, viewport.msg_type), (channel, crate::packet::MSG_SESSION_VIEWPORT));
        assert_eq!(viewport.decode::<crate::packet::Viewport>().expect("viewport payload"), crate::packet::Viewport {
            command: crate::provider::ViewportCommand::DeltaRows(-12)
        });

        connection.shutdown();
    }

    // A daemon provider accepts both legacy renders and negotiated envelopes;
    // consuming an envelope retains its complete declaration set between polls.
    #[test]
    fn declaration_envelopes_reach_provider_observations_atomically() {
        let (_temp, layout) = test_layout();
        let daemon = FakeDaemon::bind(&layout);
        let connection = DaemonConnection::open(layout, Vec::new(), Arc::new(|| {}));
        let mut server = daemon.accept(vec![directory_entry("alpha")]);
        wait_until(|| connection.is_connected());
        let (channel, slot) =
            connection.open_session_channel("alpha".into(), geometry(80, 24), ChannelRole::Watcher, AttachmentIdentity::default());
        PacketFrame::read(&mut server).unwrap();
        PacketFrame::read(&mut server).unwrap();
        let declarations = vec![crate::provider::TerminalVirtualPlacement {
            handle: 1,
            creation_order: 1,
            image_id: 7,
            generation: 9,
            columns: 4,
            rows: 2,
            ..Default::default()
        }];
        let mut update = render_update(7);
        update.virtual_placements = declarations.clone();
        RenderPacket::live(update).frame(channel, true).unwrap().write(&mut server).unwrap();
        wait_until(|| recover_lock(&slot).view_mut(0).unwrap().pending.is_some());
        let observed = recover_lock(&slot).view_mut(0).unwrap().pending.take().unwrap();
        assert_eq!(observed.virtual_placements, declarations);
        recover_lock(&slot).view_mut(0).unwrap().last.absorb(&observed);
        assert_eq!(recover_lock(&slot).view_mut(0).unwrap().last.clean_update().virtual_placements, declarations);
        RenderPacket::live(render_update(8)).frame(channel, true).unwrap().write(&mut server).unwrap();
        wait_until(|| recover_lock(&slot).view_mut(0).unwrap().pending.is_some());
        assert!(recover_lock(&slot).view_mut(0).unwrap().pending.as_ref().unwrap().virtual_placements.is_empty());
        connection.shutdown();
    }

    #[test]
    fn image_dispatch_replies_or_closes_only_the_affected_channel() {
        use crate::packet::{
            CloseChannel, ImageChunk, ImageFile, ImageFileResult, MSG_SESSION_IMAGE, MSG_SESSION_IMAGE_FILE, MSG_SESSION_IMAGE_FILE_RESULT,
        };
        for invalid_type in [MSG_SESSION_IMAGE_FILE, MSG_SESSION_IMAGE] {
            let (_temp, layout) = test_layout();
            let daemon = FakeDaemon::bind(&layout);
            let connection = DaemonConnection::open(layout, Vec::new(), Arc::new(|| {}));
            let mut server = daemon.accept(vec![directory_entry("alpha")]);
            wait_until(|| connection.is_connected());
            let (channel, slot) =
                connection.open_session_channel("alpha".into(), geometry(80, 24), ChannelRole::Watcher, AttachmentIdentity::default());
            let _ = PacketFrame::read(&mut server).unwrap();
            let _ = PacketFrame::read(&mut server).unwrap();
            let (other, sibling) =
                connection.open_session_channel("alpha".into(), geometry(80, 24), ChannelRole::Watcher, AttachmentIdentity::default());
            let _ = PacketFrame::read(&mut server).unwrap();
            let _ = PacketFrame::read(&mut server).unwrap();
            PacketFrame::new(channel, MSG_SESSION_IMAGE_FILE, &ImageFile {
                image_id: 7,
                generation: 1,
                len: 3,
                path: "/missing-cleat-image".into(),
            })
            .unwrap()
            .write(&mut server)
            .unwrap();
            let response = PacketFrame::read(&mut server).unwrap();
            assert_eq!(response.msg_type, MSG_SESSION_IMAGE_FILE_RESULT);
            let response = response.decode::<ImageFileResult>().unwrap();
            assert_eq!((response.image_id, response.generation, response.acquired), (7, 1, false));
            PacketFrame::new(channel, MSG_SESSION_IMAGE, &ImageChunk {
                image_id: 7,
                generation: 1,
                total_len: 3,
                offset: 0,
                bytes: vec![1, 2, 3],
            })
            .unwrap()
            .write(&mut server)
            .unwrap();
            let mut update = render_update(1);
            update.image_resources.push(crate::provider::TerminalImageResource {
                image_id: 7,
                generation: 1,
                data_len: 3,
                ..Default::default()
            });
            PacketFrame::new(channel, MSG_SESSION_RENDER, &RenderPacket::live(update)).unwrap().write(&mut server).unwrap();
            wait_until(|| recover_lock(&slot).view_mut(0).unwrap().pending.is_some());
            assert_eq!(recover_lock(&slot).view_mut(0).unwrap().pending_images[0].bytes(), &[1, 2, 3]);
            PacketFrame { channel, msg_type: invalid_type, payload: Vec::new() }.write(&mut server).unwrap();
            let close = PacketFrame::read(&mut server).unwrap();
            assert_eq!((close.channel, close.msg_type), (CHANNEL_CONTROL, MSG_CONTROL_CLOSE_CHANNEL));
            assert_eq!(close.decode::<CloseChannel>().unwrap().channel, channel);
            {
                let mut slot = recover_lock(&slot);
                assert!(slot.closed.is_some());
                assert!(slot.view_mut(0).unwrap().pending.is_none());
                assert!(slot.view_mut(0).unwrap().pending_images.is_empty());
            }
            PacketFrame::new(other, MSG_SESSION_RENDER, &RenderPacket::live(render_update(2))).unwrap().write(&mut server).unwrap();
            wait_until(|| recover_lock(&sibling).view_mut(0).unwrap().pending.is_some());
            assert!(recover_lock(&sibling).closed.is_none());
            connection.close_session_channel(channel);
            let _ = PacketFrame::read(&mut server).unwrap();
            PacketFrame::new(channel, MSG_SESSION_IMAGE_FILE, &ImageFile {
                image_id: 7,
                generation: 2,
                len: 3,
                path: "/missing-cleat-image".into(),
            })
            .unwrap()
            .write(&mut server)
            .unwrap();
            assert_eq!(PacketFrame::read(&mut server).unwrap().msg_type, MSG_CONTROL_CLOSE_CHANNEL);
            connection.shutdown();
        }
    }

    #[test]
    fn reconnect_reopens_channels_and_rejects_offline_input() {
        let (_temp, layout) = test_layout();
        let daemon = FakeDaemon::bind(&layout);
        let connection = DaemonConnection::open(layout, Vec::new(), Arc::new(|| {}));

        let mut server = daemon.accept(vec![directory_entry("alpha")]);
        wait_until(|| connection.is_connected());
        let (channel, slot) =
            connection.open_session_channel("alpha".to_string(), geometry(80, 24), ChannelRole::Controller, AttachmentIdentity::default());
        let _open = PacketFrame::read(&mut server).expect("open frame");
        let _resize = PacketFrame::read(&mut server).expect("resize frame");

        // Daemon connection drops (e.g. lid close); input typed while down is
        // rejected, while a resize records the new desired size.
        drop(server);
        wait_until(|| !connection.is_connected());
        if let Ok(mut slot) = slot.lock() {
            slot.view_mut(0).unwrap().desired_geometry =
                TerminalResizeEvent { cell_width_px: 12.0, cell_height_px: 24.0, ..geometry(120, 50) };
            slot.view_mut(0).unwrap().pending = Some(render_update(99));
            slot.view_mut(0).unwrap().pending_images.push(crate::image_backing::RetainedImage::from_owned(
                crate::provider::TerminalImageBytes { image_id: 1, generation: 1, bytes: vec![1] },
            ));
            slot.view_mut(0).unwrap().pending_links.push(crate::provider::TerminalViewLink {
                col: 0,
                row: 0,
                uri: b"https://example.com".to_vec(),
            });
        }
        assert!(connection.send_input(channel, 0, TerminalInputEvent::RawBytes(b"discarded".to_vec())).is_err());

        let mut server = daemon.accept(vec![directory_entry("alpha")]);
        wait_until(|| connection.is_connected());
        let open = PacketFrame::read(&mut server).expect("reopen frame");
        assert_eq!(open.decode::<OpenChannel>().expect("reopen payload"), OpenChannel {
            channel,
            session_id: "alpha".to_string(),
            role: ChannelRole::Controller,
            take: false,
            identity: AttachmentIdentity::default(),
        });
        {
            let mut slot = recover_lock(&slot);
            assert!(slot.view_mut(0).unwrap().pending.is_none());
            assert!(slot.view_mut(0).unwrap().pending_images.is_empty());
            assert!(slot.view_mut(0).unwrap().pending_links.is_empty());
        }
        let resize = PacketFrame::read(&mut server).expect("resize frame");
        assert_eq!(
            resize.decode::<Input>().expect("resize payload").event,
            TerminalInputEvent::Resize(TerminalResizeEvent { cell_width_px: 12.0, cell_height_px: 24.0, ..geometry(120, 50) })
        );
        PacketFrame::new(channel, MSG_SESSION_ROLE, &crate::packet::RoleState {
            clipboard_writes: false,
            role: ChannelRole::Controller,
            controller: None,
            denial_reason: None,
            participants: Vec::new(),
            exclusive: None,
            fixed_size: None,
        })
        .unwrap()
        .write(&mut server)
        .unwrap();
        wait_until(|| recover_lock(&slot).view_mut(0).unwrap().granted_role == Some(ChannelRole::Controller));
        connection.send_input(channel, 0, TerminalInputEvent::RawBytes(b"fresh".to_vec())).unwrap();
        let input = PacketFrame::read(&mut server).expect("fresh input frame");
        assert_eq!((input.channel, input.msg_type), (channel, MSG_SESSION_INPUT));
        assert_eq!(input.decode::<Input>().expect("input payload"), Input { event: TerminalInputEvent::RawBytes(b"fresh".to_vec()) });

        connection.shutdown();
    }

    fn role_state(role: ChannelRole) -> crate::packet::RoleState {
        crate::packet::RoleState {
            clipboard_writes: false,
            role,
            controller: None,
            denial_reason: None,
            participants: Vec::new(),
            exclusive: None,
            fixed_size: None,
        }
    }

    fn redirect_to(layout: &RuntimeLayout, session_id: &str, protocol_version: u16) -> SessionRedirect {
        SessionRedirect {
            session_id: session_id.to_string(),
            address: format!("daemon:{}", layout.daemon_name()),
            runtime_root: layout.root().display().to_string(),
            daemon: layout.daemon_name().to_string(),
            hosting_epoch: 2,
            protocol_version,
            min_supported_version: protocol_version,
        }
    }

    fn send_redirect_and_close(server: &mut SessionStream, channel: u32, redirect: SessionRedirect) {
        PacketFrame::new(CHANNEL_CONTROL, MSG_CONTROL_REDIRECT, &ChannelRedirect { channel, redirect }).unwrap().write(server).unwrap();
        PacketFrame::new(CHANNEL_CONTROL, MSG_CONTROL_ERROR, &ControlError { channel, message: "session alpha moved".to_string() })
            .unwrap()
            .write(server)
            .unwrap();
    }

    #[test]
    fn redirected_channel_reopens_at_the_new_daemon_with_its_role() {
        let (_source_temp, source) = test_layout();
        let (_target_temp, target) = test_layout();
        let source_daemon = FakeDaemon::bind(&source);
        let target_daemon = FakeDaemon::bind(&target);
        let connection = DaemonConnection::open(source, Vec::new(), Arc::new(|| {}));
        let mut server = source_daemon.accept(vec![directory_entry("alpha")]);
        wait_until(|| connection.is_connected());
        let identity = AttachmentIdentity { kind: crate::protocol::AttachmentKind::Tool, name: "driver".into() };
        let (channel, slot) = connection.open_session_channel("alpha".into(), geometry(90, 30), ChannelRole::Controller, identity.clone());
        let _open = PacketFrame::read(&mut server).unwrap();
        let _resize = PacketFrame::read(&mut server).unwrap();
        PacketFrame::new(channel, MSG_SESSION_ROLE, &role_state(ChannelRole::Controller)).unwrap().write(&mut server).unwrap();
        wait_until(|| recover_lock(&slot).view_mut(0).unwrap().granted_role == Some(ChannelRole::Controller));

        send_redirect_and_close(&mut server, channel, redirect_to(&target, "alpha", PROTOCOL_VERSION));
        let mut moved = target_daemon.accept(vec![directory_entry("alpha")]);
        let open = PacketFrame::read(&mut moved).expect("reopen at the new daemon");
        let open = open.decode::<OpenChannel>().unwrap();
        assert_eq!((open.session_id.as_str(), open.role, open.take, open.identity), ("alpha", ChannelRole::Controller, false, identity));
        let resize = PacketFrame::read(&mut moved).unwrap();
        assert_eq!(resize.decode::<Input>().unwrap().event, TerminalInputEvent::Resize(geometry(90, 30)));
        PacketFrame::new(open.channel, MSG_SESSION_ROLE, &role_state(ChannelRole::Controller)).unwrap().write(&mut moved).unwrap();
        PacketFrame::new(open.channel, MSG_SESSION_RENDER, &RenderPacket::live(render_update(3))).unwrap().write(&mut moved).unwrap();
        wait_until(|| {
            let mut slot = recover_lock(&slot);
            slot.view_mut(0).unwrap().granted_role == Some(ChannelRole::Controller) && slot.view_mut(0).unwrap().pending.is_some()
        });
        assert!(recover_lock(&slot).closed.is_none());
        assert!(connection.channel_connected(channel));

        // The caller's channel id keeps working: input reaches the new host.
        connection.send_input(channel, 0, TerminalInputEvent::RawBytes(b"after".to_vec())).unwrap();
        let input = PacketFrame::read(&mut moved).unwrap();
        assert_eq!((input.channel, input.msg_type), (open.channel, MSG_SESSION_INPUT));
        assert_eq!(input.decode::<Input>().unwrap().event, TerminalInputEvent::RawBytes(b"after".to_vec()));
        connection.close_session_channel(channel);
        let close = PacketFrame::read(&mut moved).unwrap();
        assert_eq!(close.decode::<crate::packet::CloseChannel>().unwrap().channel, open.channel);
        connection.shutdown();
    }

    #[test]
    fn a_redirect_to_an_incompatible_protocol_closes_the_channel_with_the_reason() {
        let (_source_temp, source) = test_layout();
        let (_target_temp, target) = test_layout();
        let source_daemon = FakeDaemon::bind(&source);
        let connection = DaemonConnection::open(source, Vec::new(), Arc::new(|| {}));
        let mut server = source_daemon.accept(vec![directory_entry("alpha")]);
        wait_until(|| connection.is_connected());
        let (channel, slot) =
            connection.open_session_channel("alpha".into(), geometry(80, 24), ChannelRole::Watcher, AttachmentIdentity::default());
        let _open = PacketFrame::read(&mut server).unwrap();
        send_redirect_and_close(&mut server, channel, redirect_to(&target, "alpha", PROTOCOL_VERSION + 1));
        wait_until(|| recover_lock(&slot).closed.is_some());
        let reason = recover_lock(&slot).closed.clone().unwrap();
        assert!(reason.contains(&format!("speaking protocol {}", PROTOCOL_VERSION + 1)), "{reason}");
        connection.shutdown();
    }

    #[cfg(unix)] // This test uses a socket read timeout, which named pipes do not implement.
    #[test]
    fn reconnect_skips_closed_channels() {
        let (_temp, layout) = test_layout();
        let daemon = FakeDaemon::bind(&layout);
        let connection = DaemonConnection::open(layout, Vec::new(), Arc::new(|| {}));

        let mut server = daemon.accept(vec![directory_entry("alpha"), directory_entry("beta")]);
        wait_until(|| connection.is_connected());
        let (closed_channel, closed_slot) =
            connection.open_session_channel("alpha".to_string(), geometry(80, 24), ChannelRole::Controller, AttachmentIdentity::default());
        let (live_channel, _live_slot) =
            connection.open_session_channel("beta".to_string(), geometry(80, 24), ChannelRole::Controller, AttachmentIdentity::default());
        for _ in 0..4 {
            PacketFrame::read(&mut server).expect("initial open/resize frames");
        }

        // The daemon reports alpha's channel dead (e.g. the session exited).
        PacketFrame::new(CHANNEL_CONTROL, MSG_CONTROL_ERROR, &crate::packet::ControlError {
            channel: closed_channel,
            message: "session alpha exited".to_string(),
        })
        .expect("control error frame")
        .write(&mut server)
        .expect("write control error");
        wait_until(|| closed_slot.lock().expect("slot").closed.is_some());

        drop(server);
        wait_until(|| !connection.is_connected());

        // Only the live channel is reopened; a closed channel would just
        // re-error on every reconnect until the FFI caller destroys it.
        let mut server = daemon.accept(vec![directory_entry("beta")]);
        wait_until(|| connection.is_connected());
        let open = PacketFrame::read(&mut server).expect("reopen frame");
        assert_eq!(open.decode::<OpenChannel>().expect("reopen payload"), OpenChannel {
            channel: live_channel,
            session_id: "beta".to_string(),
            role: ChannelRole::Controller,
            take: false,
            identity: AttachmentIdentity::default(),
        });
        let resize = PacketFrame::read(&mut server).expect("resize frame");
        assert_eq!((resize.channel, resize.msg_type), (live_channel, MSG_SESSION_INPUT));
        server.set_read_timeout(Some(std::time::Duration::from_millis(100))).expect("set read timeout");
        assert!(PacketFrame::read(&mut server).is_err(), "closed channel must not be reopened");

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
            daemon: None,
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
            daemon: None,
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

    fn views_daemon(layout: &RuntimeLayout) -> FakeDaemon {
        FakeDaemon { hello: ControlHello::current(), ..FakeDaemon::bind(layout) }
    }

    /// The next frame, with the view it was wrapped for.
    fn read_view_frame(server: &mut SessionStream) -> (Option<u32>, PacketFrame) {
        let frame = PacketFrame::read(server).expect("frame");
        if frame.msg_type == crate::packet::MSG_SESSION_VIEW {
            let (view, inner) = frame.decode::<crate::packet::ViewFrame>().expect("view frame").unwrap(frame.channel);
            (Some(view), inner)
        } else {
            (None, frame)
        }
    }

    fn rows_update(generation: u64, full: bool, rows: &[(u16, &str)]) -> TerminalRenderUpdate {
        use crate::provider::{TerminalRenderCell, TerminalRenderRow, TerminalRenderUpdateOp};
        let rows: Vec<_> = rows
            .iter()
            .map(|(row, text)| TerminalRenderRow {
                row: *row,
                col_count: 80,
                cells: (0..80)
                    .map(|col| TerminalRenderCell {
                        graphemes: text.chars().nth(col).map(|ch| vec![ch as u32]).unwrap_or_default(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            })
            .collect();
        let ops = if full {
            vec![TerminalRenderUpdateOp {
                kind: TerminalRenderUpdateOpKind::FullVisibleReplace,
                row_count: 24,
                col_count: 80,
                rows,
                ..Default::default()
            }]
        } else {
            rows.into_iter()
                .map(|row| TerminalRenderUpdateOp {
                    kind: TerminalRenderUpdateOpKind::RowReplace,
                    first_row: row.row,
                    row_count: 1,
                    col_count: 80,
                    rows: vec![row],
                    ..Default::default()
                })
                .collect()
        };
        TerminalRenderUpdate {
            render_generation: generation,
            cols: 80,
            rows: 24,
            dirty: if full { DirtyState::Full } else { DirtyState::Partial },
            ops,
            ..Default::default()
        }
    }

    fn update_text(update: &TerminalRenderUpdate) -> String {
        update
            .ops
            .iter()
            .flat_map(|op| &op.rows)
            .flat_map(|row| &row.cells)
            .flat_map(|cell| cell.graphemes.iter().filter_map(|scalar| char::from_u32(*scalar)))
            .collect()
    }

    /// Open a views channel with a watcher view 1 and consume the open frames.
    fn open_two_views(connection: &DaemonConnection, server: &mut SessionStream) -> (u32, Arc<Mutex<ChannelSlot>>) {
        let (channel, slot) =
            connection.open_session_channel("alpha".into(), geometry(80, 24), ChannelRole::Controller, AttachmentIdentity::default());
        assert_eq!(read_view_frame(server).1.msg_type, MSG_CONTROL_OPEN_CHANNEL);
        let (view, resize) = read_view_frame(server);
        assert_eq!((view, resize.msg_type), (Some(0), MSG_SESSION_INPUT));
        assert_eq!(connection.open_view(channel, geometry(60, 20), ChannelRole::Watcher), Some(1));
        let (view, open) = read_view_frame(server);
        assert_eq!(
            (view, open.decode::<crate::packet::OpenView>().unwrap()),
            (None, crate::packet::OpenView { view: 1, role: ChannelRole::Watcher, take: false })
        );
        let (view, resize) = read_view_frame(server);
        assert_eq!((view, resize.decode::<Input>().unwrap().event), (Some(1), TerminalInputEvent::Resize(geometry(60, 20))));
        (channel, slot)
    }

    // Live views fan out the attachment's one stream: the first to consume a
    // render gets it as sent and acks it; a view that is behind catches up
    // from the live frame without another render from the daemon.
    #[test]
    fn live_views_share_one_stream_and_a_lagging_view_catches_up() {
        let (_temp, layout) = test_layout();
        let daemon = views_daemon(&layout);
        let connection = DaemonConnection::open(layout, Vec::new(), Arc::new(|| {}));
        let mut server = daemon.accept(vec![directory_entry("alpha")]);
        wait_until(|| connection.is_connected());
        let (channel, slot) = open_two_views(&connection, &mut server);

        let first = rows_update(7, true, &[(0, "prompt"), (1, "first")]);
        RenderPacket::live(first.clone()).frame(channel, false).unwrap().write(&mut server).unwrap();
        wait_until(|| recover_lock(&slot).dirty(0) == DirtyState::Full);
        assert_eq!(recover_lock(&slot).dirty(1), DirtyState::Full);
        let consumed = recover_lock(&slot).consume(0).expect("view 0 render");
        assert_eq!(consumed.update, first);
        assert_eq!(consumed.ack, Some(RenderAck::Channel(7)));
        assert!(recover_lock(&slot).consume(0).is_none());
        assert_eq!(recover_lock(&slot).dirty(0), DirtyState::Clean);
        let caught_up = recover_lock(&slot).consume(1).expect("view 1 catch-up");
        assert_eq!(caught_up.ack, None);
        assert_eq!(caught_up.update.ops[0].kind, TerminalRenderUpdateOpKind::FullVisibleReplace);
        assert!(update_text(&caught_up.update).contains("first"));

        let second = rows_update(8, false, &[(2, "second")]);
        RenderPacket::live(second.clone()).frame(channel, false).unwrap().write(&mut server).unwrap();
        wait_until(|| recover_lock(&slot).dirty(1) == DirtyState::Partial);
        let consumed = recover_lock(&slot).consume(1).expect("view 1 render");
        assert_eq!((consumed.update.clone(), consumed.ack), (second, Some(RenderAck::Channel(8))));
        let lagging = recover_lock(&slot).consume(0).expect("view 0 catch-up");
        assert_eq!(lagging.ack, None);
        assert_eq!(lagging.update.ops.len(), 1);
        assert_eq!((lagging.update.ops[0].kind, lagging.update.ops[0].first_row), (TerminalRenderUpdateOpKind::RowReplace, 2));
        assert_eq!(update_text(&lagging.update), "second");

        // One ack per live render, from whichever view consumed it.
        connection.send_ack(channel, RenderAck::Channel(8)).unwrap();
        let (view, ack) = read_view_frame(&mut server);
        assert_eq!((view, ack.msg_type, ack.decode::<Ack>().unwrap()), (None, MSG_SESSION_ACK, Ack { generation: 8 }));
        connection.shutdown();
    }

    // A history frame is one view's own; the daemon's live state sends the
    // view back to the shared stream with a full catch-up.
    #[test]
    fn history_frames_belong_to_one_view_until_it_returns_to_live() {
        let (_temp, layout) = test_layout();
        let daemon = views_daemon(&layout);
        let connection = DaemonConnection::open(layout, Vec::new(), Arc::new(|| {}));
        let mut server = daemon.accept(vec![directory_entry("alpha")]);
        wait_until(|| connection.is_connected());
        let (channel, slot) = open_two_views(&connection, &mut server);

        RenderPacket::live(rows_update(5, true, &[(0, "live")])).frame(channel, false).unwrap().write(&mut server).unwrap();
        wait_until(|| recover_lock(&slot).dirty(1) == DirtyState::Full);
        let live = recover_lock(&slot).consume(1).expect("live frame");
        assert_eq!((update_text(&live.update).as_str(), live.ack), ("live", Some(RenderAck::Channel(5))));
        let history = RenderPacket {
            update: rows_update(6, true, &[(0, "history")]),
            links: Vec::new(),
            view: crate::provider::ViewState { status: crate::provider::ViewStatus::History, notice: None },
        };
        crate::packet::ViewFrame::wrap(1, &history.frame(channel, false).unwrap()).unwrap().write(&mut server).unwrap();
        wait_until(|| recover_lock(&slot).view(1).unwrap().pending.is_some());
        let consumed = recover_lock(&slot).consume(1).expect("history frame");
        assert_eq!((update_text(&consumed.update).as_str(), consumed.ack), ("history", Some(RenderAck::View(1, 6))));
        let live = recover_lock(&slot).consume(0).expect("live frame");
        assert_eq!((update_text(&live.update).as_str(), live.ack), ("live", None));

        // While in history, view 1 ignores the live stream.
        RenderPacket::live(rows_update(7, false, &[(1, "more")])).frame(channel, false).unwrap().write(&mut server).unwrap();
        wait_until(|| recover_lock(&slot).dirty(0) == DirtyState::Partial);
        assert_eq!(recover_lock(&slot).dirty(1), DirtyState::Clean);
        assert!(recover_lock(&slot).consume(1).is_none());
        assert!(recover_lock(&slot).consume(0).is_some());

        let live_state = crate::provider::ViewState::default();
        crate::packet::ViewFrame::wrap(1, &PacketFrame::new(channel, crate::packet::MSG_SESSION_VIEW_STATE, &live_state).unwrap())
            .unwrap()
            .write(&mut server)
            .unwrap();
        wait_until(|| recover_lock(&slot).dirty(1) == DirtyState::Full);
        // It shows history rows: only a whole live grid replaces them.
        let back = recover_lock(&slot).consume(1).expect("return to live");
        assert_eq!(back.update.ops.len(), 1);
        assert_eq!(back.update.ops[0].kind, TerminalRenderUpdateOpKind::FullVisibleReplace);
        assert!(update_text(&back.update).contains("live") && update_text(&back.update).contains("more"));
        assert!(back.update.render_generation > 6, "a view's generation went backwards");

        connection.send_ack(channel, RenderAck::View(1, 6)).unwrap();
        let (view, ack) = read_view_frame(&mut server);
        assert_eq!((view, ack.decode::<Ack>().unwrap()), (Some(1), Ack { generation: 6 }));

        // A view the daemon refuses reports why; the default view carries on.
        PacketFrame::new(channel, crate::packet::MSG_SESSION_CLOSE_VIEW, &crate::packet::CloseView { view: 1 })
            .unwrap()
            .write(&mut server)
            .unwrap();
        wait_until(|| recover_lock(&slot).view_closed(1).is_some_and(|reason| reason.contains("refused")));
        assert!(recover_lock(&slot).view_closed(0).is_none());
        connection.shutdown();
    }

    // Against a daemon before views, the default view works as it always has
    // and other views report why they cannot be served.
    #[test]
    fn a_daemon_before_views_serves_only_the_default_view() {
        let (_temp, layout) = test_layout();
        let daemon = FakeDaemon::bind(&layout);
        let connection = DaemonConnection::open(layout, Vec::new(), Arc::new(|| {}));
        let mut server = daemon.accept(vec![directory_entry("alpha")]);
        wait_until(|| connection.is_connected());
        let (channel, slot) =
            connection.open_session_channel("alpha".into(), geometry(80, 24), ChannelRole::Controller, AttachmentIdentity::default());
        assert_eq!(read_view_frame(&mut server).1.msg_type, MSG_CONTROL_OPEN_CHANNEL);
        assert_eq!(
            read_view_frame(&mut server),
            (None, PacketFrame::new(channel, MSG_SESSION_INPUT, &Input { event: TerminalInputEvent::Resize(geometry(80, 24)) }).unwrap())
        );

        let view = connection.open_view(channel, geometry(60, 20), ChannelRole::Controller).expect("view handle");
        assert!(recover_lock(&slot).view_closed(view).is_some_and(|reason| reason.contains("predates views")));
        assert!(connection.send_resize(channel, view, 10, 10).is_err());
        assert!(connection.close_view(channel, 0).is_err());
        assert!(connection.close_view(channel, view).is_ok());

        RenderPacket::live(render_update(3)).frame(channel, false).unwrap().write(&mut server).unwrap();
        wait_until(|| recover_lock(&slot).view(0).unwrap().pending.is_some());
        let consumed = recover_lock(&slot).consume(0).expect("default view render");
        assert_eq!((consumed.update, consumed.ack), (render_update(3), Some(RenderAck::Channel(3))));
        connection.send_resize(channel, 0, 100, 30).unwrap();
        assert_eq!(
            read_view_frame(&mut server),
            (None, PacketFrame::new(channel, MSG_SESSION_RESIZE, &crate::packet::Resize { cols: 100, rows: 30 }).unwrap())
        );
        connection.shutdown();
    }

    #[test]
    fn poisoned_client_mutexes_do_not_cascade_panics() {
        let (_temp, layout) = test_layout();
        let connection = DaemonConnection::open(layout, Vec::new(), Arc::new(|| {}));
        let (channel, slot) =
            connection.open_session_channel("alpha".to_string(), geometry(80, 24), ChannelRole::Controller, AttachmentIdentity::default());

        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _state = connection.state.lock().expect("state lock");
            panic!("poison state lock");
        }));
        assert_eq!(connection.with_directory(|directory| directory.entries.len()), 0);

        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _slot = slot.lock().expect("slot lock");
            panic!("poison channel slot lock");
        }));
        assert!(connection.request_role(channel, 0, ChannelRole::Watcher, false).is_err());

        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _writer = connection.writer.lock().expect("writer lock");
            panic!("poison writer lock");
        }));
        assert!(connection.send_resize(channel, 0, 100, 40).is_err());

        connection.shutdown();
    }
}
