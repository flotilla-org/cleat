#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::{
    io,
    sync::{
        atomic::{AtomicI64, AtomicU64, AtomicU8, Ordering as AtomicOrdering},
        mpsc,
        mpsc::{Receiver, SyncSender},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

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

use crate::{
    platform::pty::PtyChild,
    protocol::{InspectResult, SignalTarget},
    provider::{
        DirtyState, TerminalRenderUpdate, TerminalScrollbackExtent, TerminalScrollbarState, TerminalSnapshot, TerminalViewportKind,
        ViewportCommand, ViewportCommandOutcome,
    },
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
    pub(crate) bytes: Vec<u8>,
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
            mirror,
        };
        state.mark_full(rows);
        state
    }

    pub(crate) fn dirty(&self) -> DirtyState {
        if self.render_generation > self.observed_generation {
            self.dirty
        } else {
            DirtyState::Clean
        }
    }

    pub(crate) fn mark_full(&mut self, _rows: u16) -> bool {
        let was_clean = self.dirty() == DirtyState::Clean;
        self.render_generation = self.render_generation.saturating_add(1);
        self.dirty = DirtyState::Full;
        self.dirty_rows.clear();
        self.publish_mirror();
        was_clean
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
        was_clean
    }

    pub(crate) fn mark_partial_unknown(&mut self) -> bool {
        let was_clean = self.dirty() == DirtyState::Clean;
        self.render_generation = self.render_generation.saturating_add(1);
        if self.dirty != DirtyState::Full {
            self.dirty = DirtyState::Partial;
            self.dirty_rows.clear();
        }
        self.publish_mirror();
        was_clean
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
        snapshot.dirty = self.dirty();
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
        let dirty = self.dirty();
        if dirty == DirtyState::Clean {
            update.dirty = DirtyState::Clean;
            update.ops.clear();
        } else if update.dirty == DirtyState::Clean {
            update.dirty = dirty;
        }
    }

    fn publish_mirror(&self) {
        if let Some(mirror) = &self.mirror {
            mirror.store(self.render_generation, self.observed_generation, self.dirty);
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
    Resize { cols: u16, rows: u16, reply: mpsc::Sender<Result<(), String>> },
    SetCellSize { cell_width_px: u32, cell_height_px: u32, reply: mpsc::Sender<Result<(), String>> },
    WriteInput { bytes: Vec<u8>, reply: mpsc::Sender<Result<(), String>> },
    Wheel { event: SessionWheelEvent, reply: mpsc::Sender<Result<usize, String>> },
    Mouse { event: SessionMouseEvent, reply: mpsc::Sender<Result<usize, String>> },
    Paste { text: Vec<u8>, reply: mpsc::Sender<Result<usize, String>> },
    ScrollViewport { command: ViewportCommand, reply: mpsc::Sender<Result<ViewportCommandOutcome, String>> },
    Snapshot { reply: mpsc::Sender<Result<TerminalSnapshot, String>> },
    RenderUpdate { reply: mpsc::Sender<Result<TerminalRenderUpdate, String>> },
    FullRenderUpdate { reply: mpsc::Sender<Result<TerminalRenderUpdate, String>> },
    FullSnapshot { reply: mpsc::Sender<Result<TerminalSnapshot, String>> },
    ImageResourceData { image_id: u32, generation: u64, callback: ImageResourceDataCallback, reply: mpsc::Sender<Result<bool, String>> },
    Inspect { has_controller: bool, watcher_count: usize, reply: mpsc::Sender<Result<InspectResult, String>> },
    ApplyAttachState { cols: u16, rows: u16, capabilities: vt::ClientCapabilities, reply: mpsc::Sender<Result<RawOutputReplay, String>> },
    ReplayPayload { capabilities: vt::ClientCapabilities, reply: mpsc::Sender<Result<RawOutputReplay, String>> },
    CaptureText { reply: mpsc::Sender<Result<String, String>> },
    ValidateTextMatching { reply: mpsc::Sender<Result<(), String>> },
    ScreenContains { text: String, reply: mpsc::Sender<Result<bool, String>> },
    LastPtyOutputAt { reply: mpsc::Sender<Result<Option<Instant>, String>> },
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
    SetClientPresence { active: bool, reply: mpsc::Sender<Result<(), String>> },
    SubscribeRawOutput { reply: mpsc::Sender<RawOutputTap> },
    RecoverRawOutput { capabilities: Vec<vt::ClientCapabilities>, reply: mpsc::Sender<Result<RawOutputRecovery, String>> },
    Stop { terminate: bool },
}

pub(crate) struct SessionActor {
    tx: CommandSender,
    observation: Arc<ObservationMirror>,
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
fn wait_actor_ready(pty_child: &PtyChild, command_fd: RawFd) -> Result<ActorReadiness, String> {
    #[cfg(target_os = "macos")]
    {
        wait_actor_ready_kqueue(pty_child.master_fd(), command_fd)
    }
    #[cfg(target_os = "linux")]
    {
        wait_actor_ready_epoll(pty_child.master_fd(), command_fd)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        wait_actor_ready_poll(pty_child.master_fd(), command_fd)
    }
}

#[cfg(target_os = "macos")]
fn wait_actor_ready_kqueue(pty_fd: RawFd, command_fd: RawFd) -> Result<ActorReadiness, String> {
    const TOKEN_PTY: isize = 1;
    const TOKEN_COMMAND: isize = 2;

    let kqueue = Kqueue::new().map_err(|err| format!("create actor kqueue: {err}"))?;
    let changes = [
        KEvent::new(pty_fd as _, EventFilter::EVFILT_READ, EvFlags::EV_ADD, FilterFlag::empty(), 0, TOKEN_PTY),
        KEvent::new(command_fd as _, EventFilter::EVFILT_READ, EvFlags::EV_ADD, FilterFlag::empty(), 0, TOKEN_COMMAND),
    ];
    let mut events = [KEvent::new(0, EventFilter::EVFILT_READ, EvFlags::empty(), FilterFlag::empty(), 0, 0); 2];
    let event_count = loop {
        match kqueue.kevent(&changes, &mut events, None) {
            Ok(event_count) => break event_count,
            Err(Errno::EINTR) => continue,
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
fn wait_actor_ready_epoll(pty_fd: RawFd, command_fd: RawFd) -> Result<ActorReadiness, String> {
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
        match epoll.wait(&mut events, nix::poll::PollTimeout::NONE) {
            Ok(event_count) => break event_count,
            Err(Errno::EINTR) => continue,
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
fn wait_actor_ready_poll(pty_fd: RawFd, command_fd: RawFd) -> Result<ActorReadiness, String> {
    let mut fds = [
        PollFd::new(unsafe { std::os::fd::BorrowedFd::borrow_raw(pty_fd) }, PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR),
        PollFd::new(
            unsafe { std::os::fd::BorrowedFd::borrow_raw(command_fd) },
            PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR,
        ),
    ];
    loop {
        match poll(&mut fds, PollTimeout::NONE) {
            Ok(_) => break,
            Err(Errno::EINTR) => continue,
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
            Ok(()) => Ok(Self { tx, observation, worker: Some(worker) }),
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
        Self { tx: CommandSender::inert(tx), observation, worker }
    }

    pub(crate) fn observation(&self) -> &ObservationMirror {
        &self.observation
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

    pub(crate) fn set_client_presence(&self, active: bool) -> Result<(), String> {
        self.request_result(|reply| SessionCommand::SetClientPresence { active, reply })
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

    pub(crate) fn wheel(&self, event: SessionWheelEvent) -> Result<usize, String> {
        self.request_result(|reply| SessionCommand::Wheel { event, reply })
    }

    pub(crate) fn mouse(&self, event: SessionMouseEvent) -> Result<usize, String> {
        self.request_result(|reply| SessionCommand::Mouse { event, reply })
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

    pub(crate) fn render_update(&self) -> Result<TerminalRenderUpdate, String> {
        self.request_result(|reply| SessionCommand::RenderUpdate { reply })
    }

    pub(crate) fn full_render_update(&self) -> Result<TerminalRenderUpdate, String> {
        self.request_result(|reply| SessionCommand::FullRenderUpdate { reply })
    }

    pub(crate) fn mark_observed(&self, generation: u64) -> bool {
        self.request(|reply| SessionCommand::MarkObserved { generation, reply }, false)
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
    chunks: Vec<Vec<u8>>,
}

struct SessionActorLoopState {
    observation: ObservationState,
    exited: bool,
    exit_code: Option<i32>,
    has_active_client: bool,
    raw_output_taps: Vec<SyncSender<RawOutputChunk>>,
    last_raw_output_sequence: u64,
}

fn pump_session_runtime(
    runtime: &mut SessionRuntime,
    exited: &mut bool,
    exit_code: &mut Option<i32>,
    has_active_client: bool,
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
        runtime.drain_output_after_exit(has_active_client)?
    } else if *exited {
        PtyOutput { chunks: Vec::new() }
    } else {
        runtime.read_available_output(has_active_client)?
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
    ready: mpsc::Sender<Result<(), String>>,
    rx: mpsc::Receiver<SessionCommand>,
    #[cfg(unix)] command_wake: CommandWakeReader,
) {
    let mut state = SessionActorLoopState {
        observation: ObservationState::new_with_mirror(rows, Some(mirror)),
        exited: false,
        exit_code: None,
        has_active_client: false,
        raw_output_taps: Vec::new(),
        last_raw_output_sequence: 0,
    };
    let _ = ready.send(Ok(()));
    #[cfg(unix)]
    loop {
        let readiness = match wait_actor_ready(runtime.pty_child(), command_wake.raw_fd()) {
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
        }
    }
    #[cfg(not(unix))]
    loop {
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
        SessionCommand::WriteInput { bytes, reply } => {
            let _ = reply.send(runtime.write_input(&bytes));
        }
        SessionCommand::Wheel { event, reply } => {
            let result = route_wheel_event_on_actor(wake, runtime, &mut state.observation, event);
            let _ = reply.send(result);
        }
        SessionCommand::Mouse { event, reply } => {
            let _ = reply.send(route_mouse_event_on_actor(runtime, event));
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
            let result = runtime.snapshot(state.observation.dirty()).map(|mut snapshot| {
                state.observation.annotate_snapshot(&mut snapshot);
                snapshot
            });
            let _ = reply.send(result);
        }
        SessionCommand::RenderUpdate { reply } => {
            sync_terminal_modes_and_wake(runtime, &mut state.observation, wake);
            let result = runtime.render_update(state.observation.dirty()).map(|mut update| {
                state.observation.annotate_render_update(&mut update);
                update
            });
            let _ = reply.send(result);
        }
        SessionCommand::FullRenderUpdate { reply } => {
            session_actor_pump(runtime, state, wake);
            sync_terminal_modes_and_wake(runtime, &mut state.observation, wake);
            let result = runtime.render_update(DirtyState::Full).map(|mut update| {
                update.render_generation = state.observation.render_generation;
                update.dirty = DirtyState::Full;
                update
            });
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
            let _ = reply.send(state.observation.mark_observed(generation));
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
        SessionCommand::SetClientPresence { active, reply } => {
            state.has_active_client = active;
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
    match pump_session_runtime(runtime, &mut state.exited, &mut state.exit_code, state.has_active_client) {
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

fn publish_raw_output(taps: &mut Vec<SyncSender<RawOutputChunk>>, last_sequence: &mut u64, chunks: &[Vec<u8>]) {
    for bytes in chunks {
        *last_sequence = last_sequence.saturating_add(1);
        let chunk = RawOutputChunk { sequence: *last_sequence, bytes: bytes.clone() };
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

fn route_mouse_event_on_actor(runtime: &mut SessionRuntime, event: SessionMouseEvent) -> Result<usize, String> {
    let bytes = runtime.encode_mouse(event.action, event.button, event.any_button_pressed, event.modifiers, event.x_px, event.y_px)?;
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
        (one_based_coordinate(event.x_px), one_based_coordinate(event.y_px))
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

fn one_based_coordinate(value: f32) -> u32 {
    if !value.is_finite() || value <= 0.0 {
        1
    } else if value >= u32::MAX as f32 {
        u32::MAX
    } else {
        value.round() as u32 + 1
    }
}

fn legacy_mouse_byte(value: u16) -> Option<u8> {
    let encoded = value.checked_add(32)?;
    u8::try_from(encoded).ok()
}
