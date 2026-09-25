#[cfg(unix)]
use std::{
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
};
use std::{
    sync::{
        atomic::{AtomicI64, AtomicU64, AtomicU8, Ordering as AtomicOrdering},
        mpsc,
        mpsc::{Receiver, SyncSender},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

#[cfg(target_os = "linux")]
use nix::poll::PollTimeout;
#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
#[cfg(target_os = "linux")]
use nix::sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags};
#[cfg(target_os = "macos")]
use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue};
#[cfg(unix)]
use nix::{
    errno::Errno,
    fcntl::{fcntl, FcntlArg, OFlag},
};

use super::presentation::{GateTransition, PresentationGate};
#[cfg(unix)]
use crate::platform::pty::PtyChild;
use crate::{
    protocol::{InspectResult, SignalTarget},
    provider::{
        DirtyState, TerminalRenderUpdate, TerminalScrollbackExtent, TerminalScrollbarState, TerminalSnapshot, TerminalViewportKind,
        ViewportCommand, ViewportCommandOutcome,
    },
    screen_activity::ScreenActivityTracker,
    session_runtime::{PtyOutput, SessionRuntime},
    vt,
};

const POSIX_SIGTERM: i32 = 15;
/// Ceiling on any reply wait against a session actor. The actor's pump slice
/// is budgeted (see `PTY_READ_BUDGET_PER_PUMP`), so healthy replies arrive in
/// milliseconds even under output flood; hitting this deadline means the
/// actor is genuinely wedged (stuck VT engine, blocked disk). Callers get an
/// error — and the daemon's servicing loop faults that one session — instead
/// of one actor freezing the entire control plane (ADR 0004: the servicing
/// side is never blocked).
const ACTOR_REPLY_DEADLINE: Duration = Duration::from_secs(5);
const RAW_OUTPUT_TAP_CHUNKS: usize = 64;

pub(crate) type WakeCallback = Arc<dyn Fn() + Send + Sync + 'static>;
pub(crate) type ImageResourceDataCallback = Box<dyn FnMut(&[u8]) -> bool + Send>;

pub(crate) struct RawOutputTap {
    rx: Receiver<RawOutputChunk>,
}

impl RawOutputTap {
    pub(crate) fn try_recv(&self) -> Result<RawOutputChunk, mpsc::TryRecvError> {
        self.rx.try_recv()
    }

    #[cfg(test)]
    pub(crate) fn test_channel(capacity: usize) -> (SyncSender<RawOutputChunk>, Self) {
        let (tx, rx) = mpsc::sync_channel(capacity);
        (tx, Self { rx })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RawOutputChunk {
    pub(crate) sequence: u64,
    /// Shared with every other tap and the actor's pump result: cloning a
    /// chunk to cross the actor→servicing channel is a refcount bump, not a
    /// payload copy (issue #135).
    pub(crate) bytes: Arc<[u8]>,
}

pub(crate) struct RawOutputReplay {
    pub(crate) payload: Option<Vec<u8>>,
    pub(crate) through_sequence: u64,
}

pub(crate) struct RawOutputRecovery {
    pub(crate) tap: RawOutputTap,
    pub(crate) payloads: Vec<Option<Vec<u8>>>,
}

#[derive(Clone, Debug)]
pub(crate) struct ObservationState {
    pub(crate) render_generation: u64,
    pub(crate) observed_generation: u64,
    dirty: DirtyState,
    dirty_rows: Vec<u16>,
    terminal_modes: Option<vt::TerminalModeState>,
    /// Publication is held for an open synchronized-output batch.
    held: bool,
    mirror: Option<Arc<ObservationMirror>>,
}

impl ObservationState {
    pub(crate) fn new(rows: u16) -> Self {
        Self::new_with_mirror(rows, None)
    }

    pub(crate) fn new_with_mirror(rows: u16, mirror: Option<Arc<ObservationMirror>>) -> Self {
        let mut state = Self {
            render_generation: 0,
            observed_generation: 0,
            dirty: DirtyState::Clean,
            dirty_rows: Vec::new(),
            terminal_modes: None,
            held: false,
            mirror,
        };
        state.mark_full(rows);
        state
    }

    /// Published dirty state: clean while publication is held, so pollers
    /// and wakes wait for the completed presentation.
    pub(crate) fn dirty(&self) -> DirtyState {
        if self.held {
            DirtyState::Clean
        } else {
            self.pending_dirty()
        }
    }

    /// Damage not yet observed, whether or not publication is held.
    pub(crate) fn pending_dirty(&self) -> DirtyState {
        if self.render_generation > self.observed_generation {
            self.dirty
        } else {
            DirtyState::Clean
        }
    }

    /// Hold or release publication. Damage accumulates while held; returns
    /// true when releasing exposes it, so the caller wakes the host.
    pub(crate) fn set_held(&mut self, held: bool) -> bool {
        if self.held == held {
            return false;
        }
        self.held = held;
        self.publish_mirror();
        !held && self.dirty() != DirtyState::Clean
    }

    pub(crate) fn mark_full(&mut self, _rows: u16) -> bool {
        let was_clean = self.dirty() == DirtyState::Clean;
        self.render_generation = self.render_generation.saturating_add(1);
        self.dirty = DirtyState::Full;
        self.dirty_rows.clear();
        self.publish_mirror();
        was_clean && !self.held
    }

    pub(crate) fn mark_partial_rows(&mut self, rows: impl IntoIterator<Item = u16>) -> bool {
        let was_clean = self.dirty() == DirtyState::Clean;
        self.render_generation = self.render_generation.saturating_add(1);
        if self.dirty != DirtyState::Full {
            self.dirty = DirtyState::Partial;
            for row in rows {
                if !self.dirty_rows.contains(&row) {
                    self.dirty_rows.push(row);
                }
            }
            self.dirty_rows.sort_unstable();
        }
        self.publish_mirror();
        was_clean && !self.held
    }

    pub(crate) fn mark_partial_unknown(&mut self) -> bool {
        let was_clean = self.dirty() == DirtyState::Clean;
        self.render_generation = self.render_generation.saturating_add(1);
        if self.dirty != DirtyState::Full {
            self.dirty = DirtyState::Partial;
            self.dirty_rows.clear();
        }
        self.publish_mirror();
        was_clean && !self.held
    }

    /// Publish the child's exit code to the mirror (once), waking observers
    /// on the transition.
    pub(crate) fn record_exit(&mut self, code: i32, wake: &WakeCallback) {
        if let Some(mirror) = &self.mirror {
            if mirror.exit_code().is_none() {
                mirror.record_exit(code);
                wake();
            }
        }
    }

    pub(crate) fn sync_terminal_modes(&mut self, terminal_modes: vt::TerminalModeState) -> bool {
        let previous = self.terminal_modes.replace(terminal_modes);
        match previous {
            None => false,
            Some(previous) if previous == terminal_modes => false,
            Some(_) => self.mark_partial_unknown(),
        }
    }

    pub(crate) fn mark_observed(&mut self, generation: u64) -> bool {
        if generation > self.render_generation {
            return false;
        }
        self.observed_generation = self.observed_generation.max(generation);
        if self.observed_generation >= self.render_generation {
            self.dirty = DirtyState::Clean;
            self.dirty_rows.clear();
        }
        self.publish_mirror();
        true
    }

    pub(crate) fn annotate_snapshot(&self, snapshot: &mut TerminalSnapshot) {
        snapshot.render_generation = self.render_generation;
        snapshot.dirty = self.pending_dirty();
        snapshot.dirty_rows = if snapshot.dirty == DirtyState::Partial {
            if self.dirty_rows.is_empty() {
                snapshot.dirty_rows.clone()
            } else {
                self.dirty_rows.clone()
            }
        } else {
            Vec::new()
        };
    }

    pub(crate) fn annotate_render_update(&self, update: &mut TerminalRenderUpdate) {
        update.render_generation = self.render_generation;
        let dirty = self.pending_dirty();
        if dirty == DirtyState::Clean {
            update.dirty = DirtyState::Clean;
            update.ops.clear();
        } else if update.dirty == DirtyState::Clean {
            update.dirty = dirty;
        }
    }

    fn publish_mirror(&self) {
        if let Some(mirror) = &self.mirror {
            let dirty = if self.held { DirtyState::Clean } else { self.dirty };
            mirror.store(self.render_generation, self.observed_generation, dirty);
        }
    }
}

/// Sentinel for "the session has not exited"; exit codes are i32, so this
/// value is unreachable.
const EXIT_CODE_UNSET: i64 = i64::MIN;

#[derive(Debug)]
pub(crate) struct ObservationMirror {
    render_generation: AtomicU64,
    observed_generation: AtomicU64,
    dirty: AtomicU8,
    /// Exit code recorded by the actor when it reaps the child; lets the
    /// daemon's servicing loop poll for exit without a blocking round-trip
    /// into a possibly-busy actor (ADR 0004: never-blocked servicing side).
    exit_code: AtomicI64,
}

impl ObservationMirror {
    pub(crate) fn new() -> Self {
        Self {
            render_generation: AtomicU64::new(0),
            observed_generation: AtomicU64::new(0),
            dirty: AtomicU8::new(dirty_state_to_u8(DirtyState::Clean)),
            exit_code: AtomicI64::new(EXIT_CODE_UNSET),
        }
    }

    pub(crate) fn record_exit(&self, code: i32) {
        self.exit_code.store(code as i64, AtomicOrdering::SeqCst);
    }

    pub(crate) fn exit_code(&self) -> Option<i32> {
        match self.exit_code.load(AtomicOrdering::SeqCst) {
            EXIT_CODE_UNSET => None,
            code => Some(code as i32),
        }
    }

    pub(crate) fn store(&self, render_generation: u64, observed_generation: u64, dirty: DirtyState) {
        self.dirty.store(dirty_state_to_u8(dirty), AtomicOrdering::SeqCst);
        self.observed_generation.store(observed_generation, AtomicOrdering::SeqCst);
        self.render_generation.store(render_generation, AtomicOrdering::SeqCst);
    }

    pub(crate) fn dirty(&self) -> DirtyState {
        let render_generation = self.render_generation.load(AtomicOrdering::SeqCst);
        let observed_generation = self.observed_generation.load(AtomicOrdering::SeqCst);
        if render_generation > observed_generation {
            dirty_state_from_u8(self.dirty.load(AtomicOrdering::SeqCst))
        } else {
            DirtyState::Clean
        }
    }

    pub(crate) fn render_generation(&self) -> u64 {
        self.render_generation.load(AtomicOrdering::SeqCst)
    }
}

fn dirty_state_to_u8(dirty: DirtyState) -> u8 {
    match dirty {
        DirtyState::Clean => 0,
        DirtyState::Partial => 1,
        DirtyState::Full => 2,
    }
}

fn dirty_state_from_u8(value: u8) -> DirtyState {
    match value {
        1 => DirtyState::Partial,
        2 => DirtyState::Full,
        _ => {
            debug_assert_eq!(value, 0, "unexpected DirtyState byte");
            DirtyState::Clean
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SessionWheelEvent {
    pub(crate) modifiers: vt::MouseModifiers,
    pub(crate) cell_col: u16,
    pub(crate) cell_row: u16,
    pub(crate) x_px: f32,
    pub(crate) y_px: f32,
    pub(crate) wheel_delta_x: f32,
    pub(crate) wheel_delta_y: f32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SessionMouseEvent {
    pub(crate) action: vt::MouseAction,
    pub(crate) button: Option<vt::MouseButton>,
    pub(crate) any_button_pressed: bool,
    pub(crate) modifiers: vt::MouseModifiers,
    pub(crate) x_px: f32,
    pub(crate) y_px: f32,
}

pub(crate) enum SessionCommand {
    RetainInputSources { sources: Vec<u128>, reply: mpsc::Sender<Result<(), String>> },
    Key { source: u128, event: Box<crate::provider::TerminalKeyEvent>, reply: mpsc::Sender<Result<usize, String>> },
    ReleaseInput { source: u128, reply: mpsc::Sender<Result<(), String>> },
    SetAttachmentView { id: u128, command: ViewportCommand, reply: mpsc::Sender<Result<bool, String>> },
    CaptureAttachmentView { id: u128, reply: mpsc::Sender<Result<Option<crate::provider::CapturedView>, String>> },
    ReleaseAttachmentView { id: u128 },
    Focus { focused: bool, reply: mpsc::Sender<Result<(), String>> },
    Resize { cols: u16, rows: u16, reply: mpsc::Sender<Result<(), String>> },
    SetCellSize { cell_width_px: u32, cell_height_px: u32, reply: mpsc::Sender<Result<(), String>> },
    WriteInput { bytes: Vec<u8>, reply: mpsc::Sender<Result<(), String>> },
    Wheel { event: SessionWheelEvent, reply: mpsc::Sender<Result<usize, String>> },
    ApplicationWheel { event: SessionWheelEvent, reply: mpsc::Sender<Result<usize, String>> },
    Mouse { source: u128, event: SessionMouseEvent, reply: mpsc::Sender<Result<usize, String>> },
    Paste { text: Vec<u8>, reply: mpsc::Sender<Result<usize, String>> },
    ScrollViewport { command: ViewportCommand, reply: mpsc::Sender<Result<ViewportCommandOutcome, String>> },
    Snapshot { reply: mpsc::Sender<Result<TerminalSnapshot, String>> },
    RenderUpdate { reply: mpsc::Sender<Result<TerminalRenderUpdate, String>> },
    PacketRender { full: bool, reply: mpsc::Sender<Result<Option<crate::image_delivery::RenderBundle>, String>> },
    FullSnapshot { reply: mpsc::Sender<Result<TerminalSnapshot, String>> },
    ImageResourceData { image_id: u32, generation: u64, callback: ImageResourceDataCallback, reply: mpsc::Sender<Result<bool, String>> },
    Inspect { has_controller: bool, watcher_count: usize, reply: mpsc::Sender<Result<InspectResult, String>> },
    ApplyAttachState { cols: u16, rows: u16, capabilities: vt::ClientCapabilities, reply: mpsc::Sender<Result<RawOutputReplay, String>> },
    ReplayPayload { capabilities: vt::ClientCapabilities, reply: mpsc::Sender<Result<RawOutputReplay, String>> },
    CaptureText { reply: mpsc::Sender<Result<String, String>> },
    ValidateTextMatching { reply: mpsc::Sender<Result<(), String>> },
    ScreenContains { text: String, reply: mpsc::Sender<Result<bool, String>> },
    LastPtyOutputAt { reply: mpsc::Sender<Result<Option<Instant>, String>> },
    FlushScreenActivity,
    FlushRecording { reply: mpsc::Sender<Result<(), String>> },
    RecordingActive { reply: mpsc::Sender<Result<bool, String>> },
    RecordAttach { reply: mpsc::Sender<Result<(), String>> },
    RecordDetach { reply: mpsc::Sender<Result<(), String>> },
    WriteInputWithMark { bytes: Vec<u8>, marker_name: String, reply: mpsc::Sender<Result<u64, String>> },
    PasteWithMark { text: Vec<u8>, marker_name: String, reply: mpsc::Sender<Result<u64, String>> },
    SetRecording { enable: bool, reply: mpsc::Sender<Result<(), String>> },
    Mark { name: Option<String>, reply: mpsc::Sender<Result<u64, String>> },
    UpdateTags { add: Vec<String>, remove: Vec<String>, reply: mpsc::Sender<Result<Vec<String>, String>> },
    ResolveMarker { name: String, reply: mpsc::Sender<Result<Option<u64>, String>> },
    ResolveNextMarker { after: u64, reply: mpsc::Sender<Result<Option<u64>, String>> },
    DispatchSignal { signal: i32, target: SignalTarget, reply: mpsc::Sender<Result<(), String>> },
    ShouldKeepSessionDir { reply: mpsc::Sender<Result<bool, String>> },
    MarkObserved { generation: u64, reply: mpsc::Sender<bool> },
    ScrollbackExtent { reply: mpsc::Sender<TerminalScrollbackExtent> },
    ScrollbarState { reply: mpsc::Sender<TerminalScrollbarState> },
    SetQueryPassthrough { enabled: bool, reply: mpsc::Sender<Result<(), String>> },
    SubscribeRawOutput { reply: mpsc::Sender<RawOutputTap> },
    RecoverRawOutput { capabilities: Vec<vt::ClientCapabilities>, reply: mpsc::Sender<Result<RawOutputRecovery, String>> },
    Stop { terminate: bool },
}

pub(crate) struct SessionActor {
    tx: CommandSender,
    observation: Arc<ObservationMirror>,
    screen_activity: ScreenActivityTracker,
    worker: Option<thread::JoinHandle<()>>,
}

struct CommandSender {
    tx: mpsc::Sender<SessionCommand>,
    #[cfg(unix)]
    wake: CommandWakeWriter,
}

impl CommandSender {
    #[cfg(unix)]
    fn new(tx: mpsc::Sender<SessionCommand>, wake: CommandWakeWriter) -> Self {
        Self { tx, wake }
    }

    #[cfg(not(unix))]
    fn new(tx: mpsc::Sender<SessionCommand>) -> Self {
        Self { tx }
    }

    #[cfg(test)]
    fn inert(tx: mpsc::Sender<SessionCommand>) -> Self {
        Self {
            tx,
            #[cfg(unix)]
            wake: CommandWakeWriter::noop(),
        }
    }

    fn send(&self, command: SessionCommand) -> Result<(), mpsc::SendError<SessionCommand>> {
        self.tx.send(command)?;
        #[cfg(unix)]
        self.wake.wake();
        Ok(())
    }
}

#[cfg(unix)]
#[derive(Clone)]
struct CommandWakeWriter {
    fd: Option<Arc<OwnedFd>>,
}

#[cfg(unix)]
impl CommandWakeWriter {
    fn new(fd: OwnedFd) -> Self {
        Self { fd: Some(Arc::new(fd)) }
    }

    #[cfg(test)]
    fn noop() -> Self {
        Self { fd: None }
    }

    fn wake(&self) {
        let Some(fd) = &self.fd else {
            return;
        };
        let byte = [1u8];
        loop {
            let written = unsafe { libc::write(fd.as_raw_fd(), byte.as_ptr().cast(), byte.len()) };
            if written >= 0 {
                return;
            }
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
    }
}

#[cfg(unix)]
struct CommandWakeReader {
    fd: OwnedFd,
}

#[cfg(unix)]
impl CommandWakeReader {
    fn drain(&self) {
        let mut buf = [0u8; 64];
        loop {
            let read = unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if read > 0 {
                continue;
            }
            if read == 0 {
                return;
            }
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
    }

    fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

#[cfg(unix)]
fn command_wake_pair() -> Result<(CommandWakeReader, CommandWakeWriter), String> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(format!("create actor command wake pipe: {}", io::Error::last_os_error()));
    }
    let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    set_fd_nonblocking(read.as_raw_fd()).map_err(|err| format!("set actor command wake read nonblocking: {err}"))?;
    set_fd_nonblocking(write.as_raw_fd()).map_err(|err| format!("set actor command wake write nonblocking: {err}"))?;
    Ok((CommandWakeReader { fd: read }, CommandWakeWriter::new(write)))
}

#[cfg(unix)]
fn set_fd_nonblocking(fd: RawFd) -> Result<(), Errno> {
    let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    let flags = OFlag::from_bits_truncate(fcntl(fd, FcntlArg::F_GETFL)?);
    fcntl(fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
    Ok(())
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Default)]
struct ActorReadiness {
    pty_readable: bool,
    command_readable: bool,
}

#[cfg(unix)]
/// Wait for PTY output or a command, or until `timeout` (the presentation
/// recovery deadline) elapses with neither.
fn wait_actor_ready(pty_child: &PtyChild, command_fd: RawFd, timeout: Option<Duration>) -> Result<ActorReadiness, String> {
    #[cfg(target_os = "macos")]
    {
        wait_actor_ready_kqueue(pty_child.master_fd(), command_fd, timeout)
    }
    #[cfg(target_os = "linux")]
    {
        wait_actor_ready_epoll(pty_child.master_fd(), command_fd, timeout)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        wait_actor_ready_poll(pty_child.master_fd(), command_fd, timeout)
    }
}

/// Round up so a wait never returns just short of the deadline it serves.
#[cfg(all(unix, not(target_os = "macos")))]
fn poll_timeout(timeout: Option<Duration>) -> PollTimeout {
    timeout.map_or(PollTimeout::NONE, |timeout| {
        let millis = timeout.as_nanos().div_ceil(1_000_000);
        PollTimeout::try_from(millis).unwrap_or(PollTimeout::MAX)
    })
}

#[cfg(target_os = "macos")]
fn wait_actor_ready_kqueue(pty_fd: RawFd, command_fd: RawFd, timeout: Option<Duration>) -> Result<ActorReadiness, String> {
    const TOKEN_PTY: isize = 1;
    const TOKEN_COMMAND: isize = 2;

    let kqueue = Kqueue::new().map_err(|err| format!("create actor kqueue: {err}"))?;
    let changes = [
        KEvent::new(pty_fd as _, EventFilter::EVFILT_READ, EvFlags::EV_ADD, FilterFlag::empty(), 0, TOKEN_PTY),
        KEvent::new(command_fd as _, EventFilter::EVFILT_READ, EvFlags::EV_ADD, FilterFlag::empty(), 0, TOKEN_COMMAND),
    ];
    let mut events = [KEvent::new(0, EventFilter::EVFILT_READ, EvFlags::empty(), FilterFlag::empty(), 0, 0); 2];
    let event_count = loop {
        let timeout = timeout
            .map(|timeout| libc::timespec { tv_sec: timeout.as_secs() as libc::time_t, tv_nsec: timeout.subsec_nanos() as libc::c_long });
        match kqueue.kevent(&changes, &mut events, timeout) {
            Ok(event_count) => break event_count,
            Err(Errno::EINTR) if timeout.is_none() => continue,
            // Let the caller recompute the remaining time to its deadline.
            Err(Errno::EINTR) => return Ok(ActorReadiness::default()),
            Err(err) => return Err(format!("kqueue actor fds: {err}")),
        }
    };

    let mut readiness = ActorReadiness::default();
    for event in events.iter().take(event_count) {
        if event.flags().contains(EvFlags::EV_ERROR) {
            if event.data() != 0 {
                return Err(format!("kqueue actor fd registration: {}", Errno::from_raw(event.data() as i32)));
            }
            continue;
        }
        match event.udata() {
            TOKEN_PTY => readiness.pty_readable = true,
            TOKEN_COMMAND => readiness.command_readable = true,
            _ => {}
        }
    }
    Ok(readiness)
}

#[cfg(target_os = "linux")]
fn wait_actor_ready_epoll(pty_fd: RawFd, command_fd: RawFd, timeout: Option<Duration>) -> Result<ActorReadiness, String> {
    const TOKEN_PTY: u64 = 1;
    const TOKEN_COMMAND: u64 = 2;

    let epoll = Epoll::new(EpollCreateFlags::EPOLL_CLOEXEC).map_err(|err| format!("create actor epoll: {err}"))?;
    let read_or_closed = EpollFlags::EPOLLIN | EpollFlags::EPOLLHUP | EpollFlags::EPOLLERR;
    epoll
        .add(unsafe { std::os::fd::BorrowedFd::borrow_raw(pty_fd) }, EpollEvent::new(read_or_closed, TOKEN_PTY))
        .map_err(|err| format!("register pty with actor epoll: {err}"))?;
    epoll
        .add(unsafe { std::os::fd::BorrowedFd::borrow_raw(command_fd) }, EpollEvent::new(read_or_closed, TOKEN_COMMAND))
        .map_err(|err| format!("register command wake with actor epoll: {err}"))?;

    let mut events = [EpollEvent::empty(); 2];
    let event_count = loop {
        match epoll.wait(&mut events, poll_timeout(timeout)) {
            Ok(event_count) => break event_count,
            Err(Errno::EINTR) if timeout.is_none() => continue,
            // Let the caller recompute the remaining time to its deadline.
            Err(Errno::EINTR) => return Ok(ActorReadiness::default()),
            Err(err) => return Err(format!("epoll actor fds: {err}")),
        }
    };

    let mut readiness = ActorReadiness::default();
    for event in events.iter().take(event_count) {
        if event.events().intersects(read_or_closed) {
            match event.data() {
                TOKEN_PTY => readiness.pty_readable = true,
                TOKEN_COMMAND => readiness.command_readable = true,
                _ => {}
            }
        }
    }
    Ok(readiness)
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn wait_actor_ready_poll(pty_fd: RawFd, command_fd: RawFd, timeout: Option<Duration>) -> Result<ActorReadiness, String> {
    let mut fds = [
        PollFd::new(unsafe { std::os::fd::BorrowedFd::borrow_raw(pty_fd) }, PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR),
        PollFd::new(
            unsafe { std::os::fd::BorrowedFd::borrow_raw(command_fd) },
            PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR,
        ),
    ];
    loop {
        match poll(&mut fds, poll_timeout(timeout)) {
            Ok(_) => break,
            Err(Errno::EINTR) if timeout.is_none() => continue,
            // Let the caller recompute the remaining time to its deadline.
            Err(Errno::EINTR) => return Ok(ActorReadiness::default()),
            Err(err) => return Err(format!("poll actor fds: {err}")),
        }
    }
    let read_or_closed = PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR;
    Ok(ActorReadiness {
        pty_readable: fds[0].revents().unwrap_or_else(PollFlags::empty).intersects(read_or_closed),
        command_readable: fds[1].revents().unwrap_or_else(PollFlags::empty).intersects(read_or_closed),
    })
}

impl SessionActor {
    pub(crate) fn spawn(
        rows: u16,
        wake: WakeCallback,
        build_runtime: impl FnOnce() -> Result<SessionRuntime, String> + Send + 'static,
    ) -> Result<Self, String> {
        let observation = Arc::new(ObservationMirror::new());
        let actor_observation = observation.clone();
        let (tx, rx) = mpsc::channel();
        #[cfg(unix)]
        let (command_wake_reader, command_wake_writer) = command_wake_pair()?;
        let (ready_tx, ready_rx) = mpsc::channel();
        let worker = thread::spawn(move || match build_runtime() {
            Ok(runtime) => {
                #[cfg(unix)]
                session_actor_loop(runtime, wake, rows, actor_observation, ready_tx, rx, command_wake_reader);
                #[cfg(not(unix))]
                session_actor_loop(runtime, wake, rows, actor_observation, ready_tx, rx);
            }
            Err(err) => {
                let _ = ready_tx.send(Err(err));
            }
        });
        #[cfg(unix)]
        let tx = CommandSender::new(tx, command_wake_writer);
        #[cfg(not(unix))]
        let tx = CommandSender::new(tx);
        match ready_rx.recv().map_err(|_| "session actor did not report startup".to_string())? {
            Ok(screen_activity) => Ok(Self { tx, observation, screen_activity, worker: Some(worker) }),
            Err(err) => {
                let _ = worker.join();
                Err(err)
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn from_parts(
        tx: mpsc::Sender<SessionCommand>,
        observation: Arc<ObservationMirror>,
        worker: Option<thread::JoinHandle<()>>,
    ) -> Self {
        Self { tx: CommandSender::inert(tx), observation, screen_activity: ScreenActivityTracker::new(0), worker }
    }

    pub(crate) fn observation(&self) -> &ObservationMirror {
        &self.observation
    }

    pub(crate) fn screen_activity(&self) -> &ScreenActivityTracker {
        &self.screen_activity
    }

    /// True when the actor's worker thread has stopped. Combined with a
    /// missing mirror exit code this means the actor died without reaping
    /// its child — the session must be faulted.
    pub(crate) fn worker_finished(&self) -> bool {
        self.worker.as_ref().map(|worker| worker.is_finished()).unwrap_or(true)
    }

    pub(crate) fn request<T>(&self, make_command: impl FnOnce(mpsc::Sender<T>) -> SessionCommand, fallback: T) -> T {
        let (reply, recv) = mpsc::channel();
        if self.tx.send(make_command(reply)).is_err() {
            return fallback;
        }
        recv.recv_timeout(ACTOR_REPLY_DEADLINE).unwrap_or(fallback)
    }

    pub(crate) fn request_result<T>(
        &self,
        make_command: impl FnOnce(mpsc::Sender<Result<T, String>>) -> SessionCommand,
    ) -> Result<T, String> {
        let (reply, recv) = mpsc::channel();
        self.tx.send(make_command(reply)).map_err(|_| "session actor is not running".to_string())?;
        match recv.recv_timeout(ACTOR_REPLY_DEADLINE) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(format!("session actor did not reply within {ACTOR_REPLY_DEADLINE:?}")),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err("session actor did not reply".to_string()),
        }
    }

    /// Enable only when a raw-stream controller receives and answers terminal queries.
    /// Packet clients render structured state and leave query answering to the engine.
    pub(crate) fn set_query_passthrough(&self, enabled: bool) -> Result<(), String> {
        self.request_result(|reply| SessionCommand::SetQueryPassthrough { enabled, reply })
    }

    pub(crate) fn subscribe_raw_output(&self) -> Result<RawOutputTap, String> {
        let (reply, recv) = mpsc::channel();
        self.tx.send(SessionCommand::SubscribeRawOutput { reply }).map_err(|_| "session actor is not running".to_string())?;
        recv.recv().map_err(|_| "session actor did not reply".to_string())
    }

    pub(crate) fn recover_raw_output(&self, capabilities: Vec<vt::ClientCapabilities>) -> Result<RawOutputRecovery, String> {
        self.request_result(|reply| SessionCommand::RecoverRawOutput { capabilities, reply })
    }

    pub(crate) fn inspect(&self, has_controller: bool, watcher_count: usize) -> Result<InspectResult, String> {
        self.request_result(|reply| SessionCommand::Inspect { has_controller, watcher_count, reply })
    }

    pub(crate) fn apply_attach_state(&self, cols: u16, rows: u16, capabilities: vt::ClientCapabilities) -> Result<RawOutputReplay, String> {
        self.request_result(|reply| SessionCommand::ApplyAttachState { cols, rows, capabilities, reply })
    }

    pub(crate) fn resize(&self, cols: u16, rows: u16) -> Result<(), String> {
        self.request_result(|reply| SessionCommand::Resize { cols, rows, reply })
    }

    pub(crate) fn set_cell_size(&self, cell_width_px: u32, cell_height_px: u32) -> Result<(), String> {
        self.request_result(|reply| SessionCommand::SetCellSize { cell_width_px, cell_height_px, reply })
    }

    pub(crate) fn write_input(&self, bytes: Vec<u8>) -> Result<(), String> {
        self.request_result(|reply| SessionCommand::WriteInput { bytes, reply })
    }

    pub(crate) fn application_wheel(&self, event: SessionWheelEvent) -> Result<usize, String> {
        self.request_result(|reply| SessionCommand::ApplicationWheel { event, reply })
    }

    pub(crate) fn key(&self, source: u128, event: crate::provider::TerminalKeyEvent) -> Result<usize, String> {
        self.request_result(|reply| SessionCommand::Key { source, event: Box::new(event), reply })
    }
    pub(crate) fn retain_input_sources(&self, sources: Vec<u128>) -> Result<(), String> {
        self.request_result(|reply| SessionCommand::RetainInputSources { sources, reply })
    }
    pub(crate) fn release_input(&self, source: u128) -> Result<(), String> {
        self.request_result(|reply| SessionCommand::ReleaseInput { source, reply })
    }

    pub(crate) fn mouse(&self, source: u128, event: SessionMouseEvent) -> Result<usize, String> {
        self.request_result(|reply| SessionCommand::Mouse { source, event, reply })
    }

    pub(crate) fn paste(&self, text: Vec<u8>) -> Result<usize, String> {
        self.request_result(|reply| SessionCommand::Paste { text, reply })
    }

    pub(crate) fn replay_payload(&self, capabilities: vt::ClientCapabilities) -> Result<RawOutputReplay, String> {
        self.request_result(|reply| SessionCommand::ReplayPayload { capabilities, reply })
    }

    pub(crate) fn full_snapshot(&self) -> Result<TerminalSnapshot, String> {
        self.request_result(|reply| SessionCommand::FullSnapshot { reply })
    }

    /// Render for the packet cache; `None` while a synchronized-output batch
    /// withholds publication (the cache already holds the retained frame).
    pub(crate) fn packet_render(&self, full: bool) -> Result<Option<crate::image_delivery::RenderBundle>, String> {
        self.request_result(|reply| SessionCommand::PacketRender { full, reply })
    }

    pub(crate) fn capture_text(&self) -> Result<String, String> {
        self.request_result(|reply| SessionCommand::CaptureText { reply })
    }

    pub(crate) fn validate_text_matching(&self) -> Result<(), String> {
        self.request_result(|reply| SessionCommand::ValidateTextMatching { reply })
    }

    pub(crate) fn screen_contains(&self, text: String) -> Result<bool, String> {
        self.request_result(|reply| SessionCommand::ScreenContains { text, reply })
    }

    pub(crate) fn last_pty_output_at(&self) -> Result<Option<Instant>, String> {
        self.request_result(|reply| SessionCommand::LastPtyOutputAt { reply })
    }

    pub(crate) fn release_attachment_view(&self, id: u128) {
        let _ = self.tx.send(SessionCommand::ReleaseAttachmentView { id });
    }

    pub(crate) fn enqueue_screen_activity_flush(&self) -> Result<(), String> {
        self.tx.send(SessionCommand::FlushScreenActivity).map_err(|_| "session actor is not running".to_string())
    }

    pub(crate) fn flush_recording(&self) -> Result<(), String> {
        self.request_result(|reply| SessionCommand::FlushRecording { reply })
    }

    pub(crate) fn recording_active(&self) -> Result<bool, String> {
        self.request_result(|reply| SessionCommand::RecordingActive { reply })
    }

    pub(crate) fn record_attach(&self) -> Result<(), String> {
        self.request_result(|reply| SessionCommand::RecordAttach { reply })
    }

    pub(crate) fn record_detach(&self) -> Result<(), String> {
        self.request_result(|reply| SessionCommand::RecordDetach { reply })
    }

    pub(crate) fn write_input_with_mark(&self, bytes: Vec<u8>, marker_name: String) -> Result<u64, String> {
        self.request_result(|reply| SessionCommand::WriteInputWithMark { bytes, marker_name, reply })
    }

    pub(crate) fn paste_with_mark(&self, text: Vec<u8>, marker_name: String) -> Result<u64, String> {
        self.request_result(|reply| SessionCommand::PasteWithMark { text, marker_name, reply })
    }

    pub(crate) fn set_recording(&self, enable: bool) -> Result<(), String> {
        self.request_result(|reply| SessionCommand::SetRecording { enable, reply })
    }

    pub(crate) fn mark(&self, name: Option<String>) -> Result<u64, String> {
        self.request_result(|reply| SessionCommand::Mark { name, reply })
    }

    pub(crate) fn update_tags(&self, add: Vec<String>, remove: Vec<String>) -> Result<Vec<String>, String> {
        self.request_result(|reply| SessionCommand::UpdateTags { add, remove, reply })
    }

    pub(crate) fn resolve_marker(&self, name: String) -> Result<Option<u64>, String> {
        self.request_result(|reply| SessionCommand::ResolveMarker { name, reply })
    }

    pub(crate) fn resolve_next_marker_after(&self, after: u64) -> Result<Option<u64>, String> {
        self.request_result(|reply| SessionCommand::ResolveNextMarker { after, reply })
    }

    pub(crate) fn dispatch_signal(&self, signal: i32, target: SignalTarget) -> Result<(), String> {
        self.request_result(|reply| SessionCommand::DispatchSignal { signal, target, reply })
    }

    pub(crate) fn should_keep_session_dir(&self) -> Result<bool, String> {
        self.request_result(|reply| SessionCommand::ShouldKeepSessionDir { reply })
    }
}

/// How long `Drop` waits inline for the worker before handing the join to a
/// reaper thread. Covers the common case (the worker honors Stop within one
/// pump slice, tens of ms) without ever stalling the servicing thread on a
/// wedged worker.
const ACTOR_DROP_INLINE_WAIT: Duration = Duration::from_millis(100);

impl Drop for SessionActor {
    fn drop(&mut self) {
        let _ = self.tx.send(SessionCommand::Stop { terminate: true });
        if let Some(worker) = self.worker.take() {
            // Drop runs on the daemon's servicing thread, which must never
            // block long on one session (ADR 0004) — the residual inline
            // bound here is deliberately small and the slow path moves off
            // the shared thread entirely. The worker still runs its own
            // teardown (child terminate, recording flush) whenever it
            // finishes; the reaper just collects it.
            let deadline = Instant::now() + ACTOR_DROP_INLINE_WAIT;
            while !worker.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            if worker.is_finished() {
                let _ = worker.join();
            } else {
                thread::spawn(move || {
                    let _ = worker.join();
                });
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PumpOutcome {
    Clean,
    PartialUnknown,
    Full,
}

struct PumpResult {
    outcome: PumpOutcome,
    chunks: Vec<Arc<[u8]>>,
}

struct SessionActorLoopState {
    images: crate::image_delivery::CaptureImages,
    observation: ObservationState,
    presentation: PresentationGate,
    exited: bool,
    exit_code: Option<i32>,
    queries_forwarded_to_client: bool,
    raw_output_taps: Vec<SyncSender<RawOutputChunk>>,
    last_raw_output_sequence: u64,
}

fn pump_session_runtime(
    runtime: &mut SessionRuntime,
    exited: &mut bool,
    exit_code: &mut Option<i32>,
    queries_forwarded_to_client: bool,
) -> Result<PumpResult, String> {
    let mut exited_now = false;
    if !*exited {
        if let Some(code) = runtime.exit_code_if_exited()? {
            runtime.record_exit_code(code);
            *exited = true;
            *exit_code = Some(code);
            exited_now = true;
        }
    }
    let output = if exited_now {
        runtime.drain_output_after_exit(queries_forwarded_to_client)?
    } else if *exited {
        PtyOutput { chunks: Vec::new() }
    } else {
        runtime.read_available_output(queries_forwarded_to_client)?
    };
    let outcome = if exited_now {
        PumpOutcome::Full
    } else if !output.chunks.is_empty() {
        PumpOutcome::PartialUnknown
    } else {
        PumpOutcome::Clean
    };
    Ok(PumpResult { outcome, chunks: output.chunks })
}

fn mark_full_and_wake(observation: &mut ObservationState, rows: u16, wake: &WakeCallback) {
    if observation.mark_full(rows) {
        wake();
    }
}

fn mark_partial_unknown_and_wake(observation: &mut ObservationState, wake: &WakeCallback) {
    if observation.mark_partial_unknown() {
        wake();
    }
}

/// Record that the host observed `generation`, re-arming the wake if the
/// session is still dirty afterwards.
///
/// Wakes are edge-triggered on the clean-to-dirty transition. Output that
/// arrives between a host's render and its (asynchronous) observation of that
/// render advances the generation while the session is already dirty, so it
/// fires no wake of its own; a stale observation then leaves the session dirty
/// with no wake pending. Waking again here gives the host the edge it missed.
fn mark_observed_and_wake(observation: &mut ObservationState, generation: u64, wake: &WakeCallback) -> bool {
    let marked = observation.mark_observed(generation);
    if marked && observation.dirty() != DirtyState::Clean {
        wake();
    }
    marked
}

fn sync_terminal_modes_and_wake(runtime: &SessionRuntime, observation: &mut ObservationState, wake: &WakeCallback) {
    if let Ok(terminal_modes) = runtime.terminal_mode_state() {
        if observation.sync_terminal_modes(terminal_modes) {
            wake();
        }
    }
}

fn session_actor_loop(
    mut runtime: SessionRuntime,
    wake: WakeCallback,
    rows: u16,
    mirror: Arc<ObservationMirror>,
    ready: mpsc::Sender<Result<ScreenActivityTracker, String>>,
    rx: mpsc::Receiver<SessionCommand>,
    #[cfg(unix)] command_wake: CommandWakeReader,
) {
    let mut state = SessionActorLoopState {
        images: Default::default(),
        observation: ObservationState::new_with_mirror(rows, Some(mirror)),
        presentation: PresentationGate::default(),
        exited: false,
        exit_code: None,
        queries_forwarded_to_client: false,
        raw_output_taps: Vec::new(),
        last_raw_output_sequence: 0,
    };
    let _ = ready.send(Ok(runtime.screen_activity_tracker()));
    loop {
        // Exit has been reaped and final output drained. EOF remains readable
        // forever, so monitoring the PTY here would turn an idle retained
        // terminal into a busy loop. Keep its state available for commands,
        // but do no background work until one arrives.
        if state.exited {
            let Ok(command) = rx.recv() else { break };
            #[cfg(unix)]
            command_wake.drain();
            if session_actor_handle_command(command, &mut runtime, &mut state, &wake) {
                break;
            }
            continue;
        }
        #[cfg(unix)]
        {
            let timeout = state.presentation.deadline().map(|deadline| deadline.saturating_duration_since(Instant::now()));
            let readiness = match wait_actor_ready(runtime.pty_child(), command_wake.raw_fd(), timeout) {
                Ok(readiness) => readiness,
                Err(_) => {
                    let _ = runtime.dispatch_signal(POSIX_SIGTERM, SignalTarget::Leader);
                    break;
                }
            };
            if readiness.command_readable {
                command_wake.drain();
                if drain_session_commands(&rx, &mut runtime, &mut state, &wake) {
                    break;
                }
            }
            if readiness.pty_readable {
                session_actor_pump(&mut runtime, &mut state, &wake);
            } else if state.presentation.expired(Instant::now()) {
                reconcile_presentation(&mut runtime, &mut state, &wake);
            }
        }
        #[cfg(not(unix))]
        {
            match rx.recv_timeout(std::time::Duration::from_millis(10)) {
                Ok(command) => {
                    if session_actor_handle_command(command, &mut runtime, &mut state, &wake) {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    session_actor_pump(&mut runtime, &mut state, &wake);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = runtime.dispatch_signal(POSIX_SIGTERM, SignalTarget::Leader);
                    break;
                }
            }
        }
    }
}

#[cfg(unix)]
fn drain_session_commands(
    rx: &mpsc::Receiver<SessionCommand>,
    runtime: &mut SessionRuntime,
    state: &mut SessionActorLoopState,
    wake: &WakeCallback,
) -> bool {
    loop {
        match rx.try_recv() {
            Ok(command) => {
                if session_actor_handle_command(command, runtime, state, wake) {
                    return true;
                }
            }
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => {
                let _ = runtime.dispatch_signal(POSIX_SIGTERM, SignalTarget::Leader);
                return true;
            }
        }
    }
}

fn session_actor_handle_command(
    command: SessionCommand,
    runtime: &mut SessionRuntime,
    state: &mut SessionActorLoopState,
    wake: &WakeCallback,
) -> bool {
    let mut stop = false;
    match command {
        SessionCommand::Resize { cols, rows, reply } => {
            let cols = cols.max(1);
            let rows = rows.max(1);
            let result = runtime.resize(cols, rows).map(|_| {
                mark_full_and_wake(&mut state.observation, rows, wake);
            });
            let _ = reply.send(result);
        }
        SessionCommand::SetCellSize { cell_width_px, cell_height_px, reply } => {
            let result = runtime.set_cell_size(cell_width_px, cell_height_px).map(|_| {
                let rows = runtime.inspect(false, 0).terminal.rows;
                mark_full_and_wake(&mut state.observation, rows, wake);
            });
            let _ = reply.send(result);
        }
        SessionCommand::SetAttachmentView { id, command, reply } => {
            let _ = reply.send(runtime.set_attachment_view(id, command));
        }
        SessionCommand::CaptureAttachmentView { id, reply } => {
            let result = runtime.capture_attachment_view(id).map(|frame| {
                frame.map(|mut frame| {
                    frame.update.render_generation = state.observation.render_generation;
                    frame
                })
            });
            let _ = reply.send(result);
        }
        SessionCommand::ReleaseAttachmentView { id } => runtime.release_attachment_view(id),
        SessionCommand::Focus { focused, reply } => {
            let _ = reply.send(runtime.focus(focused));
        }
        SessionCommand::WriteInput { bytes, reply } => {
            let _ = reply.send(runtime.write_input(&bytes));
        }
        SessionCommand::Wheel { event, reply } => {
            let result = route_wheel_event_on_actor(wake, runtime, &mut state.observation, event, true);
            let _ = reply.send(result);
        }
        SessionCommand::ApplicationWheel { event, reply } => {
            let result = route_wheel_event_on_actor(wake, runtime, &mut state.observation, event, false);
            let _ = reply.send(result);
        }
        SessionCommand::RetainInputSources { sources, reply } => {
            let _ = reply.send(runtime.retain_input_sources(&sources));
        }
        SessionCommand::Key { source, event, reply } => {
            let _ = reply.send(runtime.key(source, *event));
        }
        SessionCommand::ReleaseInput { source, reply } => {
            let _ = reply.send(runtime.release_input(source));
        }
        SessionCommand::Mouse { source, event, reply } => {
            let _ = reply.send(runtime.mouse(source, event));
        }
        SessionCommand::Paste { text, reply } => {
            let _ = reply.send(route_paste_on_actor(runtime, &text));
        }
        SessionCommand::ScrollViewport { command, reply } => {
            let result = runtime.scroll_viewport(command).inspect(|outcome| {
                if *outcome == ViewportCommandOutcome::Moved {
                    let rows = runtime.inspect(false, 0).terminal.rows;
                    mark_full_and_wake(&mut state.observation, rows, wake);
                }
            });
            let _ = reply.send(result);
        }
        SessionCommand::Snapshot { reply } => {
            sync_terminal_modes_and_wake(runtime, &mut state.observation, wake);
            let result = runtime.snapshot(state.observation.pending_dirty()).map(|mut snapshot| {
                state.observation.annotate_snapshot(&mut snapshot);
                snapshot
            });
            let _ = reply.send(result);
        }
        SessionCommand::RenderUpdate { reply } => {
            sync_terminal_modes_and_wake(runtime, &mut state.observation, wake);
            let observation = &state.observation;
            let result = state.presentation.present(|| {
                runtime.render_update(observation.pending_dirty()).map(|mut update| {
                    observation.annotate_render_update(&mut update);
                    update
                })
            });
            let _ = reply.send(result);
        }
        SessionCommand::PacketRender { full, reply } => {
            if full {
                session_actor_pump(runtime, state, wake);
            }
            sync_terminal_modes_and_wake(runtime, &mut state.observation, wake);
            let result = (|| {
                // The host already holds the retained presentation; send nothing.
                if state.presentation.withholding() {
                    return Ok(None);
                }
                let mut update = runtime.render_update(if full { DirtyState::Full } else { state.observation.pending_dirty() })?;
                if full {
                    update.render_generation = state.observation.render_generation;
                    update.dirty = DirtyState::Full;
                } else {
                    state.observation.annotate_render_update(&mut update);
                }
                let images = state.images.capture(&update.image_resources, |id, generation, callback| {
                    runtime.with_image_resource_data(id, generation, callback)
                })?;
                state.presentation.retain(&update);
                Ok(Some(crate::image_delivery::RenderBundle::live(update, images)))
            })();
            if let Ok(Some(bundle)) = &result {
                // The daemon cache owns this captured generation, independently
                // of when each attachment acknowledges its delivered view.
                state.observation.mark_observed(bundle.packet.update.render_generation);
            }
            let _ = reply.send(result);
        }
        SessionCommand::FullSnapshot { reply } => {
            session_actor_pump(runtime, state, wake);
            let _ = reply.send(runtime.snapshot(DirtyState::Full));
        }
        SessionCommand::ImageResourceData { image_id, generation, mut callback, reply } => {
            let result = runtime.with_image_resource_data(image_id, generation, &mut callback);
            let _ = reply.send(result);
        }
        SessionCommand::Inspect { has_controller, watcher_count, reply } => {
            let _ = reply.send(Ok(runtime.inspect(has_controller, watcher_count)));
        }
        SessionCommand::ApplyAttachState { cols, rows, capabilities, reply } => {
            let cols = cols.max(1);
            let rows = rows.max(1);
            let result = runtime
                .apply_attach_state(cols, rows, &capabilities)
                .inspect(|_| {
                    mark_full_and_wake(&mut state.observation, rows, wake);
                })
                .map(|payload| RawOutputReplay { payload, through_sequence: state.last_raw_output_sequence });
            let _ = reply.send(result);
        }
        SessionCommand::ReplayPayload { capabilities, reply } => {
            let result = runtime
                .replay_payload(&capabilities)
                .map(|payload| RawOutputReplay { payload, through_sequence: state.last_raw_output_sequence });
            let _ = reply.send(result);
        }
        SessionCommand::CaptureText { reply } => {
            let _ = reply.send(runtime.capture_text());
        }
        SessionCommand::ValidateTextMatching { reply } => {
            let _ = reply.send(runtime.validate_text_matching());
        }
        SessionCommand::ScreenContains { text, reply } => {
            let _ = reply.send(Ok(runtime.screen_contains(&text)));
        }
        SessionCommand::LastPtyOutputAt { reply } => {
            let _ = reply.send(Ok(runtime.last_pty_output_at()));
        }
        SessionCommand::FlushScreenActivity => {
            // A held batch's damage belongs to the frame published after it.
            runtime.flush_screen_activity(!state.presentation.is_held());
        }
        SessionCommand::FlushRecording { reply } => {
            runtime.flush_recording();
            let _ = reply.send(Ok(()));
        }
        SessionCommand::RecordingActive { reply } => {
            let _ = reply.send(Ok(runtime.recording_active()));
        }
        SessionCommand::RecordAttach { reply } => {
            runtime.record_attach();
            let _ = reply.send(Ok(()));
        }
        SessionCommand::RecordDetach { reply } => {
            runtime.record_detach();
            let _ = reply.send(Ok(()));
        }
        SessionCommand::WriteInputWithMark { bytes, marker_name, reply } => {
            let _ = reply.send(runtime.write_input_with_mark(&bytes, marker_name));
        }
        SessionCommand::PasteWithMark { text, marker_name, reply } => {
            let result = runtime.encode_paste(&text).and_then(|bytes| runtime.write_input_with_mark(&bytes, marker_name));
            let _ = reply.send(result);
        }
        SessionCommand::SetRecording { enable, reply } => {
            let _ = reply.send(runtime.set_recording(enable));
        }
        SessionCommand::Mark { name, reply } => {
            let _ = reply.send(runtime.mark(name));
        }
        SessionCommand::UpdateTags { add, remove, reply } => {
            let _ = reply.send(Ok(runtime.update_tags(add, remove)));
        }
        SessionCommand::ResolveMarker { name, reply } => {
            let _ = reply.send(Ok(runtime.resolve_marker(&name)));
        }
        SessionCommand::ResolveNextMarker { after, reply } => {
            let _ = reply.send(Ok(runtime.resolve_next_marker_after(after)));
        }
        SessionCommand::DispatchSignal { signal, target, reply } => {
            let _ = reply.send(runtime.dispatch_signal(signal, target));
        }
        SessionCommand::ShouldKeepSessionDir { reply } => {
            let _ = reply.send(Ok(runtime.should_keep_session_dir()));
        }
        SessionCommand::MarkObserved { generation, reply } => {
            let _ = reply.send(mark_observed_and_wake(&mut state.observation, generation, wake));
        }
        SessionCommand::ScrollbackExtent { reply } => {
            let extent = runtime.scrollback_extent().unwrap_or_else(|_| TerminalScrollbackExtent {
                normal_scrollback_rows: 0,
                live_rows: runtime.inspect(false, 0).terminal.rows,
                alternate_screen: false,
            });
            let _ = reply.send(extent);
        }
        SessionCommand::ScrollbarState { reply } => {
            let scrollbar = runtime.scrollbar_state().unwrap_or_else(|_| {
                TerminalScrollbarState::for_live_viewport(TerminalViewportKind::LiveNormal, runtime.inspect(false, 0).terminal.rows)
            });
            let _ = reply.send(scrollbar);
        }
        SessionCommand::SetQueryPassthrough { enabled, reply } => {
            state.queries_forwarded_to_client = enabled;
            let _ = reply.send(Ok(()));
        }
        SessionCommand::SubscribeRawOutput { reply } => {
            let (tx, rx) = mpsc::sync_channel(RAW_OUTPUT_TAP_CHUNKS);
            state.raw_output_taps.push(tx);
            let _ = reply.send(RawOutputTap { rx });
        }
        SessionCommand::RecoverRawOutput { capabilities, reply } => {
            let result = capabilities.iter().map(|capabilities| runtime.replay_payload(capabilities)).collect::<Result<Vec<_>, _>>().map(
                |payloads| {
                    let (tx, rx) = mpsc::sync_channel(RAW_OUTPUT_TAP_CHUNKS);
                    state.raw_output_taps.push(tx);
                    RawOutputRecovery { tap: RawOutputTap { rx }, payloads }
                },
            );
            let _ = reply.send(result);
        }
        SessionCommand::Stop { terminate } => {
            if terminate && !state.exited {
                let _ = runtime.dispatch_signal(POSIX_SIGTERM, SignalTarget::Leader);
            }
            stop = true;
        }
    }
    if !stop {
        session_actor_pump(runtime, state, wake);
    }
    stop
}

#[cfg(not(debug_assertions))]
fn maybe_panic_actor_for_test(_session_id: &str) {}

/// Test hook mirroring `maybe_panic_for_containment_test`: lets a lifecycle
/// test kill the actor worker thread without recording an exit, to exercise
/// the daemon's worker-died fault path.
#[cfg(debug_assertions)]
fn maybe_panic_actor_for_test(session_id: &str) {
    if std::env::var("CLEAT_TEST_PANIC_ACTOR").as_deref() == Ok(session_id) {
        panic!("test-requested actor panic for session {session_id}");
    }
}

fn session_actor_pump(runtime: &mut SessionRuntime, state: &mut SessionActorLoopState, wake: &WakeCallback) {
    match pump_session_runtime(runtime, &mut state.exited, &mut state.exit_code, state.queries_forwarded_to_client) {
        Ok(result) => {
            // Fires only on pumps that read output, so session creation
            // (whose command handling also pumps) completes first.
            if !result.chunks.is_empty() {
                maybe_panic_actor_for_test(runtime.session_id());
            }
            publish_raw_output(&mut state.raw_output_taps, &mut state.last_raw_output_sequence, &result.chunks);
            // Flush actor-side after each pump slice: the servicing loop no
            // longer round-trips into the actor for it, and recording only
            // grows when the pump runs.
            if !result.chunks.is_empty() || state.exited {
                runtime.flush_recording();
            }
            // Before marking damage, so output that opens a batch never wakes.
            reconcile_presentation(runtime, state, wake);
            match result.outcome {
                PumpOutcome::Clean => {}
                PumpOutcome::PartialUnknown => mark_partial_unknown_and_wake(&mut state.observation, wake),
                PumpOutcome::Full => {
                    let rows = runtime.inspect(false, 0).terminal.rows;
                    mark_full_and_wake(&mut state.observation, rows, wake);
                }
            }
        }
        Err(_) => {
            reconcile_presentation(runtime, state, wake);
            let rows = runtime.inspect(false, 0).terminal.rows;
            mark_full_and_wake(&mut state.observation, rows, wake);
        }
    }
    if state.exited {
        if let Some(code) = state.exit_code {
            state.observation.record_exit(code, wake);
        }
    }
    sync_terminal_modes_and_wake(runtime, &mut state.observation, wake);
}

/// Follow the engine's synchronized-output mode at the publication boundary.
/// A batch still open past its deadline, or when the child has exited and can
/// never finish it, is ended in the engine as Ghostty's own timer does.
fn reconcile_presentation(runtime: &mut SessionRuntime, state: &mut SessionActorLoopState, wake: &WakeCallback) {
    let now = Instant::now();
    let mut synchronized = runtime.synchronized_output_active().unwrap_or(false);
    if synchronized && (state.exited || state.presentation.expired(now)) {
        let _ = runtime.end_synchronized_output();
        synchronized = false;
    }
    match state.presentation.reconcile(synchronized, now) {
        GateTransition::Unchanged => {}
        GateTransition::Held => {
            state.observation.set_held(true);
        }
        GateTransition::Released => {
            if state.observation.set_held(false) {
                wake();
            }
        }
    }
}

fn publish_raw_output(taps: &mut Vec<SyncSender<RawOutputChunk>>, last_sequence: &mut u64, chunks: &[Arc<[u8]>]) {
    for bytes in chunks {
        *last_sequence = last_sequence.saturating_add(1);
        let chunk = RawOutputChunk { sequence: *last_sequence, bytes: Arc::clone(bytes) };
        taps.retain(|tap| tap.try_send(chunk.clone()).is_ok());
    }
}

fn route_paste_on_actor(runtime: &mut SessionRuntime, text: &[u8]) -> Result<usize, String> {
    let bytes = runtime.encode_paste(text)?;
    if bytes.is_empty() {
        return Ok(0);
    }
    runtime.write_input(&bytes)?;
    Ok(1)
}

fn route_wheel_event_on_actor(
    wake: &WakeCallback,
    runtime: &mut SessionRuntime,
    observation: &mut ObservationState,
    event: SessionWheelEvent,
    scroll_fallback: bool,
) -> Result<usize, String> {
    let modes = runtime.terminal_mode_state()?;
    if modes.mouse_tracking {
        let bytes = mouse_report_bytes_from_wheel(event, modes).ok_or_else(|| "mouse wheel event cannot be encoded".to_string())?;
        if bytes.is_empty() {
            return Ok(0);
        }
        runtime.write_input(&bytes)?;
        return Ok(1);
    }

    if modes.active_alternate_screen && modes.alternate_scroll {
        let bytes = alternate_scroll_cursor_bytes_from_wheel(event, modes);
        if bytes.is_empty() {
            return Ok(0);
        }
        runtime.write_input(&bytes)?;
        return Ok(1);
    }

    if !scroll_fallback {
        return Ok(0);
    }

    let delta_rows = viewport_delta_rows_from_wheel(event);
    if delta_rows == 0 {
        return Ok(0);
    }
    match runtime.scroll_viewport(ViewportCommand::DeltaRows(delta_rows)) {
        Ok(ViewportCommandOutcome::Moved) => {
            let rows = runtime.inspect(false, 0).terminal.rows;
            mark_full_and_wake(observation, rows, wake);
            Ok(0)
        }
        Ok(ViewportCommandOutcome::NoOp | ViewportCommandOutcome::Unsupported) => Ok(0),
        Err(err) => Err(err),
    }
}

fn wheel_tick_count(delta: f32) -> usize {
    if !delta.is_finite() {
        return 0;
    }
    let rounded = delta.round().abs();
    if rounded == 0.0 {
        0
    } else if rounded > usize::MAX as f32 {
        usize::MAX
    } else {
        rounded as usize
    }
}

fn viewport_delta_rows_from_wheel(event: SessionWheelEvent) -> i64 {
    if !event.wheel_delta_y.is_finite() {
        return 0;
    }
    let rounded = event.wheel_delta_y.round();
    if rounded == 0.0 {
        return 0;
    }
    let rows = if rounded > i64::MAX as f32 {
        i64::MAX
    } else if rounded < i64::MIN as f32 {
        i64::MIN
    } else {
        rounded as i64
    };
    rows.saturating_neg()
}

pub(crate) fn alternate_scroll_cursor_bytes_from_wheel(event: SessionWheelEvent, modes: vt::TerminalModeState) -> Vec<u8> {
    let count = wheel_tick_count(event.wheel_delta_y);
    if count == 0 {
        return Vec::new();
    }
    let seq = if event.wheel_delta_y.is_sign_positive() {
        if modes.application_cursor_keys {
            b"\x1bOA".as_slice()
        } else {
            b"\x1b[A".as_slice()
        }
    } else if modes.application_cursor_keys {
        b"\x1bOB".as_slice()
    } else {
        b"\x1b[B".as_slice()
    };
    seq.repeat(count)
}

pub(crate) fn mouse_report_bytes_from_wheel(event: SessionWheelEvent, modes: vt::TerminalModeState) -> Option<Vec<u8>> {
    let y_count = wheel_tick_count(event.wheel_delta_y);
    let x_count = wheel_tick_count(event.wheel_delta_x);
    if y_count == 0 && x_count == 0 {
        return Some(Vec::new());
    }

    let mut out = Vec::new();
    let modifiers = mouse_report_modifier_code(event.modifiers);
    if y_count > 0 {
        let button = if event.wheel_delta_y.is_sign_positive() { 64 } else { 65 } + modifiers;
        append_mouse_report(&mut out, button, y_count, event, modes)?;
    }
    if x_count > 0 {
        let button = if event.wheel_delta_x.is_sign_positive() { 66 } else { 67 } + modifiers;
        append_mouse_report(&mut out, button, x_count, event, modes)?;
    }
    Some(out)
}

fn append_mouse_report(
    out: &mut Vec<u8>,
    button_code: u16,
    count: usize,
    event: SessionWheelEvent,
    modes: vt::TerminalModeState,
) -> Option<()> {
    let (x, y) = if modes.mouse_sgr_pixels {
        (pixel_coordinate(event.x_px), pixel_coordinate(event.y_px))
    } else {
        (u32::from(event.cell_col) + 1, u32::from(event.cell_row) + 1)
    };

    for _ in 0..count {
        if modes.mouse_sgr || modes.mouse_sgr_pixels {
            out.extend_from_slice(format!("\x1b[<{button_code};{x};{y}M").as_bytes());
        } else {
            let b = legacy_mouse_byte(button_code)?;
            let x = legacy_mouse_byte(u16::try_from(x).ok()?)?;
            let y = legacy_mouse_byte(u16::try_from(y).ok()?)?;
            out.extend_from_slice(&[b'\x1b', b'[', b'M', b, x, y]);
        }
    }
    Some(())
}

fn mouse_report_modifier_code(modifiers: vt::MouseModifiers) -> u16 {
    let mut code = 0;
    if modifiers.shift {
        code += 4;
    }
    if modifiers.alt {
        code += 8;
    }
    if modifiers.ctrl {
        code += 16;
    }
    code
}

fn pixel_coordinate(value: f32) -> u32 {
    if !value.is_finite() || value <= 0.0 {
        0
    } else if value >= u32::MAX as f32 {
        u32::MAX
    } else {
        value.round() as u32
    }
}

fn legacy_mouse_byte(value: u16) -> Option<u8> {
    let encoded = value.checked_add(32)?;
    u8::try_from(encoded).ok()
}

#[cfg(all(test, any(unix, windows)))]
mod idle_tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::vt::{ClientCapabilities, ScreenGrid, VtEngine};

    struct ObservedEngine {
        inner: Box<dyn VtEngine>,
        polls: Arc<AtomicU64>,
        dropped: Arc<AtomicBool>,
    }
    impl Drop for ObservedEngine {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }
    impl VtEngine for ObservedEngine {
        fn feed(&mut self, b: &[u8]) -> Result<(), String> {
            self.inner.feed(b)
        }
        fn resize(&mut self, c: u16, r: u16) -> Result<(), String> {
            self.inner.resize(c, r)
        }
        fn supports_replay(&self) -> bool {
            self.inner.supports_replay()
        }
        fn replay_payload(&self, c: &ClientCapabilities) -> Result<Option<Vec<u8>>, String> {
            self.inner.replay_payload(c)
        }
        fn screen_text(&self) -> Result<String, String> {
            self.inner.screen_text()
        }
        fn screen_grid(&mut self) -> Result<ScreenGrid, String> {
            self.inner.screen_grid()
        }
        fn size(&self) -> (u16, u16) {
            self.inner.size()
        }
        fn terminal_mode_state(&self) -> Result<vt::TerminalModeState, String> {
            self.polls.fetch_add(1, Ordering::SeqCst);
            self.inner.terminal_mode_state()
        }
    }

    #[test]
    fn exited_session_is_idle_but_final_output_remains_queryable() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        let polls = Arc::new(AtomicU64::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let engine_polls = polls.clone();
        let engine_dropped = dropped.clone();
        #[cfg(unix)]
        let command = "printf final-output; exit 7";
        #[cfg(windows)]
        let command = "echo final-output & exit /b 7";
        let actor = SessionActor::spawn(24, Arc::new(|| {}), move || {
            let session = crate::runtime::SessionMetadata {
                id: "exited-idle".into(),
                vt_engine: vt::default_vt_engine_kind(),
                cwd: None,
                cmd: Some(command.into()),
                tags: vec![],
                environment: vec![],
                record: false,
                initial_size: Default::default(),
                colors: Default::default(),
            };
            SessionRuntime::spawn(
                dir,
                &session,
                Box::new(ObservedEngine { inner: vt::make_default_vt_engine(80, 24), polls: engine_polls, dropped: engine_dropped }),
            )
        })
        .unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while actor.observation().exit_code().is_none() {
            assert!(Instant::now() < deadline, "child did not exit");
            thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(actor.observation().exit_code(), Some(7));
        actor.inspect(false, 0).unwrap();
        #[cfg(feature = "ghostty-vt")]
        assert!(actor.capture_text().unwrap().contains("final-output"));
        // Let the command's final pump complete, then observe an idle interval.
        thread::sleep(std::time::Duration::from_millis(30));
        let before = polls.load(Ordering::SeqCst);
        thread::sleep(std::time::Duration::from_millis(100));
        let after = polls.load(Ordering::SeqCst);
        // Still serve commands after parking and release the engine on close.
        actor.inspect(false, 0).unwrap();
        #[cfg(feature = "ghostty-vt")]
        assert!(actor.capture_text().unwrap().contains("final-output"));
        drop(actor);
        assert!(dropped.load(Ordering::SeqCst));
        assert_eq!(after, before, "exited actor continued polling while idle");
    }
}

/// End-to-end synchronized-output publication through a real PTY child.
#[cfg(all(test, unix, feature = "ghostty-vt"))]
mod synchronized_output_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn spawn(id: &str, command: &str) -> (tempfile::TempDir, SessionActor, Arc<AtomicUsize>) {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        let wakes = Arc::new(AtomicUsize::new(0));
        let counter = wakes.clone();
        let (id, command) = (id.to_string(), command.to_string());
        let actor = SessionActor::spawn(
            3,
            Arc::new(move || {
                counter.fetch_add(1, Ordering::SeqCst);
            }),
            move || {
                let session = crate::runtime::SessionMetadata {
                    id,
                    vt_engine: vt::VtEngineKind::Ghostty,
                    cwd: None,
                    cmd: Some(command),
                    tags: vec![],
                    environment: vec![],
                    record: false,
                    initial_size: Default::default(),
                    colors: Default::default(),
                };
                SessionRuntime::spawn(dir, &session, vt::make_default_vt_engine(20, 3))
            },
        )
        .unwrap();
        (temp, actor, wakes)
    }

    fn wait_until(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + timeout;
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn render(actor: &SessionActor) -> TerminalRenderUpdate {
        actor.request_result(|reply| SessionCommand::RenderUpdate { reply }).unwrap()
    }

    fn observe(actor: &SessionActor, update: &TerminalRenderUpdate) {
        let generation = update.render_generation;
        assert!(actor.request(|reply| SessionCommand::MarkObserved { generation, reply }, false));
    }

    fn text(update: &TerminalRenderUpdate) -> String {
        update
            .ops
            .iter()
            .flat_map(|op| &op.rows)
            .flat_map(|row| &row.cells)
            .flat_map(|cell| cell.graphemes.iter().copied())
            .filter_map(char::from_u32)
            .collect()
    }

    /// Render and observe until `ready` holds, as a polling host does.
    fn present_until(actor: &SessionActor, what: &str, ready: impl Fn(&TerminalRenderUpdate) -> bool) -> TerminalRenderUpdate {
        let mut seen = None;
        wait_until(what, Duration::from_secs(10), || {
            if actor.observation().dirty() == DirtyState::Clean && seen.is_some() {
                return false;
            }
            let update = render(actor);
            observe(actor, &update);
            let done = ready(&update);
            seen = Some(update);
            done
        });
        seen.unwrap()
    }

    #[test]
    fn batch_split_across_reads_publishes_only_completed_frames() {
        let (_temp, actor, wakes) = spawn(
            "sync-split",
            r"printf 'prompt> \033[?25h'; sleep 0.3; printf '\033[?2026h\033[?25lMID'; sleep 0.8; printf '\033[?25h\033[?2026l'; sleep 10",
        );
        let first = present_until(&actor, "initial prompt", |update| text(update).contains("prompt>") && update.cursor.visible);
        wait_until("mid-batch output parsed", Duration::from_secs(10), || actor.capture_text().unwrap().contains("MID"));
        let wakes_mid_batch = wakes.load(Ordering::SeqCst);

        // Parsed but not published: pollers see nothing new, and a render
        // serves the retained frame without acknowledging pending damage.
        assert_eq!(actor.observation().dirty(), DirtyState::Clean);
        let held = render(&actor);
        assert!(held.cursor.visible, "published the batch's hidden cursor");
        assert!(held.ops.is_empty());
        assert_eq!(held.render_generation, first.render_generation, "a withheld frame is not a new presentation");
        observe(&actor, &held);
        assert!(actor.packet_render(false).unwrap().is_none(), "packet path published mid-batch");
        assert_eq!(actor.observation().dirty(), DirtyState::Clean);
        assert_eq!(wakes.load(Ordering::SeqCst), wakes_mid_batch, "woke the host for a withheld frame");

        // The batch ends: one wake, and the completed frame carries the
        // damage withheld earlier.
        wait_until("wake at batch end", Duration::from_secs(10), || actor.observation().dirty() != DirtyState::Clean);
        assert!(wakes.load(Ordering::SeqCst) > wakes_mid_batch);
        let completed = render(&actor);
        assert!(completed.cursor.visible);
        assert!(text(&completed).contains("MID"), "completed frame lost mid-batch damage: {:?}", text(&completed));
        assert!(completed.render_generation > held.render_generation);
        observe(&actor, &completed);
    }

    #[test]
    fn packet_render_withholds_mid_batch_and_resumes_after() {
        let (_temp, actor, _wakes) =
            spawn("sync-packet", r"printf 'ready'; sleep 0.3; printf '\033[?2026h\033[?25lMID'; sleep 0.8; printf '\033[?2026l'; sleep 10");
        wait_until("initial output", Duration::from_secs(10), || actor.capture_text().unwrap().contains("ready"));
        let initial = actor.packet_render(true).unwrap().expect("first frame is rendered");
        wait_until("mid-batch output parsed", Duration::from_secs(10), || actor.capture_text().unwrap().contains("MID"));
        assert!(actor.packet_render(false).unwrap().is_none());
        assert!(actor.packet_render(true).unwrap().is_none(), "full render must use the host's retained frame");
        wait_until("batch end", Duration::from_secs(10), || actor.observation().dirty() != DirtyState::Clean);
        let completed = actor.packet_render(false).unwrap().expect("completed frame is published");
        assert!(completed.packet.update.render_generation > initial.packet.update.render_generation);
        assert!(!completed.packet.update.cursor.visible, "the program left its cursor hidden");
    }

    #[test]
    fn abandoned_batch_is_published_at_the_deadline_without_further_output() {
        let (_temp, actor, wakes) =
            spawn("sync-abandoned", r"printf 'ready\033[?25h'; sleep 0.3; printf '\033[?2026h\033[?25lstuck'; sleep 10");
        present_until(&actor, "initial frame", |update| text(update).contains("ready"));
        wait_until("batch output parsed", Duration::from_secs(10), || actor.capture_text().unwrap().contains("stuck"));
        let parsed_at = Instant::now();
        let wakes_before = wakes.load(Ordering::SeqCst);
        assert_eq!(actor.observation().dirty(), DirtyState::Clean);

        wait_until("deadline wake", Duration::from_secs(5), || wakes.load(Ordering::SeqCst) > wakes_before);
        let waited = parsed_at.elapsed();
        assert!(waited >= Duration::from_millis(900), "released before the deadline: {waited:?}");
        assert_ne!(actor.observation().dirty(), DirtyState::Clean);
        let update = render(&actor);
        assert!(text(&update).contains("stuck"));
        assert!(!update.cursor.visible, "the abandoned batch's own state is shown");
    }

    #[test]
    fn exit_mid_batch_publishes_final_state() {
        let (_temp, actor, _wakes) = spawn("sync-exit", r"printf 'ready'; sleep 0.3; printf '\033[?2026h\033[?25lgone'; sleep 0.2; exit 0");
        present_until(&actor, "initial frame", |update| text(update).contains("ready"));
        wait_until("exit", Duration::from_secs(10), || actor.observation().exit_code().is_some());
        let started = Instant::now();
        let update = present_until(&actor, "final frame", |update| text(update).contains("gone"));
        assert!(started.elapsed() < Duration::from_millis(900), "exit waited for the deadline");
        assert!(!update.cursor.visible);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    fn counting_wake() -> (WakeCallback, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&count);
        let wake: WakeCallback = Arc::new(move || {
            counter.fetch_add(1, AtomicOrdering::SeqCst);
        });
        (wake, count)
    }

    #[test]
    fn stale_mark_observed_rewakes_when_output_arrived_after_render() {
        let (wake, wakes) = counting_wake();
        let mut observation = ObservationState::new(24);
        let initial = observation.render_generation;
        assert!(mark_observed_and_wake(&mut observation, initial, &wake));
        assert_eq!(observation.dirty(), DirtyState::Clean);
        assert_eq!(wakes.load(AtomicOrdering::SeqCst), 0, "observing the latest render needs no wake");

        // Output makes the session dirty: the clean-to-dirty edge wakes the host.
        mark_partial_unknown_and_wake(&mut observation, &wake);
        assert_eq!(wakes.load(AtomicOrdering::SeqCst), 1);

        // The host renders this generation...
        let mut update = TerminalRenderUpdate::default();
        observation.annotate_render_update(&mut update);
        let rendered = update.render_generation;

        // ...more output lands before its mark_observed. Already dirty, so no edge.
        mark_partial_unknown_and_wake(&mut observation, &wake);
        assert_eq!(wakes.load(AtomicOrdering::SeqCst), 1);

        // The stale observation leaves the session dirty and must wake again,
        // or the host never renders the newer output.
        assert!(mark_observed_and_wake(&mut observation, rendered, &wake));
        assert_eq!(observation.dirty(), DirtyState::Partial);
        assert_eq!(wakes.load(AtomicOrdering::SeqCst), 2, "stale observation must re-arm the wake");

        // Observing the newer render cleans the session without a spurious wake.
        let latest = observation.render_generation;
        assert!(mark_observed_and_wake(&mut observation, latest, &wake));
        assert_eq!(observation.dirty(), DirtyState::Clean);
        assert_eq!(wakes.load(AtomicOrdering::SeqCst), 2);
    }

    #[test]
    fn rejected_mark_observed_does_not_wake() {
        let (wake, wakes) = counting_wake();
        let mut observation = ObservationState::new(24);
        let future = observation.render_generation + 1;
        assert!(!mark_observed_and_wake(&mut observation, future, &wake));
        assert_eq!(wakes.load(AtomicOrdering::SeqCst), 0);
    }
}
