use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use crate::asciicast::{encode_event, encode_header, CleatMeta, Event, EventCode, Header};

pub const CAST_FILE_NAME: &str = "session.cast";

const COALESCE_SIZE_THRESHOLD: usize = 4096;

/// Internal coalescing buffer for consecutive same-type events.
struct CoalesceBuffer {
    bytes: Vec<u8>,
    /// Time of the first byte pushed into this buffer.
    first_time: Duration,
    is_input: bool,
}

impl CoalesceBuffer {
    fn new() -> Self {
        Self { bytes: Vec::new(), first_time: Duration::ZERO, is_input: false }
    }

    fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Push bytes into the buffer, setting first_time if this is the first push.
    fn push(&mut self, bytes: &[u8], time: Duration, is_input: bool) {
        if self.bytes.is_empty() {
            self.first_time = time;
            self.is_input = is_input;
        }
        self.bytes.extend_from_slice(bytes);
    }

    fn exceeds_threshold(&self) -> bool {
        self.bytes.len() >= COALESCE_SIZE_THRESHOLD
    }

    /// Drain and return the pending event, resetting the buffer.
    fn drain(&mut self) -> Option<Event> {
        if self.bytes.is_empty() {
            return None;
        }
        let data = String::from_utf8_lossy(&self.bytes).into_owned();
        let code = if self.is_input { EventCode::Input } else { EventCode::Output };
        let event = Event { time: self.first_time, code, data };
        self.bytes.clear();
        Some(event)
    }
}

pub struct SessionRecorder {
    session_dir: PathBuf,
    cast_file: File,
    bytes_written: u64,
    prev_time: Duration,
    coalesce: CoalesceBuffer,
    output_bytes_since_snapshot: u64,
}

impl SessionRecorder {
    /// Create a new `SessionRecorder`. Writes the asciicast v3 header to `session.cast`.
    pub fn new(session_dir: &Path, cols: u16, rows: u16, engine: &str) -> Result<Self, String> {
        let cast_path = session_dir.join(CAST_FILE_NAME);
        let mut cast_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&cast_path)
            .map_err(|err| format!("open cast file {}: {err}", cast_path.display()))?;

        let timestamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).ok();
        let term_type = std::env::var("TERM").ok();

        let header = Header {
            cols,
            rows,
            timestamp,
            term_type,
            title: None,
            cleat: Some(CleatMeta {
                version: env!("CARGO_PKG_VERSION").to_string(),
                build: option_env!("CLEAT_BUILD_HASH").map(|s| s.to_string()),
                engine: engine.to_string(),
            }),
        };

        let header_line = encode_header(&header) + "\n";
        cast_file.write_all(header_line.as_bytes()).map_err(|err| format!("write cast header: {err}"))?;
        let bytes_written = header_line.len() as u64;

        Ok(Self {
            session_dir: session_dir.to_path_buf(),
            cast_file,
            bytes_written,
            prev_time: Duration::ZERO,
            coalesce: CoalesceBuffer::new(),
            output_bytes_since_snapshot: 0,
        })
    }

    /// Buffer output bytes for coalescing. Flushes on type change or size threshold.
    pub fn output(&mut self, bytes: &[u8], time: Duration) {
        // Flush if switching from input to output.
        if !self.coalesce.is_empty() && self.coalesce.is_input {
            self.flush();
        }
        self.coalesce.push(bytes, time, false);
        self.output_bytes_since_snapshot += bytes.len() as u64;
        if self.coalesce.exceeds_threshold() {
            self.flush();
        }
    }

    /// Buffer input bytes for coalescing. Flushes on type change or size threshold.
    pub fn input(&mut self, bytes: &[u8], time: Duration) {
        // Flush if switching from output to input.
        if !self.coalesce.is_empty() && !self.coalesce.is_input {
            self.flush();
        }
        self.coalesce.push(bytes, time, true);
        if self.coalesce.exceeds_threshold() {
            self.flush();
        }
    }

    /// Flush buffer, then write a one-off event immediately.
    pub fn event(&mut self, code: EventCode, data: &str, time: Duration) {
        self.flush();
        let event = Event { time, code, data: data.to_string() };
        self.write_event(&event);
    }

    /// Flush buffer, then write a gap event with code 'g'.
    pub fn emit_gap(&mut self, reason: &str, time: Duration) {
        self.flush();
        let data = serde_json::json!({"reason": reason}).to_string();
        let event = Event { time, code: EventCode::Custom('g'), data };
        self.write_event(&event);
    }

    /// Flush buffer, write a snapshot event with code 'S', reset output_bytes_since_snapshot.
    pub fn write_snapshot(&mut self, vt_state: &str, engine: &str, cols: u16, rows: u16, time: Duration) {
        self.flush();
        let data = serde_json::json!({"engine": engine, "cols": cols, "rows": rows, "state": vt_state}).to_string();
        let event = Event { time, code: EventCode::Custom('S'), data };
        self.write_event(&event);
        self.output_bytes_since_snapshot = 0;
    }

    /// Flush the coalescing buffer — write accumulated bytes as a single event.
    pub fn flush(&mut self) {
        if let Some(event) = self.coalesce.drain() {
            self.write_event(&event);
        }
    }

    /// Current byte offset in the .cast file (matches file size).
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    /// Output bytes accumulated since last snapshot.
    pub fn output_bytes_since_snapshot(&self) -> u64 {
        self.output_bytes_since_snapshot
    }

    /// Reset the output bytes counter without writing a snapshot (e.g. when no
    /// snapshot payload is available).
    pub fn reset_output_bytes_since_snapshot(&mut self) {
        self.output_bytes_since_snapshot = 0;
    }

    /// The session directory this recorder is writing into.
    pub fn session_dir(&self) -> &Path {
        &self.session_dir
    }

    /// Write a single event line to the cast file, updating bytes_written and prev_time.
    fn write_event(&mut self, event: &Event) {
        let line = encode_event(event, &mut self.prev_time) + "\n";
        if let Err(err) = self.cast_file.write_all(line.as_bytes()) {
            eprintln!("recording write error: {err}");
            return;
        }
        self.bytes_written += line.len() as u64;
    }
}
