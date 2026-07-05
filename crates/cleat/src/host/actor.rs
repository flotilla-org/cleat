#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::{
    io,
    sync::{
        atomic::{AtomicU64, AtomicU8, Ordering as AtomicOrdering},
        mpsc,
        mpsc::{Receiver, SyncSender},
        Arc,
    },
    thread,
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
    protocol::SignalTarget,
    provider::{
        DirtyState, TerminalRenderUpdate, TerminalScrollbackExtent, TerminalScrollbarState, TerminalSnapshot, TerminalViewportKind,
        ViewportCommand, ViewportCommandOutcome,
    },
    session_runtime::{PtyOutput, SessionRuntime},
    vt,
};

const POSIX_SIGTERM: i32 = 15;
const RAW_OUTPUT_TAP_CHUNKS: usize = 64;

pub(crate) type WakeCallback = Arc<dyn Fn() + Send + Sync + 'static>;
pub(crate) type ImageResourceDataCallback = Box<dyn FnMut(&[u8]) -> bool + Send>;

#[allow(dead_code)]
pub(crate) struct RawOutputTap {
    rx: Receiver<Vec<u8>>,
}

impl RawOutputTap {
    #[allow(dead_code)]
    pub(crate) fn try_recv(&self) -> Result<Vec<u8>, mpsc::TryRecvError> {
        self.rx.try_recv()
    }
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

#[derive(Debug)]
pub(crate) struct ObservationMirror {
    render_generation: AtomicU64,
    observed_generation: AtomicU64,
    dirty: AtomicU8,
}

impl ObservationMirror {
    pub(crate) fn new() -> Self {
        Self {
            render_generation: AtomicU64::new(0),
            observed_generation: AtomicU64::new(0),
            dirty: AtomicU8::new(dirty_state_to_u8(DirtyState::Clean)),
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
    ImageResourceData { image_id: u32, generation: u64, callback: ImageResourceDataCallback, reply: mpsc::Sender<Result<bool, String>> },
    MarkObserved { generation: u64, reply: mpsc::Sender<bool> },
    ScrollbackExtent { reply: mpsc::Sender<TerminalScrollbackExtent> },
    ScrollbarState { reply: mpsc::Sender<TerminalScrollbarState> },
    SetClientPresence { active: bool, reply: mpsc::Sender<()> },
    SubscribeRawOutput { reply: mpsc::Sender<RawOutputTap> },
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
    epoll
        .add(unsafe { std::os::fd::BorrowedFd::borrow_raw(pty_fd) }, EpollEvent::new(EpollFlags::EPOLLIN, TOKEN_PTY))
        .map_err(|err| format!("register pty with actor epoll: {err}"))?;
    epoll
        .add(unsafe { std::os::fd::BorrowedFd::borrow_raw(command_fd) }, EpollEvent::new(EpollFlags::EPOLLIN, TOKEN_COMMAND))
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
        if event.events().contains(EpollFlags::EPOLLIN) {
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
        PollFd::new(unsafe { std::os::fd::BorrowedFd::borrow_raw(pty_fd) }, PollFlags::POLLIN),
        PollFd::new(unsafe { std::os::fd::BorrowedFd::borrow_raw(command_fd) }, PollFlags::POLLIN),
    ];
    loop {
        match poll(&mut fds, PollTimeout::NONE) {
            Ok(_) => break,
            Err(Errno::EINTR) => continue,
            Err(err) => return Err(format!("poll actor fds: {err}")),
        }
    }
    Ok(ActorReadiness {
        pty_readable: fds[0].revents().unwrap_or_else(PollFlags::empty).contains(PollFlags::POLLIN),
        command_readable: fds[1].revents().unwrap_or_else(PollFlags::empty).contains(PollFlags::POLLIN),
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

    pub(crate) fn request<T>(&self, make_command: impl FnOnce(mpsc::Sender<T>) -> SessionCommand, fallback: T) -> T {
        let (reply, recv) = mpsc::channel();
        if self.tx.send(make_command(reply)).is_err() {
            return fallback;
        }
        recv.recv().unwrap_or(fallback)
    }

    pub(crate) fn request_result<T>(
        &self,
        make_command: impl FnOnce(mpsc::Sender<Result<T, String>>) -> SessionCommand,
    ) -> Result<T, String> {
        let (reply, recv) = mpsc::channel();
        self.tx.send(make_command(reply)).map_err(|_| "session actor is not running".to_string())?;
        recv.recv().map_err(|_| "session actor did not reply".to_string())?
    }

    pub(crate) fn set_client_presence(&self, active: bool) -> Result<(), String> {
        let (reply, recv) = mpsc::channel();
        self.tx.send(SessionCommand::SetClientPresence { active, reply }).map_err(|_| "session actor is not running".to_string())?;
        recv.recv().map_err(|_| "session actor did not reply".to_string())
    }

    #[allow(dead_code)]
    pub(crate) fn subscribe_raw_output(&self) -> Result<RawOutputTap, String> {
        let (reply, recv) = mpsc::channel();
        self.tx.send(SessionCommand::SubscribeRawOutput { reply }).map_err(|_| "session actor is not running".to_string())?;
        recv.recv().map_err(|_| "session actor did not reply".to_string())
    }
}

impl Drop for SessionActor {
    fn drop(&mut self) {
        let _ = self.tx.send(SessionCommand::Stop { terminate: true });
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
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

fn pump_session_runtime(runtime: &mut SessionRuntime, exited: &mut bool, has_active_client: bool) -> Result<PumpResult, String> {
    let mut exited_now = false;
    if !*exited {
        if let Some(exit_code) = runtime.exit_code_if_exited()? {
            runtime.record_exit_code(exit_code);
            *exited = true;
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
    let mut observation = ObservationState::new_with_mirror(rows, Some(mirror));
    let mut exited = false;
    let mut has_active_client = false;
    let mut raw_output_taps = Vec::new();
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
            if drain_session_commands(&rx, &mut runtime, &mut observation, &mut exited, &mut has_active_client, &mut raw_output_taps, &wake)
            {
                break;
            }
        }
        if readiness.pty_readable {
            session_actor_pump(&mut runtime, &mut observation, &mut exited, has_active_client, &mut raw_output_taps, &wake);
        }
    }
    #[cfg(not(unix))]
    loop {
        match rx.recv_timeout(std::time::Duration::from_millis(10)) {
            Ok(command) => {
                if session_actor_handle_command(
                    command,
                    &mut runtime,
                    &mut observation,
                    &mut exited,
                    &mut has_active_client,
                    &mut raw_output_taps,
                    &wake,
                ) {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                session_actor_pump(&mut runtime, &mut observation, &mut exited, has_active_client, &mut raw_output_taps, &wake);
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
    observation: &mut ObservationState,
    exited: &mut bool,
    has_active_client: &mut bool,
    raw_output_taps: &mut Vec<SyncSender<Vec<u8>>>,
    wake: &WakeCallback,
) -> bool {
    loop {
        match rx.try_recv() {
            Ok(command) => {
                if session_actor_handle_command(command, runtime, observation, exited, has_active_client, raw_output_taps, wake) {
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
    observation: &mut ObservationState,
    exited: &mut bool,
    has_active_client: &mut bool,
    raw_output_taps: &mut Vec<SyncSender<Vec<u8>>>,
    wake: &WakeCallback,
) -> bool {
    let mut stop = false;
    match command {
        SessionCommand::Resize { cols, rows, reply } => {
            let cols = cols.max(1);
            let rows = rows.max(1);
            let result = runtime.resize(cols, rows).map(|_| {
                mark_full_and_wake(observation, rows, wake);
            });
            let _ = reply.send(result);
        }
        SessionCommand::SetCellSize { cell_width_px, cell_height_px, reply } => {
            let result = runtime.set_cell_size(cell_width_px, cell_height_px).map(|_| {
                let rows = runtime.inspect(false, 0).terminal.rows;
                mark_full_and_wake(observation, rows, wake);
            });
            let _ = reply.send(result);
        }
        SessionCommand::WriteInput { bytes, reply } => {
            let _ = reply.send(runtime.write_input(&bytes));
        }
        SessionCommand::Wheel { event, reply } => {
            let result = route_wheel_event_on_actor(wake, runtime, observation, event);
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
                    mark_full_and_wake(observation, rows, wake);
                }
            });
            let _ = reply.send(result);
        }
        SessionCommand::Snapshot { reply } => {
            sync_terminal_modes_and_wake(runtime, observation, wake);
            let result = runtime.snapshot(observation.dirty()).map(|mut snapshot| {
                observation.annotate_snapshot(&mut snapshot);
                snapshot
            });
            let _ = reply.send(result);
        }
        SessionCommand::RenderUpdate { reply } => {
            sync_terminal_modes_and_wake(runtime, observation, wake);
            let result = runtime.render_update(observation.dirty()).map(|mut update| {
                observation.annotate_render_update(&mut update);
                update
            });
            let _ = reply.send(result);
        }
        SessionCommand::ImageResourceData { image_id, generation, mut callback, reply } => {
            let result = runtime.with_image_resource_data(image_id, generation, &mut callback);
            let _ = reply.send(result);
        }
        SessionCommand::MarkObserved { generation, reply } => {
            let _ = reply.send(observation.mark_observed(generation));
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
            *has_active_client = active;
            let _ = reply.send(());
        }
        SessionCommand::SubscribeRawOutput { reply } => {
            let (tx, rx) = mpsc::sync_channel(RAW_OUTPUT_TAP_CHUNKS);
            raw_output_taps.push(tx);
            let _ = reply.send(RawOutputTap { rx });
        }
        SessionCommand::Stop { terminate } => {
            if terminate && !*exited {
                let _ = runtime.dispatch_signal(POSIX_SIGTERM, SignalTarget::Leader);
            }
            stop = true;
        }
    }
    if !stop {
        session_actor_pump(runtime, observation, exited, *has_active_client, raw_output_taps, wake);
    }
    stop
}

fn session_actor_pump(
    runtime: &mut SessionRuntime,
    observation: &mut ObservationState,
    exited: &mut bool,
    has_active_client: bool,
    raw_output_taps: &mut Vec<SyncSender<Vec<u8>>>,
    wake: &WakeCallback,
) {
    match pump_session_runtime(runtime, exited, has_active_client) {
        Ok(result) => {
            publish_raw_output(raw_output_taps, &result.chunks);
            match result.outcome {
                PumpOutcome::Clean => {}
                PumpOutcome::PartialUnknown => mark_partial_unknown_and_wake(observation, wake),
                PumpOutcome::Full => {
                    let rows = runtime.inspect(false, 0).terminal.rows;
                    mark_full_and_wake(observation, rows, wake);
                }
            }
        }
        Err(_) => {
            let rows = runtime.inspect(false, 0).terminal.rows;
            mark_full_and_wake(observation, rows, wake);
        }
    }
    sync_terminal_modes_and_wake(runtime, observation, wake);
}

fn publish_raw_output(taps: &mut Vec<SyncSender<Vec<u8>>>, chunks: &[Vec<u8>]) {
    if chunks.is_empty() || taps.is_empty() {
        return;
    }
    taps.retain(|tap| {
        for chunk in chunks {
            if tap.try_send(chunk.clone()).is_err() {
                return false;
            }
        }
        true
    });
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
