#![cfg_attr(not(any(unix, windows)), allow(dead_code))]

use std::{
    collections::HashMap,
    path::PathBuf,
    time::{Duration, Instant},
};

use crate::{
    da::DeviceAttributeTracker,
    platform::pty::{exit_code_from_wait_status, PtyChild},
    protocol::{InspectResult, SignalTarget},
    provider::{
        DirtyState, TerminalRenderUpdate, TerminalScrollbackExtent, TerminalScrollbarState, TerminalSnapshot, ViewportCommand,
        ViewportCommandOutcome,
    },
    recording::SessionRecorder,
    runtime::SessionMetadata,
    vt::{self, TerminalModeState, VtEngine},
};

const PTY_READ_BUFFER_SIZE: usize = 64 * 1024;
const SNAPSHOT_INTERVAL_BYTES: u64 = 256 * 1024;

pub(crate) struct SessionRuntime {
    session: SessionMetadata,
    session_dir: PathBuf,
    pty_child: PtyChild,
    vt_engine: Box<dyn VtEngine>,
    detached_da: Option<DeviceAttributeTracker>,
    recorder: Option<SessionRecorder>,
    markers: HashMap<String, u64>,
    epoch: Instant,
    last_pty_output_at: Option<Instant>,
    // Current cell pixel size, used to fill the PTY winsize ws_xpixel/ws_ypixel
    // so TIOCGWINSZ-based apps (e.g. katzensteg) can compute image aspect. Zero
    // until the first geometry/set_cell_size, matching a terminal that hasn't
    // reported a pixel size yet.
    cell_width_px: u32,
    cell_height_px: u32,
}

pub(crate) struct PtyOutput {
    pub chunks: Vec<Vec<u8>>,
}

impl SessionRuntime {
    pub(crate) fn spawn(session_dir: PathBuf, session: &SessionMetadata, mut vt_engine: Box<dyn VtEngine>) -> Result<Self, String> {
        // Recreation from recording (ADR 0001): if the session dir already holds
        // a recording from a prior activation, replay it into the fresh engine so
        // its history returns as scrollback above the freshly-invoked command.
        // Detection is by cast presence — a brand-new session has an empty dir.
        let cast_path = session_dir.join(crate::recording::CAST_FILE_NAME);
        let recreating = crate::recreate::session_is_recreatable(&session_dir);
        if recreating {
            crate::recreate::seed_engine_from_cast(&mut *vt_engine, &cast_path)?;
        }

        let pty_child = PtyChild::spawn(session)?;
        pty_child.set_nonblocking()?;
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

        Ok(Self {
            session: session.clone(),
            session_dir,
            pty_child,
            vt_engine,
            detached_da,
            recorder,
            markers: HashMap::new(),
            epoch: Instant::now(),
            last_pty_output_at: None,
            cell_width_px: 0,
            cell_height_px: 0,
        })
    }

    fn pty_pixel_size(&self, cols: u16, rows: u16) -> (u32, u32) {
        ((cols as u32).saturating_mul(self.cell_width_px), (rows as u32).saturating_mul(self.cell_height_px))
    }

    pub(crate) fn pty_child(&self) -> &PtyChild {
        &self.pty_child
    }

    pub(crate) fn last_pty_output_at(&self) -> Option<Instant> {
        self.last_pty_output_at
    }

    pub(crate) fn recording_active(&self) -> bool {
        self.recorder.is_some()
    }

    pub(crate) fn flush_recording(&mut self) {
        if let Some(ref mut recorder) = self.recorder {
            recorder.flush();
        }
    }

    pub(crate) fn flush_recording_if_idle(&mut self, pty_readable: bool, client_readable: bool) {
        if !pty_readable && !client_readable {
            self.flush_recording();
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

    pub(crate) fn scroll_viewport(&mut self, command: ViewportCommand) -> Result<ViewportCommandOutcome, String> {
        self.vt_engine.scroll_viewport(command)
    }

    pub(crate) fn terminal_mode_state(&self) -> Result<TerminalModeState, String> {
        self.vt_engine.terminal_mode_state()
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

    pub(crate) fn inspect(&self, has_controller: bool) -> InspectResult {
        let (cols, rows) = self.vt_engine.size();
        let foreground_pgid = self.pty_child.foreground_pgid();

        InspectResult {
            session: crate::protocol::SessionInspect {
                id: self.session.id.clone(),
                state: "running".to_string(),
                vt_engine: self.session.vt_engine.as_str().to_string(),
                vt_engine_status: crate::vt::vt_engine_status(self.session.vt_engine).to_string(),
                functional_vt_available: crate::vt::functional_vt_available(),
                cwd: self.session.cwd.clone(),
                cmd: self.session.cmd.clone(),
            },
            terminal: crate::protocol::TerminalInspect { rows, cols },
            process: crate::protocol::ProcessInspect {
                leader_pid: self.pty_child.leader_pid(),
                foreground_pgid,
                leader_cwd: self.pty_child.leader_cwd(),
                foreground_cwd: self.pty_child.foreground_cwd(),
            },
            attachments: if has_controller { vec![crate::protocol::AttachmentInspect { role: "controller".to_string() }] } else { vec![] },
            recording: crate::protocol::RecordingInspect {
                active: self.recorder.as_ref().is_some_and(|r| !r.is_paused()),
                bytes_written: self.recorder.as_ref().map(|r| r.bytes_written()).unwrap_or(0),
                markers: self.markers.clone(),
            },
        }
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

    pub(crate) fn read_available_output(&mut self, has_active_client: bool) -> Result<PtyOutput, String> {
        self.read_available_output_inner(has_active_client, false)
    }

    pub(crate) fn drain_output_after_exit(&mut self, has_active_client: bool) -> Result<PtyOutput, String> {
        self.read_available_output_inner(has_active_client, true)
    }

    pub(crate) fn exit_code_if_exited(&self) -> Result<Option<i32>, String> {
        self.pty_child.exited().map(|status| status.as_ref().map(exit_code_from_wait_status))
    }

    pub(crate) fn record_exit_code(&mut self, code: i32) {
        if let Some(ref mut recorder) = self.recorder {
            // Flush any held-back incomplete UTF-8 bytes before the exit
            // event so they appear in the correct order in the cast file.
            recorder.flush_final();
            recorder.event(crate::asciicast::EventCode::Exit, &code.to_string(), self.epoch.elapsed());
        }
    }

    pub(crate) fn should_keep_session_dir(&self) -> bool {
        self.recorder.is_some()
    }

    fn read_available_output_inner(&mut self, has_active_client: bool, after_exit: bool) -> Result<PtyOutput, String> {
        let mut chunks = Vec::new();
        loop {
            let mut buf = [0u8; PTY_READ_BUFFER_SIZE];
            match self.pty_child.read_output(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let bytes = &buf[..n];
                    self.last_pty_output_at = Some(Instant::now());
                    self.vt_engine.feed(bytes)?;
                    self.record_output(bytes);

                    // Drain engine replies every iteration so the buffer never accumulates
                    // stale replies across an attach-to-detach transition. When attached, the
                    // host terminal is authoritative for query responses, so we discard.
                    let engine_reply = self.vt_engine.drain_replies();
                    if !has_active_client {
                        self.write_detached_replies(bytes, &engine_reply)?;
                    }
                    chunks.push(bytes.to_vec());
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock || (after_exit && is_pty_eof_after_exit(&err)) => break,
                Err(err) if after_exit => return Err(format!("read pty output after exit: {err}")),
                Err(err) => return Err(format!("read pty output: {err}")),
            }
        }
        Ok(PtyOutput { chunks })
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
    use crate::vt::{passthrough::PassthroughVtEngine, CellFlags, CellWidth, CursorState, ResolvedCell, Rgb, ScreenGrid, VtEngineKind};

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
            let deadline = Instant::now() + Duration::from_secs(5);
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
}
