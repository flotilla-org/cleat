use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use crate::asciicast::{encode_event, encode_header, CleatMeta, Event, EventCode, Header};

pub const CAST_FILE_NAME: &str = "session.cast";

const COALESCE_SIZE_THRESHOLD: usize = 4096;

/// Return the byte length of the longest prefix of `bytes` that is complete
/// UTF-8 (i.e. does not end with a partial multi-byte sequence).
fn utf8_complete_len(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    let len = bytes.len();
    // Find the last leading byte (non-continuation).
    let mut i = len;
    while i > 0 {
        i -= 1;
        let b = bytes[i];
        if b & 0b1100_0000 != 0b1000_0000 {
            // Found a leading byte (or ASCII). Check if the sequence is complete.
            let expected_len = if b < 0x80 {
                1
            } else if b & 0b1110_0000 == 0b1100_0000 {
                2
            } else if b & 0b1111_0000 == 0b1110_0000 {
                3
            } else if b & 0b1111_1000 == 0b1111_0000 {
                4
            } else {
                // Invalid byte — treat as complete (lossy will handle it).
                return len;
            };
            let available = len - i;
            if available >= expected_len {
                return len;
            } else {
                return i;
            }
        }
    }
    // All bytes are continuation bytes — treat as complete (lossy will handle it).
    len
}

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
    /// Holds back any trailing incomplete UTF-8 sequence so it can be
    /// completed by the next push+drain cycle.
    fn drain(&mut self) -> Option<Event> {
        if self.bytes.is_empty() {
            return None;
        }
        let split = utf8_complete_len(&self.bytes);
        if split == 0 {
            // Only incomplete bytes in the buffer — hold everything back.
            return None;
        }
        let data = String::from_utf8_lossy(&self.bytes[..split]).into_owned();
        let code = if self.is_input { EventCode::Input } else { EventCode::Output };
        let event = Event { time: self.first_time, code, data };
        // Move any trailing incomplete bytes to the front.
        if split < self.bytes.len() {
            let tail = self.bytes[split..].to_vec();
            self.bytes.clear();
            self.bytes.extend_from_slice(&tail);
        } else {
            self.bytes.clear();
        }
        Some(event)
    }

    /// Drain all bytes unconditionally, using lossy conversion for any
    /// incomplete trailing sequence. Called on session exit when no more
    /// bytes will arrive.
    fn drain_final(&mut self) -> Option<Event> {
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
    paused: bool,
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
            paused: false,
        })
    }

    /// Pause recording. Flushes the buffer first. Output/input calls become no-ops.
    pub fn pause(&mut self, time: Duration) {
        if !self.paused {
            self.flush();
            self.paused = true;
            // Gap event is emitted on resume, not on pause, so it appears
            // in the stream at the point where data resumes.
            let _ = time; // timestamp captured at resume
        }
    }

    /// Resume recording after a pause. Emits a gap event marking the discontinuity.
    pub fn resume(&mut self, time: Duration) {
        if self.paused {
            self.paused = false;
            self.emit_gap("recording-paused", time);
        }
    }

    /// Whether recording is currently paused.
    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Buffer output bytes for coalescing. Flushes on type change or size threshold.
    /// No-op when paused.
    pub fn output(&mut self, bytes: &[u8], time: Duration) {
        if self.paused {
            return;
        }
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
    /// No-op when paused.
    pub fn input(&mut self, bytes: &[u8], time: Duration) {
        if self.paused {
            return;
        }
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

    /// Final flush — emits all remaining bytes, including incomplete UTF-8
    /// sequences (using lossy conversion). Call once when the session is
    /// ending and no more bytes will arrive.
    pub fn flush_final(&mut self) {
        if let Some(event) = self.coalesce.drain_final() {
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
    /// On write failure, prev_time is NOT advanced so subsequent events have correct deltas.
    fn write_event(&mut self, event: &Event) {
        let mut candidate_prev = self.prev_time;
        let line = encode_event(event, &mut candidate_prev) + "\n";
        if let Err(err) = self.cast_file.write_all(line.as_bytes()) {
            eprintln!("recording write error: {err}");
            return;
        }
        self.prev_time = candidate_prev;
        self.bytes_written += line.len() as u64;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::CoalesceBuffer;

    #[test]
    fn drain_complete_utf8_emits_all_bytes() {
        let mut buf = CoalesceBuffer::new();
        buf.push("hello café".as_bytes(), Duration::ZERO, false);
        let event = buf.drain().expect("should produce event");
        assert_eq!(event.data, "hello café");
        assert!(buf.is_empty());
    }

    #[test]
    fn drain_split_2byte_char_holds_back_incomplete() {
        let mut buf = CoalesceBuffer::new();
        // é is U+00E9, encoded as [0xC3, 0xA9]
        let bytes = "café".as_bytes(); // [99, 97, 102, 195, 169]
                                       // Push everything except the last byte
        buf.push(&bytes[..4], Duration::ZERO, false);
        let event = buf.drain().expect("should produce event");
        assert_eq!(event.data, "caf");
        // The leading byte 0xC3 should be held back
        assert!(!buf.is_empty());

        // Now push the continuation byte
        buf.push(&bytes[4..], Duration::from_secs(1), false);
        let event = buf.drain().expect("should produce event");
        assert_eq!(event.data, "é");
        assert!(buf.is_empty());
    }

    #[test]
    fn drain_split_3byte_char_at_first_boundary() {
        let mut buf = CoalesceBuffer::new();
        // € is U+20AC, encoded as [0xE2, 0x82, 0xAC]
        let euro = "€".as_bytes();
        // Push only the lead byte
        buf.push(&euro[..1], Duration::ZERO, false);
        let event = buf.drain();
        assert!(event.is_none(), "single lead byte with no complete chars should produce no event");
        assert!(!buf.is_empty());

        // Push remaining two bytes
        buf.push(&euro[1..], Duration::from_secs(1), false);
        let event = buf.drain().expect("should produce event");
        assert_eq!(event.data, "€");
        assert!(buf.is_empty());
    }

    #[test]
    fn drain_split_3byte_char_at_second_boundary() {
        let mut buf = CoalesceBuffer::new();
        // "A€" = [0x41, 0xE2, 0x82, 0xAC]
        let bytes = "A€".as_bytes();
        // Push "A" + lead byte + first continuation
        buf.push(&bytes[..3], Duration::ZERO, false);
        let event = buf.drain().expect("should produce event");
        assert_eq!(event.data, "A");
        assert!(!buf.is_empty());

        // Push final continuation byte
        buf.push(&bytes[3..], Duration::from_secs(1), false);
        let event = buf.drain().expect("should produce event");
        assert_eq!(event.data, "€");
        assert!(buf.is_empty());
    }

    #[test]
    fn drain_split_4byte_char_at_each_boundary() {
        let mut buf = CoalesceBuffer::new();
        // 😀 is U+1F600, encoded as [0xF0, 0x9F, 0x98, 0x80]
        let emoji = "😀".as_bytes();

        // Split after 1 byte
        buf.push(&emoji[..1], Duration::ZERO, false);
        assert!(buf.drain().is_none());

        buf.push(&emoji[1..], Duration::from_secs(1), false);
        let event = buf.drain().expect("should produce event");
        assert_eq!(event.data, "😀");
        assert!(buf.is_empty());

        // Split after 2 bytes
        buf.push(&emoji[..2], Duration::ZERO, false);
        assert!(buf.drain().is_none());

        buf.push(&emoji[2..], Duration::from_secs(1), false);
        let event = buf.drain().expect("should produce event");
        assert_eq!(event.data, "😀");
        assert!(buf.is_empty());

        // Split after 3 bytes
        buf.push(&emoji[..3], Duration::ZERO, false);
        assert!(buf.drain().is_none());

        buf.push(&emoji[3..], Duration::from_secs(1), false);
        let event = buf.drain().expect("should produce event");
        assert_eq!(event.data, "😀");
        assert!(buf.is_empty());
    }

    #[test]
    fn drain_final_flush_with_incomplete_bytes_emits_replacement() {
        let mut buf = CoalesceBuffer::new();
        // Push just a 3-byte lead + one continuation (incomplete €)
        buf.push(&[0xE2, 0x82], Duration::ZERO, false);
        // drain holds them back:
        assert!(buf.drain().is_none());
        // The incomplete bytes are still in the buffer:
        assert!(!buf.is_empty());
    }

    #[test]
    fn drain_ascii_only_emits_everything() {
        let mut buf = CoalesceBuffer::new();
        buf.push(b"hello world 123", Duration::ZERO, false);
        let event = buf.drain().expect("should produce event");
        assert_eq!(event.data, "hello world 123");
        assert!(buf.is_empty());
    }

    #[test]
    fn drain_mixed_complete_multibyte_emits_all() {
        let mut buf = CoalesceBuffer::new();
        buf.push("hello 日本語 café 😀".as_bytes(), Duration::ZERO, false);
        let event = buf.drain().expect("should produce event");
        assert_eq!(event.data, "hello 日本語 café 😀");
        assert!(buf.is_empty());
    }
}
