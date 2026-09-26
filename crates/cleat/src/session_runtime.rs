#![cfg_attr(not(any(unix, windows)), allow(dead_code))]

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use crate::{
    conpty_startup::{ConptyStartup, StartupStep},
    da::DeviceAttributeTracker,
    platform::{pty::PtyChild, ChildExit},
    protocol::{InspectResult, SignalTarget},
    provider::{
        DirtyState, TerminalRenderUpdate, TerminalScrollbackExtent, TerminalScrollbarState, TerminalSnapshot, ViewportCommand,
        ViewportCommandOutcome,
    },
    recording::SessionRecorder,
    runtime::{normalize_tags, AmbientSessionCoordinates, SessionMetadata},
    screen_activity::{ScreenActivityTime, ScreenActivityTracker},
    vt::{self, TerminalModeState, VtEngine},
};

const PTY_READ_BUFFER_SIZE: usize = 64 * 1024;
const SNAPSHOT_INTERVAL_BYTES: u64 = 256 * 1024;
/// Per-pump read budget. A PTY child that produces output faster than the
/// VT engine consumes it (`yes` at saturation) would otherwise keep one
/// `read_available_output` call spinning forever, starving the actor's
/// command channel — and with it the daemon's whole control plane. The
/// actor's readiness poll re-arms immediately, so bounding the slice costs
/// no throughput; it only guarantees commands drain between slices. Two
/// reads keeps a slice around ~100ms even for a debug-build VT engine.
const PTY_READ_BUDGET_PER_PUMP: usize = 2 * PTY_READ_BUFFER_SIZE;
/// Output a releasing host may read between its transfer snapshot and the
/// commit. The adopter replays it into its engine; a transfer that outruns
/// this bound fails and the session stays where it is.
#[cfg(unix)]
const TRANSFER_TAIL_LIMIT: usize = 16 * 1024 * 1024;

pub(crate) struct SessionRuntime {
    session: SessionMetadata,
    session_dir: PathBuf,
    hosting_epoch: u64,
    pty_child: PtyChild,
    vt_engine: Box<dyn VtEngine>,
    detached_da: Option<DeviceAttributeTracker>,
    /// The bundled ConPTY's startup handshake, until it has been answered.
    conpty_startup: Option<ConptyStartup>,
    recorder: Option<SessionRecorder>,
    markers: HashMap<String, u64>,
    held_keys: crate::keyboard::HeldKeys,
    held_buttons: crate::mouse::HeldButtons,
    epoch: Instant,
    last_pty_output_at: Option<Instant>,
    screen_activity: ScreenActivityTracker,
    pending_screen_activity_at: Option<ScreenActivityTime>,
    // Current cell pixel size, used to fill the PTY winsize ws_xpixel/ws_ypixel
    // so TIOCGWINSZ-based apps (e.g. katzensteg) can compute image aspect. Zero
    // until the first geometry/set_cell_size, matching a terminal that hasn't
    // reported a pixel size yet.
    cell_width_px: u32,
    cell_height_px: u32,
    /// Output read since a transfer snapshot, while a transfer is pending.
    #[cfg(unix)]
    transfer_tail: Option<TransferTail>,
}

#[cfg(unix)]
#[derive(Default)]
struct TransferTail {
    bytes: Vec<u8>,
    overflowed: bool,
}

/// Everything a releasing host hands an adopter, captured on the session's
/// actor so the snapshot and the output tail share one cut point.
#[cfg(unix)]
pub(crate) struct TransferSource {
    pub session: SessionMetadata,
    pub size: crate::runtime::TerminalSize,
    pub cell_pixel_size: (u16, u16),
    pub child_pid: u32,
    pub hosting_epoch: u64,
    pub replay_snapshot: crate::recording::ReplaySnapshot,
    pub markers: HashMap<String, u64>,
    pub recording_paused: bool,
    pub pty_master: std::os::fd::OwnedFd,
    pub recording: Option<std::fs::File>,
}

/// The adopting half: descriptors and state received in a transfer manifest.
#[cfg(unix)]
pub(crate) struct AdoptedSession {
    pub session_dir: PathBuf,
    pub session: SessionMetadata,
    pub cell_pixel_size: (u16, u16),
    pub hosting_epoch: u64,
    pub replay_snapshot: crate::recording::ReplaySnapshot,
    pub markers: HashMap<String, u64>,
    pub recording_paused: bool,
    pub pty_child: PtyChild,
    pub recording: Option<std::fs::File>,
}

pub(crate) struct PtyOutput {
    /// Shared so publishing a chunk to each raw-output tap is a refcount
    /// bump, not a payload copy (issue #135).
    pub chunks: Vec<Arc<[u8]>>,
}

impl SessionRuntime {
    pub(crate) fn spawn(session_dir: PathBuf, session: &SessionMetadata, vt_engine: Box<dyn VtEngine>) -> Result<Self, String> {
        Self::spawn_inner(session_dir, session, vt_engine, None)
    }

    pub(crate) fn spawn_in_daemon(
        session_dir: PathBuf,
        session: &SessionMetadata,
        vt_engine: Box<dyn VtEngine>,
        coordinates: &AmbientSessionCoordinates,
    ) -> Result<Self, String> {
        Self::spawn_inner(session_dir, session, vt_engine, Some(coordinates))
    }

    fn spawn_inner(
        session_dir: PathBuf,
        session: &SessionMetadata,
        mut vt_engine: Box<dyn VtEngine>,
        coordinates: Option<&AmbientSessionCoordinates>,
    ) -> Result<Self, String> {
        // Recreation from recording (ADR 0001): if the session dir already holds
        // a recording from a prior activation, replay it into the fresh engine so
        // its history returns as scrollback above the freshly-invoked command.
        // Detection is by cast presence — a brand-new session has an empty dir.
        crate::runtime::ensure_hosting_epoch(&session_dir)?;
        let cast_path = session_dir.join(crate::recording::CAST_FILE_NAME);
        let recreating = crate::recreate::session_is_recreatable(&session_dir);
        if recreating {
            crate::recreate::seed_engine_from_cast(&mut *vt_engine, &cast_path)?;
            // Seeded history can contain queries from the prior activation
            // (DSR/CPR, DA, DECRQM, ...) which the engine answers synchronously
            // into its reply buffer. Those answers belong to a program that no
            // longer exists — discard them so the first detached pump doesn't
            // write them to the new child's stdin as phantom input.
            let _ = vt_engine.drain_replies();
        }

        let pty_child = PtyChild::spawn_with_ambient(session, coordinates)?;
        pty_child.set_nonblocking()?;
        let conpty_startup = pty_child.sends_startup_queries().then(ConptyStartup::new);
        let detached_da = match session.vt_engine {
            // The DA tracker is the only DA source for the passthrough engine.
            // The ghostty engine answers DA itself via its DeviceAttributes callback,
            // so we skip the tracker there to avoid double replies.
            vt::VtEngineKind::Passthrough => Some(DeviceAttributeTracker::new()),
            vt::VtEngineKind::Ghostty => None,
        };
        let recorder = if session.record {
            // Recording appends across activations (ADR 0002): on recreation,
            // reopen the existing cast (writing a boundary marker) rather than
            // truncating it with a new header.
            let mut recorder = if recreating {
                SessionRecorder::reopen_append(&session_dir, Duration::ZERO).map_err(|err| format!("failed to resume recording: {err}"))?
            } else {
                SessionRecorder::new(&session_dir, vt_engine.size().0, vt_engine.size().1, session.vt_engine.as_str())
                    .map_err(|err| format!("failed to start recording: {err}"))?
            };
            // Snapshot the (possibly seeded) state. For recreation this checkpoints
            // the activation boundary; for a fresh session it is the initial frame.
            write_replay_snapshot(&mut *vt_engine, &mut recorder, session.vt_engine.as_str(), Duration::ZERO);
            Some(recorder)
        } else {
            None
        };

        let hosting_epoch = crate::runtime::hosting_epoch(&session_dir)?;
        let mut runtime = Self {
            hosting_epoch,
            session: session.clone(),
            session_dir,
            pty_child,
            conpty_startup,
            vt_engine,
            detached_da,
            recorder,
            markers: HashMap::new(),
            held_keys: Default::default(),
            held_buttons: Default::default(),
            epoch: Instant::now(),
            last_pty_output_at: None,
            screen_activity: ScreenActivityTracker::new(unix_timestamp_millis(SystemTime::now())),
            pending_screen_activity_at: None,
            cell_width_px: 0,
            cell_height_px: 0,
            #[cfg(unix)]
            transfer_tail: None,
        };
        // Establish a clean activity baseline before the child emits output.
        let _ = runtime.vt_engine.screen_grid();
        Ok(runtime)
    }

    /// Build the runtime for a session transferred from another host. The
    /// engine is seeded from the manifest's replay snapshot; the child is
    /// not armed and the PTY must not be read until [`Self::resume_adopted`].
    #[cfg(unix)]
    pub(crate) fn adopt(adopted: AdoptedSession, mut vt_engine: Box<dyn VtEngine>) -> Result<Self, String> {
        let snapshot = &adopted.replay_snapshot;
        vt_engine.resize(snapshot.cols.max(1), snapshot.rows.max(1))?;
        let (cell_width_px, cell_height_px) = (u32::from(adopted.cell_pixel_size.0), u32::from(adopted.cell_pixel_size.1));
        if cell_width_px > 0 && cell_height_px > 0 {
            vt_engine.set_cell_size(cell_width_px, cell_height_px)?;
        }
        vt_engine.feed(snapshot.state.as_bytes())?;
        // The releasing host already answered every query in this history.
        let _ = vt_engine.drain_replies();
        adopted.pty_child.set_nonblocking()?;
        let recorder = adopted
            .recording
            .map(|file| {
                let mut recorder = SessionRecorder::adopt_append(&adopted.session_dir, file)?;
                if adopted.recording_paused {
                    recorder.pause(Duration::ZERO);
                }
                Ok::<_, String>(recorder)
            })
            .transpose()?;
        let detached_da = match adopted.session.vt_engine {
            vt::VtEngineKind::Passthrough => Some(DeviceAttributeTracker::new()),
            vt::VtEngineKind::Ghostty => None,
        };
        let mut runtime = Self {
            hosting_epoch: adopted.hosting_epoch,
            session: adopted.session,
            session_dir: adopted.session_dir,
            pty_child: adopted.pty_child,
            conpty_startup: None,
            vt_engine,
            detached_da,
            recorder,
            markers: adopted.markers,
            held_keys: Default::default(),
            held_buttons: Default::default(),
            epoch: Instant::now(),
            last_pty_output_at: None,
            screen_activity: ScreenActivityTracker::new(unix_timestamp_millis(SystemTime::now())),
            pending_screen_activity_at: None,
            cell_width_px,
            cell_height_px,
            transfer_tail: None,
        };
        let _ = runtime.vt_engine.screen_grid();
        Ok(runtime)
    }

    /// Commit an adoption: replay the output the releasing host read after its
    /// snapshot, then take over the recording and the child.
    #[cfg(unix)]
    pub(crate) fn resume_adopted(&mut self, tail: &[u8]) -> Result<(), String> {
        if !tail.is_empty() {
            self.vt_engine.feed(tail)?;
            let _ = self.vt_engine.drain_replies();
        }
        if let Some(recorder) = &mut self.recorder {
            recorder.rebind_to_session_dir()?;
        }
        self.pty_child.arm();
        Ok(())
    }

    /// Capture the transfer snapshot and descriptors, and start collecting the
    /// output read after it. PTY output keeps flowing and is still recorded.
    #[cfg(unix)]
    pub(crate) fn prepare_transfer(&mut self) -> Result<TransferSource, String> {
        if self.transfer_tail.is_some() {
            return Err(format!("session {} is already transferring", self.session.id));
        }
        if self.pty_child.is_released() {
            return Err(format!("session {} was already transferred", self.session.id));
        }
        if !self.vt_engine.supports_replay() {
            return Err(format!(
                "session {} cannot transfer: its {} VT engine has no replay snapshot",
                self.session.id,
                self.session.vt_engine.as_str()
            ));
        }
        // No payload means nothing has been drawn yet: the empty screen.
        let payload = replay_snapshot_payload(&mut *self.vt_engine).unwrap_or_default();
        let (cols, rows) = self.vt_engine.size();
        let pty_master = self.pty_child.duplicate_master()?;
        let recording = self.recorder.as_ref().map(SessionRecorder::append_handle).transpose()?;
        self.transfer_tail = Some(TransferTail::default());
        Ok(TransferSource {
            session: self.session.clone(),
            size: crate::runtime::TerminalSize { cols, rows },
            cell_pixel_size: (clamp_u16(self.cell_width_px), clamp_u16(self.cell_height_px)),
            child_pid: self.pty_child.leader_pid(),
            hosting_epoch: self.hosting_epoch,
            replay_snapshot: crate::recording::ReplaySnapshot {
                engine: self.session.vt_engine.as_str().to_string(),
                cols,
                rows,
                state: String::from_utf8_lossy(&payload).into_owned(),
            },
            markers: self.markers.clone(),
            recording_paused: self.recorder.as_ref().is_some_and(SessionRecorder::is_paused),
            pty_master,
            recording,
        })
    }

    /// Stop collecting a transfer tail; the session stays here.
    #[cfg(unix)]
    pub(crate) fn abort_transfer(&mut self) {
        self.transfer_tail = None;
    }

    /// The output read since the transfer snapshot. The caller stops reading
    /// the PTY from here until it commits or aborts.
    #[cfg(unix)]
    pub(crate) fn transfer_tail(&mut self) -> Result<Vec<u8>, String> {
        let tail = self.transfer_tail.take().ok_or_else(|| format!("session {} is not transferring", self.session.id))?;
        if tail.overflowed {
            return Err(format!("session {} produced more than {TRANSFER_TAIL_LIMIT} bytes during the transfer", self.session.id));
        }
        if let Some(recorder) = &mut self.recorder {
            recorder.flush_final();
        }
        Ok(tail.bytes)
    }

    /// Hand the session to its adopter: mark the recording, close this host's
    /// recording descriptor, and give up the child without signalling it.
    /// Returns the pid this host must keep reaping.
    #[cfg(unix)]
    pub(crate) fn commit_transfer(&mut self, epoch: u64, address: &str) -> Option<u32> {
        if let Some(mut recorder) = self.recorder.take() {
            recorder.flush_final();
            recorder.transferred(epoch, address, self.epoch.elapsed());
            recorder.flush();
        }
        self.hosting_epoch = epoch;
        self.pty_child.release()
    }

    #[cfg(unix)]
    pub(crate) fn is_released(&self) -> bool {
        self.pty_child.is_released()
    }

    fn pty_pixel_size(&self, cols: u16, rows: u16) -> (u32, u32) {
        ((cols as u32).saturating_mul(self.cell_width_px), (rows as u32).saturating_mul(self.cell_height_px))
    }

    /// The Unix actor waits on the PTY master; elsewhere only tests inspect it.
    #[cfg(any(unix, test))]
    pub(crate) fn pty_child(&self) -> &PtyChild {
        &self.pty_child
    }

    pub(crate) fn last_pty_output_at(&self) -> Option<Instant> {
        self.last_pty_output_at
    }

    pub(crate) fn screen_activity_tracker(&self) -> ScreenActivityTracker {
        self.screen_activity.clone()
    }

    pub(crate) fn recording_active(&self) -> bool {
        self.recorder.is_some()
    }

    pub(crate) fn flush_recording(&mut self) {
        if let Some(ref mut recorder) = self.recorder {
            recorder.flush();
        }
    }

    pub(crate) fn record_attach(&mut self) {
        self.record_custom_event('a', r#"{"client":"foreground"}"#);
    }

    pub(crate) fn record_detach(&mut self) {
        self.record_custom_event('d', r#"{"client":"foreground"}"#);
    }

    pub(crate) fn apply_attach_state(
        &mut self,
        cols: u16,
        rows: u16,
        capabilities: &vt::ClientCapabilities,
    ) -> Result<Option<Vec<u8>>, String> {
        let (width_px, height_px) = self.pty_pixel_size(cols, rows);
        self.pty_child.resize(cols, rows, width_px, height_px)?;
        self.vt_engine.resize(cols, rows)?;
        if self.vt_engine.supports_replay() {
            self.vt_engine.replay_payload(capabilities)
        } else {
            Ok(None)
        }
    }

    pub(crate) fn capture_text(&self) -> Result<String, String> {
        self.vt_engine.screen_text()
    }

    pub(crate) fn screen_contains(&self, text: &str) -> bool {
        self.vt_engine.screen_text().is_ok_and(|screen| screen.contains(text))
    }

    pub(crate) fn validate_text_matching(&self) -> Result<(), String> {
        self.vt_engine.screen_text().map(|_| ())
    }

    pub(crate) fn snapshot(&mut self, dirty: DirtyState) -> Result<TerminalSnapshot, String> {
        self.observe_pending_screen_activity();
        let grid = self.vt_engine.screen_grid()?;
        let scrollbar = self.vt_engine.scrollbar_state()?;
        let mut snapshot = TerminalSnapshot::from_screen_grid(grid, dirty);
        snapshot.viewport_kind = scrollbar.viewport_kind;
        snapshot.scrollbar = scrollbar;
        snapshot.scrollback_offset_rows = scrollbar.viewport_top_row;
        snapshot.terminal_modes = self.vt_engine.terminal_mode_state()?;
        Ok(snapshot)
    }

    pub(crate) fn render_update(&mut self, dirty: DirtyState) -> Result<TerminalRenderUpdate, String> {
        self.observe_pending_screen_activity();
        let scrollbar = self.vt_engine.scrollbar_state()?;
        let mut update = self.vt_engine.render_update(dirty)?;
        update.viewport_kind = scrollbar.viewport_kind;
        update.scrollbar = scrollbar;
        update.scrollback_offset_rows = scrollbar.viewport_top_row;
        update.terminal_modes = self.vt_engine.terminal_mode_state()?;
        Ok(update)
    }

    pub(crate) fn with_image_resource_data(
        &mut self,
        image_id: u32,
        generation: u64,
        callback: &mut dyn FnMut(&[u8]) -> bool,
    ) -> Result<bool, String> {
        self.vt_engine.with_image_resource_data(image_id, generation, callback)
    }

    pub(crate) fn scrollback_extent(&self) -> Result<TerminalScrollbackExtent, String> {
        self.vt_engine.scrollback_extent()
    }

    pub(crate) fn scrollbar_state(&self) -> Result<TerminalScrollbarState, String> {
        self.vt_engine.scrollbar_state()
    }

    pub(crate) fn set_attachment_view(&mut self, id: u128, command: ViewportCommand) -> Result<bool, String> {
        let history = self.vt_engine.set_attachment_view(id, command)?;
        if history {
            self.release_input(id)?;
        }
        Ok(history)
    }
    pub(crate) fn capture_attachment_view(&mut self, id: u128) -> Result<Option<crate::provider::CapturedView>, String> {
        self.vt_engine.capture_attachment_view(id)
    }
    pub(crate) fn release_attachment_view(&mut self, id: u128) {
        self.vt_engine.release_attachment_view(id);
    }

    pub(crate) fn focus(&mut self, focused: bool) -> Result<(), String> {
        let bytes = self.vt_engine.encode_focus(focused)?;
        if !bytes.is_empty() {
            self.write_input(&bytes)?;
        }
        Ok(())
    }

    pub(crate) fn scroll_viewport(&mut self, command: ViewportCommand) -> Result<ViewportCommandOutcome, String> {
        let outcome = self.vt_engine.scroll_viewport(command)?;
        if outcome == ViewportCommandOutcome::Moved {
            self.release_input(0)?;
        }
        Ok(outcome)
    }

    pub(crate) fn terminal_mode_state(&self) -> Result<TerminalModeState, String> {
        self.vt_engine.terminal_mode_state()
    }

    pub(crate) fn synchronized_output_active(&self) -> Result<bool, String> {
        self.vt_engine.synchronized_output_active()
    }

    pub(crate) fn end_synchronized_output(&mut self) -> Result<(), String> {
        self.vt_engine.end_synchronized_output()
    }

    pub(crate) fn encode_mouse(
        &mut self,
        action: vt::MouseAction,
        button: Option<vt::MouseButton>,
        any_button_pressed: bool,
        modifiers: vt::MouseModifiers,
        x_px: f32,
        y_px: f32,
    ) -> Result<Vec<u8>, String> {
        self.vt_engine.encode_mouse(action, button, any_button_pressed, modifiers, x_px, y_px)
    }

    pub(crate) fn key(&mut self, source: u128, event: crate::provider::TerminalKeyEvent) -> Result<usize, String> {
        // Validate and encode before changing ownership. Encoder failures must
        // not leave phantom holds in the session.
        let bytes = self.vt_engine.encode_key(&event)?;
        let Some(delivery) = self.held_keys.event(source, event.clone())? else {
            return Ok(0);
        };
        let bytes = if delivery != event { self.vt_engine.encode_key(&delivery)? } else { bytes };
        if bytes.is_empty() {
            return Ok(0);
        }
        self.write_input(&bytes)?;
        Ok(1)
    }

    pub(crate) fn retain_input_sources(&mut self, sources: &[u128]) -> Result<(), String> {
        for source in self.held_keys.sources().into_iter().chain(self.held_buttons.sources()) {
            if !sources.contains(&source) {
                self.release_input(source)?;
            }
        }
        Ok(())
    }

    pub(crate) fn release_input(&mut self, source: u128) -> Result<(), String> {
        for event in self.held_buttons.release(source) {
            self.deliver_mouse(event)?;
        }
        for event in self.held_keys.release(source) {
            let bytes = self.vt_engine.encode_key(&event)?;
            if !bytes.is_empty() {
                self.write_input(&bytes)?;
            }
        }
        Ok(())
    }

    pub(crate) fn mouse(&mut self, source: u128, event: crate::host::actor::SessionMouseEvent) -> Result<usize, String> {
        let Some(event) = self.held_buttons.event(source, event) else {
            return Ok(0);
        };
        self.deliver_mouse(event)
    }

    fn deliver_mouse(&mut self, event: crate::host::actor::SessionMouseEvent) -> Result<usize, String> {
        let bytes = self.encode_mouse(event.action, event.button, event.any_button_pressed, event.modifiers, event.x_px, event.y_px)?;
        if bytes.is_empty() {
            return Ok(0);
        }
        self.write_input(&bytes)?;
        Ok(1)
    }

    pub(crate) fn encode_paste(&mut self, text: &[u8]) -> Result<Vec<u8>, String> {
        self.vt_engine.encode_paste(text)
    }

    pub(crate) fn write_input(&mut self, bytes: &[u8]) -> Result<(), String> {
        if let Some(ref mut recorder) = self.recorder {
            recorder.input(bytes, self.epoch.elapsed());
        }
        self.pty_child.write_all(bytes)
    }

    pub(crate) fn write_input_with_mark(&mut self, bytes: &[u8], marker_name: String) -> Result<u64, String> {
        let recorder = self.recorder.as_mut().ok_or_else(|| "recording not active".to_string())?;
        recorder.flush();
        recorder.event(crate::asciicast::EventCode::Marker, &marker_name, self.epoch.elapsed());
        let offset = recorder.bytes_written();
        self.markers.insert(marker_name, offset);
        recorder.input(bytes, self.epoch.elapsed());
        self.pty_child.write_all(bytes)?;
        Ok(offset)
    }

    pub(crate) fn resize(&mut self, cols: u16, rows: u16) -> Result<(), String> {
        if let Some(ref mut recorder) = self.recorder {
            recorder.event(crate::asciicast::EventCode::Resize, &format!("{}x{}", cols, rows), self.epoch.elapsed());
        }
        let (width_px, height_px) = self.pty_pixel_size(cols, rows);
        self.pty_child.resize(cols, rows, width_px, height_px)?;
        self.vt_engine.resize(cols, rows)
    }

    pub(crate) fn set_cell_size(&mut self, cell_width_px: u32, cell_height_px: u32) -> Result<(), String> {
        self.cell_width_px = cell_width_px;
        self.cell_height_px = cell_height_px;
        self.vt_engine.set_cell_size(cell_width_px, cell_height_px)?;
        // Refresh the PTY winsize pixel fields for the current grid so apps that
        // read TIOCGWINSZ pick up the new cell size even if cols/rows are unchanged.
        let (cols, rows) = self.vt_engine.size();
        let (width_px, height_px) = self.pty_pixel_size(cols, rows);
        self.pty_child.resize(cols, rows, width_px, height_px)
    }

    pub(crate) fn replay_payload(&mut self, capabilities: &vt::ClientCapabilities) -> Result<Option<Vec<u8>>, String> {
        if self.vt_engine.supports_replay() {
            self.vt_engine.replay_payload(capabilities)
        } else {
            Ok(None)
        }
    }

    pub(crate) fn inspect(&self, has_controller: bool, watcher_count: usize) -> InspectResult {
        let (cols, rows) = self.vt_engine.size();
        let foreground_pgid = self.pty_child.foreground_pgid();
        let mut attachments = Vec::new();
        if has_controller {
            attachments.push(crate::protocol::AttachmentInspect {
                role: "controller".to_string(),
                identity: crate::protocol::AttachmentIdentity::default(),
                denial_reason: None,
            });
        }
        attachments.extend((0..watcher_count).map(|_| crate::protocol::AttachmentInspect {
            role: "watcher".to_string(),
            identity: crate::protocol::AttachmentIdentity::default(),
            denial_reason: None,
        }));

        let activity = self.screen_activity.json_snapshot(Instant::now());
        InspectResult {
            generation: self.session_dir.parent().and_then(|p| p.parent()).and_then(crate::runtime::generation_from_daemon_dir),
            hosting_epoch: self.hosting_epoch,
            session: crate::protocol::SessionInspect {
                id: self.session.id.clone(),
                state: "running".to_string(),
                vt_engine: self.session.vt_engine.as_str().to_string(),
                vt_engine_status: crate::vt::vt_engine_status(self.session.vt_engine).to_string(),
                functional_vt_available: crate::vt::functional_vt_available(),
                cwd: self.session.cwd.clone(),
                cmd: self.session.cmd.clone(),
                tags: self.session.tags.clone(),
                conpty: self.pty_child.conpty().cloned(),
            },
            terminal: crate::protocol::TerminalInspect { rows, cols },
            process: crate::protocol::ProcessInspect {
                leader_pid: self.pty_child.leader_pid(),
                foreground_pgid,
                leader_cwd: self.pty_child.leader_cwd(),
                foreground_cwd: self.pty_child.foreground_cwd(),
            },
            attachments,
            recording: crate::protocol::RecordingInspect {
                active: self.recorder.as_ref().is_some_and(|r| !r.is_paused()),
                bytes_written: self.recorder.as_ref().map(|r| r.bytes_written()).unwrap_or(0),
                markers: self.markers.clone(),
            },
            screen_activity: activity.screen_activity,
            stable_since: activity.stable_since,
            last_output_at: activity.last_output_at,
        }
    }

    #[cfg(unix)]
    pub(crate) fn terminate_tree(&mut self) -> Result<crate::platform::signals::ProcessTree, String> {
        let tree = self.pty_child.process_tree();
        self.pty_child.signal_tree(&tree, nix::sys::signal::Signal::SIGTERM)?;
        self.record_custom_event('s', &serde_json::json!({"signal": libc::SIGTERM, "target": "tree"}).to_string());
        Ok(tree)
    }

    pub(crate) fn dispatch_signal(&mut self, signal: i32, target: SignalTarget) -> Result<(), String> {
        self.pty_child.dispatch_signal(signal, target)?;
        let target_str = match target {
            SignalTarget::Foreground => "foreground",
            SignalTarget::Leader => "leader",
            SignalTarget::Tree => "tree",
        };
        self.record_custom_event('s', &serde_json::json!({"signal": signal, "target": target_str}).to_string());
        Ok(())
    }

    pub(crate) fn update_tags(&mut self, add: Vec<String>, remove: Vec<String>) -> Vec<String> {
        for tag in add {
            if !self.session.tags.contains(&tag) {
                self.session.tags.push(tag);
            }
        }
        self.session.tags.retain(|tag| !remove.contains(tag));
        normalize_tags(&mut self.session.tags);
        self.session.tags.clone()
    }

    pub(crate) fn mark(&mut self, name: Option<String>) -> Result<u64, String> {
        let recorder = self.recorder.as_mut().ok_or_else(|| "recording not active".to_string())?;
        recorder.flush();
        if let Some(marker_name) = name {
            recorder.event(crate::asciicast::EventCode::Marker, &marker_name, self.epoch.elapsed());
            self.markers.insert(marker_name, recorder.bytes_written());
        }
        Ok(recorder.bytes_written())
    }

    pub(crate) fn resolve_marker(&self, name: &str) -> Option<u64> {
        self.markers.get(name).copied()
    }

    pub(crate) fn resolve_next_marker_after(&self, after: u64) -> Option<u64> {
        self.markers.values().copied().filter(|offset| *offset > after).min()
    }

    pub(crate) fn set_recording(&mut self, enable: bool) -> Result<(), String> {
        if enable && self.recorder.is_none() {
            let (cols, rows) = self.vt_engine.size();
            let mut recorder = SessionRecorder::new(&self.session_dir, cols, rows, self.session.vt_engine.as_str())?;
            write_replay_snapshot(&mut *self.vt_engine, &mut recorder, self.session.vt_engine.as_str(), self.epoch.elapsed());
            self.recorder = Some(recorder);
        } else if enable {
            if let Some(ref mut recorder) = self.recorder {
                if recorder.is_paused() {
                    recorder.resume(self.epoch.elapsed());
                    write_replay_snapshot(&mut *self.vt_engine, recorder, self.session.vt_engine.as_str(), self.epoch.elapsed());
                }
            }
        } else if !enable && self.recorder.as_ref().is_some_and(|r| !r.is_paused()) {
            if let Some(ref mut recorder) = self.recorder {
                recorder.pause(self.epoch.elapsed());
            }
        }
        Ok(())
    }

    pub(crate) fn read_available_output(&mut self, queries_forwarded_to_client: bool) -> Result<PtyOutput, String> {
        self.read_available_output_inner(queries_forwarded_to_client, false)
    }

    pub(crate) fn drain_output_after_exit(&mut self, queries_forwarded_to_client: bool) -> Result<PtyOutput, String> {
        self.read_available_output_inner(queries_forwarded_to_client, true)
    }

    pub(crate) fn child_exit_if_exited(&self) -> Result<Option<ChildExit>, String> {
        #[cfg(unix)]
        {
            self.pty_child.exit_state()
        }
        #[cfg(not(unix))]
        {
            self.pty_child
                .exited()
                .map(|status| status.as_ref().map(|status| ChildExit::Code(crate::platform::pty::exit_code_from_wait_status(status))))
        }
    }

    pub(crate) fn record_exit(&mut self, exit: ChildExit) {
        if let Some(ref mut recorder) = self.recorder {
            // Flush any held-back incomplete UTF-8 bytes before the exit
            // event so they appear in the correct order in the cast file.
            recorder.flush_final();
            match exit {
                ChildExit::Code(code) => recorder.event(crate::asciicast::EventCode::Exit, &code.to_string(), self.epoch.elapsed()),
                // An exit without a status is not an exit code: say so with a
                // structured marker (like `transferred`) instead of inventing one.
                ChildExit::Unknown => recorder.event(
                    crate::asciicast::EventCode::Marker,
                    &serde_json::json!({"event": "exit", "status": "unknown"}).to_string(),
                    self.epoch.elapsed(),
                ),
            }
        }
    }

    pub(crate) fn should_keep_session_dir(&self) -> bool {
        self.recorder.is_some()
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session.id
    }

    fn read_available_output_inner(&mut self, queries_forwarded_to_client: bool, after_exit: bool) -> Result<PtyOutput, String> {
        let mut chunks = Vec::new();
        let mut budget = PTY_READ_BUDGET_PER_PUMP;
        loop {
            // Draining after exit is finite (the child is gone), so the
            // budget applies only to live pumps.
            if !after_exit && budget == 0 {
                break;
            }
            let mut buf = [0u8; PTY_READ_BUFFER_SIZE];
            // Cap the final read to the remaining budget so the slice bound
            // is hard, not "budget plus one buffer".
            let want = if after_exit { buf.len() } else { buf.len().min(budget) };
            match self.pty_child.read_output(&mut buf[..want]) {
                Ok(0) => break,
                Ok(n) => {
                    budget = budget.saturating_sub(n);
                    let startup_rest;
                    let bytes = match self.conpty_startup.as_mut().map(|startup| startup.push(&buf[..n])) {
                        None => &buf[..n],
                        Some(StartupStep::Pending) => continue,
                        Some(StartupStep::Answered { rest }) => {
                            self.conpty_startup = None;
                            self.answer_conpty_startup_query()?;
                            startup_rest = rest;
                            &startup_rest[..]
                        }
                        Some(StartupStep::Absent { bytes }) => {
                            self.conpty_startup = None;
                            startup_rest = bytes;
                            &startup_rest[..]
                        }
                    };
                    if bytes.is_empty() {
                        continue;
                    }
                    self.last_pty_output_at = Some(Instant::now());
                    self.vt_engine.feed(bytes)?;
                    self.record_output(bytes);
                    #[cfg(unix)]
                    if let Some(tail) = &mut self.transfer_tail {
                        if tail.bytes.len().saturating_add(bytes.len()) > TRANSFER_TAIL_LIMIT {
                            tail.overflowed = true;
                        } else if !tail.overflowed {
                            tail.bytes.extend_from_slice(bytes);
                        }
                    }

                    // Drain engine replies every iteration so the buffer never accumulates
                    // stale replies across authority changes. Only a raw-stream controller
                    // forwards queries to its terminal. Packet clients receive rendered
                    // state, so the engine must answer regardless of their driving roles.
                    let engine_reply = self.vt_engine.drain_replies();
                    if !queries_forwarded_to_client {
                        self.write_detached_replies(bytes, &engine_reply)?;
                    }
                    chunks.push(Arc::from(bytes));
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock || (after_exit && is_pty_eof_after_exit(&err)) => break,
                Err(err) if after_exit => return Err(format!("read pty output after exit: {err}")),
                Err(err) => return Err(format!("read pty output: {err}")),
            }
        }
        if !chunks.is_empty() {
            self.note_screen_activity_candidate(ScreenActivityTime::new(Instant::now(), unix_timestamp_millis(SystemTime::now())));
        }
        Ok(PtyOutput { chunks })
    }

    fn note_screen_activity_candidate(&mut self, changed_at: ScreenActivityTime) {
        match self.vt_engine.screen_activity_changed() {
            Ok(Some(true)) => self.pending_screen_activity_at = Some(changed_at),
            Ok(Some(false) | None) => {}
            Err(err) => eprintln!("screen activity observation error: {err}"),
        }
    }

    fn observe_pending_screen_activity(&mut self) -> bool {
        let Some(changed_at) = self.pending_screen_activity_at.take() else {
            return false;
        };
        self.screen_activity.render_changed(changed_at);
        true
    }

    /// Record pending screen activity, and unless `consume_damage` is false
    /// also consume the engine's render damage it was derived from.
    pub(crate) fn flush_screen_activity(&mut self, consume_damage: bool) {
        if self.observe_pending_screen_activity() && consume_damage {
            if let Err(err) = self.vt_engine.screen_grid() {
                eprintln!("screen activity render flush error: {err}");
            }
        }
    }

    /// Answer ConPTY's startup DA1 query as the VT engine answers a program's,
    /// independent of attached clients so the program never waits on one.
    /// An engine without a DA1 answer (the no-VT build) gets the fixed reply
    /// the detached tracker gives programs.
    fn answer_conpty_startup_query(&mut self) -> Result<(), String> {
        self.vt_engine.feed(crate::conpty_startup::DA1_QUERY)?;
        let reply = self.vt_engine.drain_replies();
        let reply = if reply.is_empty() { crate::da::DA1_RESPONSE.to_vec() } else { reply };
        self.pty_child.write_all(&reply)
    }

    fn write_detached_replies(&mut self, pty_output: &[u8], engine_reply: &[u8]) -> Result<(), String> {
        if let Some(ref mut tracker) = self.detached_da {
            for reply in tracker.push(pty_output) {
                self.pty_child.write_all(&reply)?;
            }
        }
        if !engine_reply.is_empty() {
            self.pty_child.write_all(engine_reply)?;
        }
        Ok(())
    }

    fn record_output(&mut self, bytes: &[u8]) {
        if let Some(ref mut recorder) = self.recorder {
            let elapsed = self.epoch.elapsed();
            recorder.output(bytes, elapsed);
            if recorder.output_bytes_since_snapshot() >= SNAPSHOT_INTERVAL_BYTES {
                if let Some(payload) = replay_snapshot_payload(&mut *self.vt_engine) {
                    let (cols, rows) = self.vt_engine.size();
                    let state = String::from_utf8_lossy(&payload);
                    recorder.write_snapshot(&state, self.session.vt_engine.as_str(), cols, rows, elapsed);
                    return;
                }
                recorder.reset_output_bytes_since_snapshot();
            }
        }
    }

    fn record_custom_event(&mut self, code: char, payload: &str) {
        if let Some(ref mut recorder) = self.recorder {
            recorder.event(crate::asciicast::EventCode::Custom(code), payload, self.epoch.elapsed());
        }
    }
}

fn write_replay_snapshot(engine: &mut dyn VtEngine, recorder: &mut SessionRecorder, engine_name: &str, time: Duration) {
    if let Some(payload) = replay_snapshot_payload(engine) {
        let (cols, rows) = engine.size();
        let state = String::from_utf8_lossy(&payload);
        recorder.write_snapshot(&state, engine_name, cols, rows, time);
    }
}

fn replay_snapshot_payload(engine: &mut dyn VtEngine) -> Option<Vec<u8>> {
    engine.replay_payload(&vt::ClientCapabilities::conservative_fallback()).ok().flatten()
}

#[cfg(unix)]
fn clamp_u16(value: u32) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

fn unix_timestamp_millis(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_millis().try_into().unwrap_or(u64::MAX)
}

fn is_pty_eof_after_exit(err: &std::io::Error) -> bool {
    #[cfg(unix)]
    {
        err.raw_os_error() == Some(libc::EIO)
    }
    #[cfg(not(unix))]
    {
        let _ = err;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::vt::passthrough::PassthroughVtEngine;
    use crate::vt::{CellFlags, CellWidth, CursorState, ResolvedCell, Rgb, ScreenGrid, VtEngineKind};

    #[derive(Debug)]
    struct GridEngine {
        grid: ScreenGrid,
    }

    impl VtEngine for GridEngine {
        fn feed(&mut self, _bytes: &[u8]) -> Result<(), String> {
            Ok(())
        }

        fn resize(&mut self, cols: u16, rows: u16) -> Result<(), String> {
            self.grid.cols = cols;
            self.grid.rows = rows;
            self.grid.cells.resize(cols as usize * rows as usize, ResolvedCell::default());
            Ok(())
        }

        fn supports_replay(&self) -> bool {
            false
        }

        fn replay_payload(&self, _capabilities: &vt::ClientCapabilities) -> Result<Option<Vec<u8>>, String> {
            Ok(None)
        }

        fn screen_text(&self) -> Result<String, String> {
            Ok(self.grid.row_text(0))
        }

        fn screen_grid(&mut self) -> Result<ScreenGrid, String> {
            Ok(self.grid.clone())
        }

        fn size(&self) -> (u16, u16) {
            (self.grid.cols, self.grid.rows)
        }
    }

    #[test]
    fn screen_grid_preserves_cell_shape_for_future_provider_snapshots() {
        let grid = ScreenGrid {
            cols: 2,
            rows: 1,
            cursor: CursorState { col: 1, row: 0, visible: true, ..CursorState::default() },
            dirty_rows: Vec::new(),
            cells: vec![
                ResolvedCell {
                    graphemes: vec!['A' as u32],
                    fg: Rgb { r: 1, g: 2, b: 3 },
                    bg: Rgb { r: 4, g: 5, b: 6 },
                    flags: CellFlags::BOLD | CellFlags::UNDERLINE,
                    width: CellWidth::Wide,
                    ..ResolvedCell::default()
                },
                ResolvedCell { width: CellWidth::SpacerTail, ..ResolvedCell::default() },
            ],
        };
        let mut engine = GridEngine { grid: grid.clone() };

        assert_eq!(engine.screen_grid().expect("screen grid"), grid);
        assert_eq!(engine.screen_text().expect("screen text"), "A");
    }

    #[cfg(unix)]
    #[test]
    fn spawn_fails_when_requested_recording_cannot_start() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(temp.path().join(crate::recording::CAST_FILE_NAME)).expect("create cast path directory");
        let session = SessionMetadata {
            id: "alpha".to_string(),
            vt_engine: VtEngineKind::Passthrough,
            cwd: None,
            cmd: Some("true".to_string()),
            tags: Vec::new(),
            environment: Vec::new(),
            record: true,
            initial_size: crate::runtime::TerminalSize::default(),
            colors: crate::vt::TerminalColors::default(),
        };

        let err = match SessionRuntime::spawn(temp.path().to_path_buf(), &session, Box::new(PassthroughVtEngine::new(80, 24))) {
            Ok(_) => panic!("requested recording startup failure should fail session spawn"),
            Err(err) => err,
        };

        assert!(err.contains("failed to start recording"), "{err}");
        assert!(err.contains(crate::recording::CAST_FILE_NAME), "{err}");
    }

    #[cfg(all(unix, feature = "ghostty-vt"))]
    #[test]
    fn recreation_seeds_scrollback_from_prior_recording() {
        fn pump_until_screen_contains(rt: &mut SessionRuntime, needle: &str) {
            // This wait is setup for a replay correctness check, not a latency
            // assertion. Under parallel PTY-heavy test runs, the child can take
            // several seconds to exec before it emits the marker.
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                rt.read_available_output(false).expect("read pty output");
                if rt.screen_contains(needle) {
                    return;
                }
                assert!(Instant::now() < deadline, "timed out waiting for {needle:?}; screen was {:?}", rt.capture_text());
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        let temp = tempfile::tempdir().expect("tempdir");
        let session_dir = temp.path().to_path_buf();
        let colors = crate::vt::TerminalColors::default();

        // Activation 1: print a marker, then idle so it stays on screen and the
        // recording captures it before we tear the runtime down.
        let session1 = SessionMetadata {
            id: "recreate".to_string(),
            vt_engine: VtEngineKind::Ghostty,
            cwd: None,
            cmd: Some("sh -c 'printf RECREATE_MARKER; sleep 30'".to_string()),
            tags: Vec::new(),
            environment: Vec::new(),
            record: true,
            initial_size: crate::runtime::TerminalSize::default(),
            colors,
        };
        let engine1 = crate::vt::make_vt_engine_with_colors(VtEngineKind::Ghostty, 80, 24, colors).expect("engine 1");
        let mut rt1 = SessionRuntime::spawn(session_dir.clone(), &session1, engine1).expect("spawn activation 1");
        pump_until_screen_contains(&mut rt1, "RECREATE_MARKER");
        rt1.flush_recording();
        drop(rt1);

        // Activation 2: recreate the same session dir with a different command.
        // Seeding happens synchronously in spawn, so the prior marker is on the
        // recreated screen before the new command produces any output.
        let session2 = SessionMetadata { cmd: Some("sleep 30".to_string()), ..session1.clone() };
        let engine2 = crate::vt::make_vt_engine_with_colors(VtEngineKind::Ghostty, 80, 24, colors).expect("engine 2");
        let rt2 = SessionRuntime::spawn(session_dir.clone(), &session2, engine2).expect("spawn activation 2");

        assert!(
            rt2.screen_contains("RECREATE_MARKER"),
            "recreated session should restore prior output; screen was {:?}",
            rt2.capture_text()
        );

        // The recording continues in the same cast across the activation boundary.
        let raw = std::fs::read_to_string(session_dir.join(crate::recording::CAST_FILE_NAME)).expect("read cast");
        assert!(raw.contains("session-recreated"), "activation boundary marker recorded");
    }

    #[cfg(all(unix, feature = "ghostty-vt"))]
    #[test]
    fn recreation_discards_stale_query_replies_from_seeded_history() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_dir = temp.path().to_path_buf();
        let colors = crate::vt::TerminalColors::default();

        // A prior activation whose recorded output contains a CPR query
        // (\x1b[6n): the dead program asked for the cursor position.
        let mut recorder = crate::recording::SessionRecorder::new(&session_dir, 80, 24, "ghostty").expect("recorder");
        recorder.output(b"hello\x1b[6n", Duration::from_millis(10));
        recorder.flush();
        drop(recorder);

        // Precondition: seeding this history really does buffer a reply, so the
        // assertion below cannot pass vacuously.
        let mut probe = crate::vt::make_vt_engine_with_colors(VtEngineKind::Ghostty, 80, 24, colors).expect("probe engine");
        crate::recreate::seed_engine_from_cast(&mut *probe, &session_dir.join(crate::recording::CAST_FILE_NAME))
            .expect("seed probe engine");
        assert!(!probe.drain_replies().is_empty(), "seeded query should buffer a reply in the engine");

        let session = SessionMetadata {
            id: "recreate-replies".to_string(),
            vt_engine: VtEngineKind::Ghostty,
            cwd: None,
            cmd: Some("sleep 30".to_string()),
            tags: Vec::new(),
            environment: Vec::new(),
            record: false,
            initial_size: crate::runtime::TerminalSize::default(),
            colors,
        };
        let engine = crate::vt::make_vt_engine_with_colors(VtEngineKind::Ghostty, 80, 24, colors).expect("engine");
        let mut rt = SessionRuntime::spawn(session_dir, &session, engine).expect("spawn recreation");

        // The stale answer belongs to the dead program: it must not be pending
        // where the first detached pump would write it to the new child's stdin.
        assert!(rt.vt_engine.drain_replies().is_empty(), "stale replies must be discarded during spawn");
    }

    /// The child half of the ConPTY pass-through regression below. It is an
    /// ordinary no-op test unless that regression launches it inside a session.
    #[cfg(windows)]
    const CONPTY_EMITTER_ENV: &str = "CLEAT_TEST_CONPTY_GRAPHICS_EMITTER";
    #[cfg(windows)]
    const KITTY_APC: &[u8] = b"\x1b_Ga=T,f=24,s=1,v=1;AAAA\x1b\\";
    #[cfg(windows)]
    const SIXEL_DCS: &[u8] = b"\x1bPq#0;2;100;0;0#0~~~~\x1b\\";

    #[cfg(windows)]
    #[test]
    fn conpty_graphics_emitter() {
        use windows_sys::Win32::{
            Storage::FileSystem::WriteFile,
            System::Console::{
                GetConsoleMode, GetStdHandle, SetConsoleMode, DISABLE_NEWLINE_AUTO_RETURN, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
                STD_OUTPUT_HANDLE,
            },
        };

        if std::env::var_os(CONPTY_EMITTER_ENV).is_none() {
            return;
        }
        let payload = [b"<BEGIN>".as_slice(), KITTY_APC, b"<AFTER-APC>", SIXEL_DCS, b"<AFTER-SIXEL><END>\r\n"].concat();
        // SAFETY: plain console calls on this process's own output handle.
        unsafe {
            let output = GetStdHandle(STD_OUTPUT_HANDLE);
            let mut mode = 0;
            GetConsoleMode(output, &mut mode);
            SetConsoleMode(output, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING | DISABLE_NEWLINE_AUTO_RETURN);
            let mut written = 0;
            assert_ne!(WriteFile(output, payload.as_ptr(), payload.len() as u32, &mut written, std::ptr::null_mut()), 0);
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    /// Records every byte a session feeds its VT engine. It never answers
    /// queries, so ConPTY's startup DA1 gets the no-VT fixed reply.
    #[cfg(windows)]
    struct FeedSpy {
        fed: Arc<std::sync::Mutex<Vec<u8>>>,
    }

    #[cfg(windows)]
    impl VtEngine for FeedSpy {
        fn feed(&mut self, bytes: &[u8]) -> Result<(), String> {
            self.fed.lock().expect("spy lock").extend_from_slice(bytes);
            Ok(())
        }

        fn resize(&mut self, _cols: u16, _rows: u16) -> Result<(), String> {
            Ok(())
        }

        fn supports_replay(&self) -> bool {
            false
        }

        fn replay_payload(&self, _capabilities: &vt::ClientCapabilities) -> Result<Option<Vec<u8>>, String> {
            Ok(None)
        }

        fn screen_text(&self) -> Result<String, String> {
            Err("feed spy has no screen".into())
        }

        fn screen_grid(&mut self) -> Result<ScreenGrid, String> {
            Err("feed spy has no screen".into())
        }

        fn size(&self) -> (u16, u16) {
            (80, 24)
        }
    }

    /// Spawn `cmd` in a detached session and pump its output into a feed spy
    /// until `marker` arrives. Returns every fed byte and the time it took.
    #[cfg(windows)]
    fn run_detached_until(cmd: String, environment: Vec<(String, String)>, marker: &[u8]) -> (Vec<u8>, Duration, SessionRuntime) {
        let temp = tempfile::tempdir().expect("tempdir");
        let session = SessionMetadata {
            id: "conpty-regression".to_string(),
            vt_engine: VtEngineKind::Passthrough,
            cwd: None,
            cmd: Some(cmd),
            tags: Vec::new(),
            environment,
            record: false,
            initial_size: crate::runtime::TerminalSize::default(),
            colors: crate::vt::TerminalColors::default(),
        };
        let fed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let started = Instant::now();
        let mut rt =
            SessionRuntime::spawn(temp.path().to_path_buf(), &session, Box::new(FeedSpy { fed: fed.clone() })).expect("spawn session");
        let deadline = started + Duration::from_secs(30);
        loop {
            // No client is attached: queries are never forwarded.
            rt.read_available_output(false).expect("read pty output");
            let bytes = fed.lock().expect("spy lock").clone();
            if bytes.windows(marker.len()).any(|window| window == marker) {
                return (bytes, started.elapsed(), rt);
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {:?}; fed {:?}",
                String::from_utf8_lossy(marker),
                String::from_utf8_lossy(&bytes)
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(windows)]
    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|window| window == needle)
    }

    /// The inbox ConPTY drops Kitty graphics APC and sixel DCS; the bundle
    /// passes them through (ADR 0006). Run with CLEAT_CONPTY=inbox to watch
    /// this fail against the inbox ConPTY.
    #[cfg(windows)]
    #[test]
    fn conpty_passes_kitty_apc_and_sixel_dcs_to_the_vt_engine() {
        let exe = std::env::current_exe().expect("test executable");
        let test_path = module_path!().split_once("::").map(|(_, path)| path).expect("crate-relative module path");
        let cmd = format!("{} --exact {test_path}::conpty_graphics_emitter --nocapture --test-threads=1", exe.display());
        let (fed, _, rt) = run_detached_until(cmd, vec![(CONPTY_EMITTER_ENV.to_string(), "1".to_string())], b"<END>");

        let conpty = rt.pty_child().conpty().expect("Windows sessions report their ConPTY").clone();
        let text = String::from_utf8_lossy(&fed);
        assert!(
            contains(&fed, KITTY_APC) && contains(&fed, SIXEL_DCS),
            "Kitty APC and sixel DCS must reach the VT engine verbatim under the {} ConPTY; fed {text:?}",
            conpty.summary()
        );
        assert!(
            contains(&fed, &[b"<BEGIN>".as_slice(), KITTY_APC, b"<AFTER-APC>", SIXEL_DCS, b"<AFTER-SIXEL>"].concat()),
            "order preserved: {text:?}"
        );
        // The startup handshake is ConPTY's question to Cleat, not program output.
        assert!(!contains(&fed, b"\x1b[1t\x1b[c"), "startup queries must not reach the engine: {text:?}");
    }

    /// Without an answer to its startup DA1 query the bundled ConPTY holds the
    /// program for 3 s. Cleat answers it even with no client attached.
    #[cfg(windows)]
    #[test]
    fn detached_conpty_session_starts_without_startup_query_delay() {
        let (_, elapsed, rt) = run_detached_until("echo cleat-startup-ready".to_string(), Vec::new(), b"cleat-startup-ready");
        let conpty = rt.pty_child().conpty().expect("Windows sessions report their ConPTY").summary();
        assert!(elapsed < Duration::from_millis(2000), "detached {conpty} session took {elapsed:?} to produce output");
    }
}
