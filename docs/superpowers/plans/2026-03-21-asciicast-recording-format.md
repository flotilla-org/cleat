# Asciicast v3 Recording Format Implementation Plan

**Goal:** Replace the ad-hoc `output.log` + text snapshots recording format with structured asciicast v3 NDJSON, enabling timestamped playback, incremental capture, and rich metadata events.

**Architecture:** New `asciicast` module owns the v3 format types and encoder. The existing `recording` module is rewritten to use asciicast events with coalescing. The daemon writes events to a single `session.cast` file. Input recording is added alongside output. The `Mark` protocol frame reports current file offset for cursor-based capture (#6, future work).

**Tech Stack:** Rust, serde_json (existing dep), `std::time::Instant` for timestamps.

---

## File Structure

| File | Action | Responsibility |
|------|--------|----------------|
| `crates/cleat/src/asciicast.rs` | Create | Asciicast v3 types (`Header`, `Event`, `EventCode`), encoder, decoder. Pure format logic, no I/O. |
| `crates/cleat/src/recording.rs` | Rewrite | `SessionRecorder` that writes asciicast events to `session.cast`. Owns coalescing buffer, snapshot interval tracking, file handle. |
| `crates/cleat/src/session.rs` | Modify | Wire new recorder into daemon event loop: output events, input events, resize, attach/detach, signals, exit, snapshots, RecordControl with gap events. Preserve session.cast on session exit. |
| `crates/cleat/src/protocol.rs` | Modify | Add `Mark` / `MarkResult` frames. Add `cast_offset` to `RecordingInspect`. |
| `crates/cleat/src/cli.rs` | Modify | Add `Mark` subcommand. |
| `crates/cleat/src/server.rs` | Modify | Add `mark()` method to `SessionService`. |
| `crates/cleat/src/lib.rs` | Modify | Add `pub mod asciicast;` |
| `crates/cleat/tests/asciicast.rs` | Create | Unit tests for encoder/decoder round-trips. |
| `crates/cleat/tests/recording.rs` | Rewrite | Tests for new `SessionRecorder` (coalescing, snapshots, gaps). |

---

## Task 1: Asciicast v3 encoder/decoder types

**Files:**
- Create: `crates/cleat/src/asciicast.rs`
- Modify: `crates/cleat/src/lib.rs`
- Create: `crates/cleat/tests/asciicast.rs`

This task builds the pure-format module with no I/O dependencies. All types are tested via round-trip encoding/decoding.

- [ ] **Step 1: Write failing test — header round-trip**

In `crates/cleat/tests/asciicast.rs`:

```rust
use cleat::asciicast::{Header, CleatMeta, encode_header, decode_header};

#[test]
fn header_round_trips_with_cleat_metadata() {
    let header = Header {
        cols: 120,
        rows: 40,
        timestamp: Some(1742572800),
        term_type: Some("xterm-256color".to_string()),
        cleat: Some(CleatMeta {
            version: "0.1.0".to_string(),
            build: Some("eb219e3".to_string()),
            engine: "ghostty".to_string(),
        }),
        ..Default::default()
    };
    let encoded = encode_header(&header);
    let decoded = decode_header(&encoded).expect("decode header");
    assert_eq!(decoded.cols, 120);
    assert_eq!(decoded.rows, 40);
    assert_eq!(decoded.timestamp, Some(1742572800));
    assert_eq!(decoded.term_type.as_deref(), Some("xterm-256color"));
    let cleat = decoded.cleat.unwrap();
    assert_eq!(cleat.engine, "ghostty");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --workspace --locked -p cleat --test asciicast`
Expected: compile error — module doesn't exist yet.

- [ ] **Step 3: Write failing test — output event round-trip**

Append to `crates/cleat/tests/asciicast.rs`:

```rust
use std::time::Duration;
use cleat::asciicast::{Event, EventCode, encode_event, decode_event};

#[test]
fn output_event_round_trips() {
    let event = Event {
        time: Duration::from_millis(1234),
        code: EventCode::Output,
        data: "hello \x1b[31mworld\x1b[0m\r\n".to_string(),
    };
    let mut prev_time = Duration::ZERO;
    let encoded = encode_event(&event, &mut prev_time);
    let mut decode_prev = Duration::ZERO;
    let decoded = decode_event(&encoded, &mut decode_prev).expect("decode event");
    assert_eq!(decoded.time, event.time);
    assert_eq!(decoded.code, EventCode::Output);
    assert_eq!(decoded.data, event.data);
}
```

- [ ] **Step 4: Write failing tests — all event codes**

Append to `crates/cleat/tests/asciicast.rs`:

```rust
#[test]
fn input_event_round_trips() {
    let event = Event {
        time: Duration::from_millis(500),
        code: EventCode::Input,
        data: "ls\r".to_string(),
    };
    let mut prev = Duration::ZERO;
    let encoded = encode_event(&event, &mut prev);
    let mut decode_prev = Duration::ZERO;
    let decoded = decode_event(&encoded, &mut decode_prev).expect("decode");
    assert_eq!(decoded.code, EventCode::Input);
    assert_eq!(decoded.data, "ls\r");
}

#[test]
fn resize_event_round_trips() {
    let event = Event {
        time: Duration::from_millis(2000),
        code: EventCode::Resize,
        data: "100x40".to_string(),
    };
    let mut prev = Duration::ZERO;
    let encoded = encode_event(&event, &mut prev);
    let mut decode_prev = Duration::ZERO;
    let decoded = decode_event(&encoded, &mut decode_prev).expect("decode");
    assert_eq!(decoded.code, EventCode::Resize);
    assert_eq!(decoded.data, "100x40");
}

#[test]
fn marker_event_round_trips() {
    let event = Event {
        time: Duration::from_millis(3000),
        code: EventCode::Marker,
        data: "test-start".to_string(),
    };
    let mut prev = Duration::ZERO;
    let encoded = encode_event(&event, &mut prev);
    let mut decode_prev = Duration::ZERO;
    let decoded = decode_event(&encoded, &mut decode_prev).expect("decode");
    assert_eq!(decoded.code, EventCode::Marker);
    assert_eq!(decoded.data, "test-start");
}

#[test]
fn exit_event_round_trips() {
    let event = Event {
        time: Duration::from_millis(5000),
        code: EventCode::Exit,
        data: "0".to_string(),
    };
    let mut prev = Duration::ZERO;
    let encoded = encode_event(&event, &mut prev);
    let mut decode_prev = Duration::ZERO;
    let decoded = decode_event(&encoded, &mut decode_prev).expect("decode");
    assert_eq!(decoded.code, EventCode::Exit);
    assert_eq!(decoded.data, "0");
}

#[test]
fn custom_event_code_round_trips() {
    let event = Event {
        time: Duration::from_millis(100),
        code: EventCode::Custom('S'),
        data: r#"{"engine":"ghostty","cols":80,"rows":24}"#.to_string(),
    };
    let mut prev = Duration::ZERO;
    let encoded = encode_event(&event, &mut prev);
    let mut decode_prev = Duration::ZERO;
    let decoded = decode_event(&encoded, &mut decode_prev).expect("decode");
    assert_eq!(decoded.code, EventCode::Custom('S'));
}
```

- [ ] **Step 5: Write failing test — v3 delta timing**

Append to `crates/cleat/tests/asciicast.rs`:

```rust
#[test]
fn v3_timing_uses_deltas_not_absolute() {
    let e1 = Event { time: Duration::from_millis(1000), code: EventCode::Output, data: "a".into() };
    let e2 = Event { time: Duration::from_millis(2500), code: EventCode::Output, data: "b".into() };
    let mut prev = Duration::ZERO;
    let line1 = encode_event(&e1, &mut prev);
    let line2 = encode_event(&e2, &mut prev);
    // Parse raw JSON to verify the delta, not the absolute time
    let arr1: serde_json::Value = serde_json::from_str(&line1).unwrap();
    let arr2: serde_json::Value = serde_json::from_str(&line2).unwrap();
    assert_eq!(arr1[0].as_f64().unwrap(), 1.0);   // 1000ms delta from 0
    assert_eq!(arr2[0].as_f64().unwrap(), 1.5);   // 1500ms delta from 1000ms
}
```

- [ ] **Step 6: Write failing test — unknown event codes preserved**

```rust
#[test]
fn unknown_event_code_decoded_as_custom() {
    let line = r#"[0.5, "Z", "some data"]"#;
    let mut prev = Duration::ZERO;
    let event = decode_event(line, &mut prev).expect("decode unknown");
    assert_eq!(event.code, EventCode::Custom('Z'));
    assert_eq!(event.data, "some data");
}
```

- [ ] **Step 7: Implement the asciicast module**

Create `crates/cleat/src/asciicast.rs`:

```rust
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub cols: u16,
    pub rows: u16,
    pub timestamp: Option<u64>,
    pub term_type: Option<String>,
    pub title: Option<String>,
    pub cleat: Option<CleatMeta>,
}

impl Default for Header {
    fn default() -> Self {
        Self {
            cols: 80,
            rows: 24,
            timestamp: None,
            term_type: None,
            title: None,
            cleat: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleatMeta {
    pub version: String,
    pub build: Option<String>,
    pub engine: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub time: Duration,
    pub code: EventCode,
    pub data: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventCode {
    Output,
    Input,
    Resize,
    Marker,
    Exit,
    Custom(char),
}

impl EventCode {
    fn as_char(self) -> char {
        match self {
            Self::Output => 'o',
            Self::Input => 'i',
            Self::Resize => 'r',
            Self::Marker => 'm',
            Self::Exit => 'x',
            Self::Custom(c) => c,
        }
    }

    fn from_char(c: char) -> Self {
        match c {
            'o' => Self::Output,
            'i' => Self::Input,
            'r' => Self::Resize,
            'm' => Self::Marker,
            'x' => Self::Exit,
            other => Self::Custom(other),
        }
    }
}

// --- Header serde ---

#[derive(Serialize, Deserialize)]
struct HeaderJson {
    version: u8,
    term: TermJson,
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cleat: Option<CleatMetaJson>,
}

#[derive(Serialize, Deserialize)]
struct TermJson {
    cols: u16,
    rows: u16,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    type_: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct CleatMetaJson {
    version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    build: Option<String>,
    engine: String,
}

pub fn encode_header(header: &Header) -> String {
    let json = HeaderJson {
        version: 3,
        term: TermJson {
            cols: header.cols,
            rows: header.rows,
            type_: header.term_type.clone(),
        },
        timestamp: header.timestamp,
        title: header.title.clone(),
        cleat: header.cleat.as_ref().map(|c| CleatMetaJson {
            version: c.version.clone(),
            build: c.build.clone(),
            engine: c.engine.clone(),
        }),
    };
    serde_json::to_string(&json).expect("header serialization should not fail")
}

pub fn decode_header(line: &str) -> Result<Header, String> {
    let json: HeaderJson =
        serde_json::from_str(line).map_err(|e| format!("invalid header: {e}"))?;
    if json.version != 3 {
        return Err(format!("unsupported asciicast version: {}", json.version));
    }
    Ok(Header {
        cols: json.term.cols,
        rows: json.term.rows,
        timestamp: json.timestamp,
        term_type: json.term.type_,
        title: json.title,
        cleat: json.cleat.map(|c| CleatMeta {
            version: c.version,
            build: c.build,
            engine: c.engine,
        }),
    })
}

// --- Event serde ---

pub fn encode_event(event: &Event, prev_time: &mut Duration) -> String {
    let delta = event.time.saturating_sub(*prev_time);
    *prev_time = event.time;
    let secs = delta.as_millis() as f64 / 1000.0;
    let code = serde_json::to_string(&event.code.as_char().to_string())
        .expect("char serialization should not fail");
    let data =
        serde_json::to_string(&event.data).expect("data serialization should not fail");
    format!("[{:.3}, {}, {}]", secs, code, data)
}

pub fn decode_event(line: &str, prev_time: &mut Duration) -> Result<Event, String> {
    let arr: (f64, String, String) =
        serde_json::from_str(line).map_err(|e| format!("invalid event: {e}"))?;
    let delta = Duration::from_micros((arr.0 * 1_000_000.0) as u64);
    let time = *prev_time + delta;
    *prev_time = time;
    let code = arr
        .1
        .chars()
        .next()
        .ok_or_else(|| "empty event code".to_string())?;
    Ok(Event {
        time,
        code: EventCode::from_char(code),
        data: arr.2,
    })
}
```

- [ ] **Step 8: Register module in lib.rs**

Add `pub mod asciicast;` to `crates/cleat/src/lib.rs`.

- [ ] **Step 9: Run tests to verify they pass**

Run: `cargo test --workspace --locked -p cleat --test asciicast`
Expected: all tests pass.

- [ ] **Step 10: Run full checks**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings && cargo +nightly-2026-03-12 fmt --check`
Expected: clean.

- [ ] **Step 11: Commit**

```bash
git add crates/cleat/src/asciicast.rs crates/cleat/src/lib.rs crates/cleat/tests/asciicast.rs
git commit -m "feat(recording): add asciicast v3 encoder/decoder module"
```

---

## Task 2: Rewrite recording module with asciicast events and coalescing

**Files:**
- Rewrite: `crates/cleat/src/recording.rs`
- Rewrite: `crates/cleat/tests/recording.rs`

The new `SessionRecorder` writes asciicast v3 events to `session.cast`. It owns a coalescing buffer that accumulates same-type events and flushes on type change, idle timeout, or size threshold.

- [ ] **Step 1: Write failing test — recorder creates session.cast with header**

In `crates/cleat/tests/recording.rs` (replace existing content):

```rust
use std::fs;
use std::time::Duration;

use cleat::asciicast::{decode_header, EventCode};
use cleat::recording::SessionRecorder;

#[test]
fn new_recorder_creates_session_cast_with_header() {
    let temp = tempfile::tempdir().expect("tempdir");
    let _recorder = SessionRecorder::new(temp.path(), 80, 24, "passthrough").expect("create");
    let contents = fs::read_to_string(temp.path().join("session.cast")).expect("read");
    let first_line = contents.lines().next().expect("header line");
    let header = decode_header(first_line).expect("decode header");
    assert_eq!(header.cols, 80);
    assert_eq!(header.rows, 24);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --workspace --locked -p cleat --test recording`
Expected: compile error — `SessionRecorder` doesn't exist yet.

- [ ] **Step 3: Write failing test — output event written on flush**

```rust
#[test]
fn output_event_written_after_flush() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut recorder = SessionRecorder::new(temp.path(), 80, 24, "passthrough").expect("create");
    recorder.output(b"hello", Duration::from_millis(100));
    recorder.flush();
    let contents = fs::read_to_string(temp.path().join("session.cast")).expect("read");
    let lines: Vec<&str> = contents.lines().collect();
    assert_eq!(lines.len(), 2); // header + one output event
    assert!(lines[1].contains("\"o\""));
    assert!(lines[1].contains("hello"));
}
```

- [ ] **Step 4: Write failing test — coalescing same-type events**

```rust
#[test]
fn consecutive_outputs_coalesced_into_single_event() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut recorder = SessionRecorder::new(temp.path(), 80, 24, "passthrough").expect("create");
    recorder.output(b"hel", Duration::from_millis(100));
    recorder.output(b"lo", Duration::from_millis(105));
    recorder.flush();
    let contents = fs::read_to_string(temp.path().join("session.cast")).expect("read");
    let lines: Vec<&str> = contents.lines().collect();
    assert_eq!(lines.len(), 2); // header + one coalesced output event
    assert!(lines[1].contains("hello"));
}
```

- [ ] **Step 5: Write failing test — type change flushes buffer**

```rust
#[test]
fn type_change_flushes_previous_buffer() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut recorder = SessionRecorder::new(temp.path(), 80, 24, "passthrough").expect("create");
    recorder.output(b"out", Duration::from_millis(100));
    recorder.input(b"in", Duration::from_millis(200));
    recorder.flush();
    let contents = fs::read_to_string(temp.path().join("session.cast")).expect("read");
    let lines: Vec<&str> = contents.lines().collect();
    assert_eq!(lines.len(), 3); // header + output + input
    assert!(lines[1].contains("\"o\""));
    assert!(lines[2].contains("\"i\""));
}
```

- [ ] **Step 6: Write failing test — input events recorded**

```rust
#[test]
fn input_event_recorded() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut recorder = SessionRecorder::new(temp.path(), 80, 24, "passthrough").expect("create");
    recorder.input(b"ls\r", Duration::from_millis(500));
    recorder.flush();
    let contents = fs::read_to_string(temp.path().join("session.cast")).expect("read");
    let event_line = contents.lines().nth(1).expect("event line");
    assert!(event_line.contains("\"i\""));
    assert!(event_line.contains("ls\\r"));
}
```

- [ ] **Step 7: Write failing test — bytes_written tracks file offset**

```rust
#[test]
fn bytes_written_tracks_cast_file_offset() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut recorder = SessionRecorder::new(temp.path(), 80, 24, "passthrough").expect("create");
    let header_offset = recorder.bytes_written();
    assert!(header_offset > 0); // header was written
    recorder.output(b"hello", Duration::from_millis(100));
    recorder.flush();
    assert!(recorder.bytes_written() > header_offset);
    // bytes_written should match actual file size
    let file_size = fs::metadata(temp.path().join("session.cast")).expect("meta").len();
    assert_eq!(recorder.bytes_written(), file_size);
}
```

- [ ] **Step 8: Write failing test — metadata event (signal)**

```rust
#[test]
fn metadata_event_flushes_buffer_and_writes_inline() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut recorder = SessionRecorder::new(temp.path(), 80, 24, "passthrough").expect("create");
    recorder.output(b"before", Duration::from_millis(100));
    recorder.event(EventCode::Custom('s'), r#"{"signal":"SIGTERM","target":"foreground"}"#, Duration::from_millis(200));
    recorder.flush();
    let contents = fs::read_to_string(temp.path().join("session.cast")).expect("read");
    let lines: Vec<&str> = contents.lines().collect();
    assert_eq!(lines.len(), 3); // header + output + signal
    assert!(lines[1].contains("\"o\""));
    assert!(lines[2].contains("\"s\""));
    assert!(lines[2].contains("SIGTERM"));
}
```

- [ ] **Step 9: Write failing test — gap event on recording toggle**

```rust
#[test]
fn gap_event_emitted_on_resume() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut recorder = SessionRecorder::new(temp.path(), 80, 24, "passthrough").expect("create");
    recorder.output(b"before", Duration::from_millis(100));
    recorder.flush();
    // Simulate: recording was paused, then resumed — emit gap
    recorder.emit_gap("recording-paused", Duration::from_millis(5000));
    let contents = fs::read_to_string(temp.path().join("session.cast")).expect("read");
    let lines: Vec<&str> = contents.lines().collect();
    assert_eq!(lines.len(), 3); // header + output + gap
    assert!(lines[2].contains("\"g\""));
    assert!(lines[2].contains("recording-paused"));
}
```

- [ ] **Step 10: Implement SessionRecorder**

Rewrite `crates/cleat/src/recording.rs`:

```rust
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use crate::asciicast::{self, Event, EventCode, Header, CleatMeta};

const CAST_FILE_NAME: &str = "session.cast";
const COALESCE_SIZE_THRESHOLD: usize = 4096;

pub struct SessionRecorder {
    session_dir: PathBuf,
    cast_file: File,
    bytes_written: u64,
    prev_time: Duration,
    buffer: CoalesceBuffer,
    output_bytes_since_snapshot: u64,
}

struct CoalesceBuffer {
    code: Option<EventCode>,
    data: Vec<u8>,
    first_time: Duration,
}

impl CoalesceBuffer {
    fn new() -> Self {
        Self { code: None, data: Vec::new(), first_time: Duration::ZERO }
    }

    fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    fn matches(&self, code: EventCode) -> bool {
        self.code.map_or(true, |c| c == code)
    }

    fn push(&mut self, code: EventCode, bytes: &[u8], time: Duration) {
        if self.code.is_none() {
            self.code = Some(code);
            self.first_time = time;
        }
        self.data.extend_from_slice(bytes);
    }

    fn take(&mut self) -> Option<(EventCode, Vec<u8>, Duration)> {
        if self.data.is_empty() {
            return None;
        }
        let code = self.code.take().expect("code set when data present");
        let data = std::mem::take(&mut self.data);
        Some((code, data, self.first_time))
    }

    fn should_flush(&self) -> bool {
        self.data.len() >= COALESCE_SIZE_THRESHOLD
    }
}

impl SessionRecorder {
    pub fn new(session_dir: &Path, cols: u16, rows: u16, engine: &str) -> Result<Self, String> {
        let cast_path = session_dir.join(CAST_FILE_NAME);
        let mut cast_file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&cast_path)
            .map_err(|e| format!("open {}: {e}", cast_path.display()))?;

        let header = Header {
            cols,
            rows,
            timestamp: Some(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            ),
            term_type: std::env::var("TERM").ok(),
            cleat: Some(CleatMeta {
                version: env!("CARGO_PKG_VERSION").to_string(),
                build: option_env!("CLEAT_BUILD_HASH").map(|s| s.to_string()),
                engine: engine.to_string(),
            }),
            ..Default::default()
        };

        let mut header_line = asciicast::encode_header(&header);
        header_line.push('\n');
        cast_file.write_all(header_line.as_bytes())
            .map_err(|e| format!("write header: {e}"))?;
        let bytes_written = header_line.len() as u64;

        Ok(Self {
            session_dir: session_dir.to_path_buf(),
            cast_file,
            bytes_written,
            prev_time: Duration::ZERO,
            buffer: CoalesceBuffer::new(),
            output_bytes_since_snapshot: 0,
        })
    }

    pub fn output(&mut self, bytes: &[u8], time: Duration) {
        if !self.buffer.matches(EventCode::Output) || self.buffer.should_flush() {
            self.flush();
        }
        self.buffer.push(EventCode::Output, bytes, time);
        self.output_bytes_since_snapshot += bytes.len() as u64;
        if self.buffer.should_flush() {
            self.flush();
        }
    }

    pub fn input(&mut self, bytes: &[u8], time: Duration) {
        if !self.buffer.matches(EventCode::Input) || self.buffer.should_flush() {
            self.flush();
        }
        self.buffer.push(EventCode::Input, bytes, time);
        if self.buffer.should_flush() {
            self.flush();
        }
    }

    pub fn event(&mut self, code: EventCode, data: &str, time: Duration) {
        self.flush();
        let event = Event { time, code, data: data.to_string() };
        self.write_event(&event);
    }

    pub fn emit_gap(&mut self, reason: &str, time: Duration) {
        self.flush();
        let data = serde_json::json!({"reason": reason}).to_string();
        let event = Event { time, code: EventCode::Custom('g'), data };
        self.write_event(&event);
    }

    pub fn write_snapshot(&mut self, vt_state: &str, engine: &str, cols: u16, rows: u16, time: Duration) {
        self.flush();
        let data = serde_json::json!({
            "engine": engine,
            "cols": cols,
            "rows": rows,
            "state": vt_state,
        }).to_string();
        let event = Event { time, code: EventCode::Custom('S'), data };
        self.write_event(&event);
        self.output_bytes_since_snapshot = 0;
    }

    pub fn flush(&mut self) {
        if let Some((code, data, time)) = self.buffer.take() {
            let text = String::from_utf8_lossy(&data).into_owned();
            let event = Event { time, code, data: text };
            self.write_event(&event);
        }
    }

    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    pub fn output_bytes_since_snapshot(&self) -> u64 {
        self.output_bytes_since_snapshot
    }

    pub fn session_dir(&self) -> &Path {
        &self.session_dir
    }

    fn write_event(&mut self, event: &Event) {
        let mut line = asciicast::encode_event(event, &mut self.prev_time);
        line.push('\n');
        if let Err(e) = self.cast_file.write_all(line.as_bytes()) {
            eprintln!("recording write error: {e}");
            return;
        }
        self.bytes_written += line.len() as u64;
    }
}
```

- [ ] **Step 11: Run tests to verify they pass**

Run: `cargo test --workspace --locked -p cleat --test recording`
Expected: all tests pass.

- [ ] **Step 12: Run full checks**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings && cargo +nightly-2026-03-12 fmt --check`
Expected: clean.

- [ ] **Step 13: Commit**

```bash
git add crates/cleat/src/recording.rs crates/cleat/tests/recording.rs
git commit -m "feat(recording): rewrite recording module with asciicast v3 events and coalescing"
```

---

## Task 3: Wire new recorder into daemon event loop

**Files:**
- Modify: `crates/cleat/src/session.rs:448-685` (the `run_session_daemon` function)

This is the integration task. The daemon's event loop currently calls `OutputRecorder::record()` and `write_snapshot()`. We replace these with `SessionRecorder` calls, add input recording, and emit lifecycle events.

- [ ] **Step 0: Update build_inspect_result signature**

The `build_inspect_result` function (~line 692) accepts `&Option<crate::recording::OutputRecorder>`. Update it to accept the new type:

```rust
fn build_inspect_result(
    session: &SessionMetadata,
    vt_engine: &dyn VtEngine,
    active_client: &Option<ActiveClient>,
    pty_child: &PtyChild,
    recorder: &Option<crate::recording::SessionRecorder>,
) -> crate::protocol::InspectResult {
```

The function body calls `recorder.as_ref().map(|r| r.bytes_written())` which works identically on the new type.

- [ ] **Step 1: Update recorder creation**

In `run_session_daemon`, replace the `OutputRecorder` creation block (~lines 469-477):

```rust
// Old:
// let mut recorder: Option<crate::recording::OutputRecorder> = None;
// if session.record || std::env::var("CLEAT_RECORD") ...
//     match crate::recording::OutputRecorder::new(...) { ... }
// let mut bytes_since_snapshot: u64 = 0;

// New:
let mut recorder: Option<crate::recording::SessionRecorder> = None;
let epoch = Instant::now();
if session.record || std::env::var("CLEAT_RECORD").map(|v| v == "1").unwrap_or(false) {
    let (cols, rows) = vt_engine.size();
    match crate::recording::SessionRecorder::new(
        &root.join(id), cols, rows, session.vt_engine.as_str(),
    ) {
        Ok(mut r) => {
            // Bootstrap: emit initial snapshot if VT engine has state
            if let Ok(Some(payload)) = vt_engine.replay_payload(&vt::ClientCapabilities::conservative_fallback()) {
                let state = String::from_utf8_lossy(&payload);
                r.write_snapshot(&state, session.vt_engine.as_str(), cols, rows, Duration::ZERO);
            }
            recorder = Some(r);
        }
        Err(err) => eprintln!("failed to start recording: {err}"),
    }
}
```

Remove the `bytes_since_snapshot` variable and `SNAPSHOT_INTERVAL_BYTES` constant — the recorder tracks this internally.

- [ ] **Step 2: Update PTY output recording**

In the `poll_result.pty_readable` block (~lines 635-656), replace the old recording calls:

```rust
// Old:
// if let Some(ref mut rec) = recorder {
//     if let Err(err) = rec.record(&buf[..n]) { ... }
// }
// if let Some(ref mut rec) = recorder {
//     bytes_since_snapshot += n as u64;
//     if bytes_since_snapshot >= SNAPSHOT_INTERVAL_BYTES { ... }
// }

// New:
if let Some(ref mut rec) = recorder {
    let elapsed = epoch.elapsed();
    rec.output(&buf[..n], elapsed);
    if rec.output_bytes_since_snapshot() >= 256 * 1024 {
        if let Ok(Some(payload)) = vt_engine.replay_payload(&vt::ClientCapabilities::conservative_fallback()) {
            let (cols, rows) = vt_engine.size();
            let state = String::from_utf8_lossy(&payload);
            rec.write_snapshot(&state, session.vt_engine.as_str(), cols, rows, elapsed);
        }
    }
}
```

- [ ] **Step 3: Add input recording**

In the client frame processing block (~lines 607-616), add input recording:

```rust
while let Some(frame) = pending.pop_front() {
    match frame {
        Frame::Input(bytes) => {
            if let Some(ref mut rec) = recorder {
                rec.input(&bytes, epoch.elapsed());
            }
            write_fd_all(pty_fd, &bytes)?;
        }
        Frame::Resize { cols, rows } => {
            if let Some(ref mut rec) = recorder {
                let data = format!("{}x{}", cols, rows);
                rec.event(
                    crate::asciicast::EventCode::Resize,
                    &data,
                    epoch.elapsed(),
                );
            }
            resize_pty(pty_fd, cols, rows)?;
            vt_engine.resize(cols, rows)?;
        }
        _ => {}
    }
}
```

- [ ] **Step 4: Add attach/detach recording**

In the `AttachInit` handler (~line 509, after `active_client = Some(client)`):

```rust
if let Some(ref mut rec) = recorder {
    rec.event(
        crate::asciicast::EventCode::Custom('a'),
        r#"{"client":"foreground"}"#,
        epoch.elapsed(),
    );
}
```

In the `Detach` handler (~line 516) and client disconnect (~line 618):

```rust
if let Some(ref mut rec) = recorder {
    rec.event(
        crate::asciicast::EventCode::Custom('d'),
        r#"{"client":"foreground"}"#,
        epoch.elapsed(),
    );
}
```

- [ ] **Step 5: Add signal recording**

In the `Signal` handler (after `dispatch_signal` call):

```rust
if let Some(ref mut rec) = recorder {
    let target_str = match target {
        crate::protocol::SignalTarget::Foreground => "foreground",
        crate::protocol::SignalTarget::Leader => "leader",
        crate::protocol::SignalTarget::Tree => "tree",
    };
    rec.event(
        crate::asciicast::EventCode::Custom('s'),
        &serde_json::json!({"signal": signal, "target": target_str}).to_string(),
        epoch.elapsed(),
    );
}
```

- [ ] **Step 6: Update RecordControl handler for gap events and snapshots**

Replace the `RecordControl` handler (~lines 551-568):

```rust
Ok(Frame::RecordControl { enable }) => {
    if enable && recorder.is_none() {
        let (cols, rows) = vt_engine.size();
        match crate::recording::SessionRecorder::new(
            &root.join(id), cols, rows, session.vt_engine.as_str(),
        ) {
            Ok(mut r) => {
                // Mid-session start: emit VT state snapshot
                if let Ok(Some(payload)) = vt_engine.replay_payload(
                    &vt::ClientCapabilities::conservative_fallback(),
                ) {
                    let state = String::from_utf8_lossy(&payload);
                    r.write_snapshot(
                        &state, session.vt_engine.as_str(),
                        cols, rows, epoch.elapsed(),
                    );
                }
                recorder = Some(r);
                let _ = Frame::Ack.write(&mut stream);
            }
            Err(err) => {
                let _ = Frame::Error(err).write(&mut stream);
            }
        }
    } else if !enable && recorder.is_some() {
        if let Some(ref mut rec) = recorder {
            rec.flush();
        }
        recorder = None;
        let _ = Frame::Ack.write(&mut stream);
    } else {
        let _ = Frame::Ack.write(&mut stream);
    }
}
```

- [ ] **Step 7: Add idle flush at top of event loop**

After the `poll_ready` call (~line 486), add an idle flush. The poll timeout is 100ms, so if we got here with no events, flush the buffer:

```rust
if let Some(ref mut rec) = recorder {
    if !poll_result.pty_readable && !poll_result.client_readable {
        rec.flush();
    }
}
```

- [ ] **Step 8: Flush recorder and emit exit event before daemon exit**

Before the cleanup block (~line 675), after `child_exited` returns `Some`:

```rust
if let Some(ref mut rec) = recorder {
    // Emit exit event with child status
    if let Some(status) = child_exited(pty_child.pid)? {
        let exit_code = match status {
            WaitStatus::Exited(_, code) => code,
            WaitStatus::Signaled(_, sig, _) => 128 + sig as i32,
            _ => 1,
        };
        rec.event(
            crate::asciicast::EventCode::Exit,
            &exit_code.to_string(),
            epoch.elapsed(),
        );
    }
    rec.flush();
}
```

Note: adapt the existing `child_exited` check to capture the `WaitStatus` before breaking, so the exit code is available for the event.

- [ ] **Step 8b: Preserve session.cast on session exit**

In the cleanup block (~line 683), the daemon currently runs `fs::remove_dir_all(root.join(id))` which destroys the recording. Change this to preserve `session.cast`:

```rust
// Old: let _ = fs::remove_dir_all(root.join(id));
// New: preserve session.cast if recording was active
let session_dir = root.join(id);
if recorder.is_some() {
    // Remove socket and pid file but keep the session directory with session.cast
    let _ = fs::remove_file(&socket_path);
    let _ = fs::remove_file(daemon_pid_path(root, id));
    let _ = fs::remove_file(foreground_path(root, id));
} else {
    let _ = fs::remove_dir_all(&session_dir);
}
```

Also remove the individual `remove_file` calls above this block that are now redundant (lines 680-682).

- [ ] **Step 9: Run full test suite**

Run: `cargo test --workspace --locked`
Expected: all non-lifecycle tests pass. The 3 pre-existing lifecycle failures (#11) may still fail.

- [ ] **Step 10: Run full checks**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings && cargo +nightly-2026-03-12 fmt --check`
Expected: clean.

- [ ] **Step 11: Commit**

```bash
git add crates/cleat/src/session.rs
git commit -m "feat(recording): wire asciicast recorder into daemon with input, lifecycle events, and coalescing"
```

---

## Task 4: Add Mark protocol frame and CLI command

**Files:**
- Modify: `crates/cleat/src/protocol.rs`
- Modify: `crates/cleat/src/session.rs` (daemon Mark handler)
- Modify: `crates/cleat/src/server.rs` (mark method)
- Modify: `crates/cleat/src/cli.rs` (Mark subcommand)

- [ ] **Step 1: Write failing test — Mark frame round-trip**

In `crates/cleat/src/protocol.rs` tests:

```rust
#[test]
fn mark_round_trip() {
    let frame = Frame::Mark;
    let mut bytes = Vec::new();
    frame.write(&mut bytes).expect("write frame");
    let decoded = Frame::read(&mut bytes.as_slice()).expect("read frame");
    assert_eq!(decoded, Frame::Mark);
}

#[test]
fn mark_result_round_trip() {
    let frame = Frame::MarkResult { offset: 12345 };
    let mut bytes = Vec::new();
    frame.write(&mut bytes).expect("write frame");
    let decoded = Frame::read(&mut bytes.as_slice()).expect("read frame");
    assert_eq!(decoded, Frame::MarkResult { offset: 12345 });
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --workspace --locked -p cleat protocol`
Expected: compile error — `Frame::Mark` doesn't exist.

- [ ] **Step 3: Implement Mark/MarkResult frames**

Add to `protocol.rs`:

```rust
// Add constants:
const TAG_MARK: u8 = 15;
const TAG_MARK_RESULT: u8 = 16;

// Add variants to Frame enum:
Mark,
MarkResult { offset: u64 },

// Add encode cases:
Frame::Mark => (TAG_MARK, vec![]),
Frame::MarkResult { offset } => (TAG_MARK_RESULT, offset.to_le_bytes().to_vec()),

// Add decode cases:
TAG_MARK => Ok(Frame::Mark),
TAG_MARK_RESULT => {
    if payload.len() != 8 {
        return Err(Error::new(ErrorKind::InvalidData, "invalid mark result frame"));
    }
    let offset = u64::from_le_bytes([
        payload[0], payload[1], payload[2], payload[3],
        payload[4], payload[5], payload[6], payload[7],
    ]);
    Ok(Frame::MarkResult { offset })
}
```

Note: `RecordingInspect.bytes_written` already reports the `.cast` file byte offset. No new field needed — `Mark` provides the same value via a dedicated frame. The existing `bytes_written` field semantics change from "bytes of raw output" to "bytes in .cast file" but this is more useful and the field name is still accurate.

- [ ] **Step 4: Add Mark handler in daemon**

In `session.rs`, in the listener frame dispatch (alongside Inspect, Signal, etc.):

```rust
Ok(Frame::Mark) => {
    if let Some(ref mut rec) = recorder {
        rec.flush();
        let offset = rec.bytes_written();
        let _ = Frame::MarkResult { offset }.write(&mut stream);
    } else {
        let _ = Frame::Error("recording not active".to_string()).write(&mut stream);
    }
}
```

- [ ] **Step 5: Add mark() to SessionService**

In `server.rs`:

```rust
pub fn mark(&self, id: &str) -> Result<u64, String> {
    if !self.layout.root().join(id).exists() {
        return Err(format!("missing session {id}"));
    }
    let socket_path = session_socket_path(self.layout.root(), id);
    let mut stream = connect_session_socket(&socket_path)?;
    Frame::Mark.write(&mut stream).map_err(|e| format!("write mark: {e}"))?;
    match Frame::read(&mut stream).map_err(|e| format!("read mark response: {e}"))? {
        Frame::MarkResult { offset } => Ok(offset),
        Frame::Error(msg) => Err(msg),
        other => Err(format!("unexpected mark response: {other:?}")),
    }
}
```

- [ ] **Step 6: Write failing test — CLI mark parsing**

In `crates/cleat/tests/cli.rs`, add (matching existing test patterns that use `Cli::try_parse_from`):

```rust
#[test]
fn mark_command_parses_session_id() {
    let cli = Cli::try_parse_from(["cleat", "mark", "my-session"]).expect("mark parses");
    assert_eq!(cli.command, Command::Mark { id: "my-session".into() });
}
```

Also update the `help_lists_expected_subcommands` test to include `"mark"`:

```rust
assert_eq!(subcommands, vec!["attach", "create", "list", "capture", "detach", "kill", "send-keys", "inspect", "signal", "record", "mark"]);
```

- [ ] **Step 7: Add Mark CLI subcommand**

In `cli.rs`:

```rust
// Add to Command enum:
Mark {
    id: String,
},

// Add to execute():
Command::Mark { id } => {
    let offset = service.mark(&id)?;
    Ok(Some(offset.to_string()))
}
```

- [ ] **Step 8: Run full test suite**

Run: `cargo test --workspace --locked`
Expected: pass.

- [ ] **Step 9: Run full checks**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings && cargo +nightly-2026-03-12 fmt --check`
Expected: clean.

- [ ] **Step 10: Commit**

```bash
git add crates/cleat/src/protocol.rs crates/cleat/src/session.rs crates/cleat/src/server.rs crates/cleat/src/cli.rs crates/cleat/tests/cli.rs
git commit -m "feat(recording): add mark command for cursor-based capture offsets"
```

---

## Task 5: Clean up old recording artifacts

**Files:**
- Modify: `crates/cleat/src/session.rs` (remove old snapshot/output.log references if any remain)
- Verify: no remaining references to `output.log`, `snapshots/`, `write_snapshot` (old signature), `OutputRecorder`

- [ ] **Step 1: Search for stale references**

Run: `cargo build --workspace --locked 2>&1`

Fix any remaining compilation errors from the old `OutputRecorder` API.

- [ ] **Step 2: Verify no old file references**

Run: `grep -r "output\.log\|OutputRecorder\|snapshots/" crates/cleat/src/`
Expected: no matches.

- [ ] **Step 3: Run full test suite**

Run: `cargo test --workspace --locked`
Expected: pass.

- [ ] **Step 4: Run full checks**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings && cargo +nightly-2026-03-12 fmt --check`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "chore(recording): remove old output.log and snapshot references"
```
