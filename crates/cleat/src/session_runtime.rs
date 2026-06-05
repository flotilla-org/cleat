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
    recording::SessionRecorder,
    runtime::SessionMetadata,
    vt::{self, ScreenGrid, VtEngine},
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
}

pub(crate) struct PtyOutput {
    pub chunks: Vec<Vec<u8>>,
}

impl SessionRuntime {
    pub(crate) fn spawn(session_dir: PathBuf, session: &SessionMetadata, mut vt_engine: Box<dyn VtEngine>) -> Result<Self, String> {
        let pty_child = PtyChild::spawn(session)?;
        pty_child.set_nonblocking()?;
        let detached_da = match session.vt_engine {
            // The DA tracker is the only DA source for the passthrough engine.
            // The ghostty engine answers DA itself via its DeviceAttributes callback,
            // so we skip the tracker there to avoid double replies.
            vt::VtEngineKind::Passthrough => Some(DeviceAttributeTracker::new()),
            vt::VtEngineKind::Ghostty => None,
        };
        let mut recorder = None;
        if session.record {
            match SessionRecorder::new(&session_dir, vt_engine.size().0, vt_engine.size().1, session.vt_engine.as_str()) {
                Ok(mut r) => {
                    write_replay_snapshot(&mut *vt_engine, &mut r, session.vt_engine.as_str(), Duration::ZERO);
                    recorder = Some(r);
                }
                Err(err) => eprintln!("failed to start recording: {err}"),
            }
        }

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
        })
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
        self.pty_child.resize(cols, rows)?;
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

    #[allow(dead_code)]
    pub(crate) fn screen_grid(&mut self) -> Result<ScreenGrid, String> {
        self.vt_engine.screen_grid()
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
        self.pty_child.resize(cols, rows)?;
        self.vt_engine.resize(cols, rows)
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
    use crate::vt::{CellFlags, CellWidth, CursorState, ResolvedCell, Rgb};

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
            cells: vec![
                ResolvedCell {
                    graphemes: vec!['A' as u32],
                    fg: Rgb { r: 1, g: 2, b: 3 },
                    bg: Rgb { r: 4, g: 5, b: 6 },
                    flags: CellFlags::BOLD | CellFlags::UNDERLINE,
                    width: CellWidth::Wide,
                },
                ResolvedCell { width: CellWidth::SpacerTail, ..ResolvedCell::default() },
            ],
        };
        let mut engine = GridEngine { grid: grid.clone() };

        assert_eq!(engine.screen_grid().expect("screen grid"), grid);
        assert_eq!(engine.screen_text().expect("screen text"), "A");
    }
}
