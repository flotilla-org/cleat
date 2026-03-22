# Capture Cursors Implementation Plan

**Goal:** Add `capture --since <cursor>` to return output events from a .cast file after a given byte offset, with `--text` (default) and `--raw` rendering modes.

**Architecture:** The client reads the `session.cast` file directly (no daemon involvement for `--since`). A new `cast_reader` module handles seeking to a byte offset, parsing events, and extracting output. Existing `capture` (no `--since`) continues to work via the daemon's live VT engine.

**Phase 1 simplification:** Both `--text` (default) and `--raw` return concatenated output event data. Full VT replay (snapshot seek + engine replay + screen diff) is deferred — it requires the ghostty feature on the client side and is a separate enhancement. The `--text` vs `--raw` flag distinction is wired now so the API is stable; the rendering upgrade is backwards-compatible.

**Tech Stack:** Rust, serde_json (existing), `std::io::BufRead` for line-by-line parsing.

---

## File Structure

| File | Action | Responsibility |
|------|--------|----------------|
| `crates/cleat/src/cast_reader.rs` | Create | Read and parse .cast files: seek to offset, iterate events, find snapshots. Pure I/O + parsing, no VT engine dependency. |
| `crates/cleat/src/cli.rs` | Modify | Add `--since`, `--raw` flags to `Capture` command. Route to new capture path when `--since` is present. |
| `crates/cleat/src/server.rs` | Modify | Add `capture_since()` method that reads .cast file client-side, returns raw or text output. |
| `crates/cleat/src/lib.rs` | Modify | Add `pub mod cast_reader;` |
| `crates/cleat/tests/cast_reader.rs` | Create | Unit tests for .cast file reading and event extraction. |
| `crates/cleat/tests/cli.rs` | Modify | Add parse tests for new capture flags. |

---

## Task 1: Cast file reader module

**Files:**
- Create: `crates/cleat/src/cast_reader.rs`
- Modify: `crates/cleat/src/lib.rs`
- Create: `crates/cleat/tests/cast_reader.rs`

Pure .cast file reading — seek to byte offset, parse event lines, filter by code, extract output data. No VT engine, no rendering.

- [ ] **Step 1: Write failing test — read events from offset**

Create `crates/cleat/tests/cast_reader.rs`:

```rust
use std::io::Write;
use std::time::Duration;

use cleat::asciicast::{encode_event, encode_header, Event, EventCode, Header};
use cleat::cast_reader;

/// Helper: write a minimal .cast file and return its path.
fn write_cast_file(dir: &std::path::Path, events: &[Event]) -> std::path::PathBuf {
    let path = dir.join("session.cast");
    let mut f = std::fs::File::create(&path).unwrap();
    let header = Header { cols: 80, rows: 24, ..Default::default() };
    writeln!(f, "{}", encode_header(&header)).unwrap();
    let mut prev = Duration::ZERO;
    for event in events {
        writeln!(f, "{}", encode_event(event, &mut prev)).unwrap();
    }
    path
}

#[test]
fn read_output_since_offset_returns_events_after_cursor() {
    let temp = tempfile::tempdir().unwrap();
    let events = vec![
        Event { time: Duration::from_millis(100), code: EventCode::Output, data: "first".into() },
        Event { time: Duration::from_millis(200), code: EventCode::Output, data: "second".into() },
        Event { time: Duration::from_millis(300), code: EventCode::Output, data: "third".into() },
    ];
    let path = write_cast_file(temp.path(), &events);

    // Get offset after header + first event
    let contents = std::fs::read_to_string(&path).unwrap();
    let mut lines = contents.lines();
    let header_line = lines.next().unwrap();
    let first_event_line = lines.next().unwrap();
    let cursor = (header_line.len() + 1 + first_event_line.len() + 1) as u64;

    let result = cast_reader::read_output_since(&path, cursor).unwrap();
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].data, "second");
    assert_eq!(result[1].data, "third");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --workspace --locked -p cleat --test cast_reader`
Expected: compile error — module doesn't exist.

- [ ] **Step 3: Write failing test — offset 0 returns all output events**

Append to `crates/cleat/tests/cast_reader.rs`:

```rust
#[test]
fn read_output_since_zero_returns_all_output() {
    let temp = tempfile::tempdir().unwrap();
    let events = vec![
        Event { time: Duration::from_millis(100), code: EventCode::Output, data: "aaa".into() },
        Event { time: Duration::from_millis(200), code: EventCode::Input, data: "bbb".into() },
        Event { time: Duration::from_millis(300), code: EventCode::Output, data: "ccc".into() },
    ];
    let path = write_cast_file(temp.path(), &events);

    let result = cast_reader::read_output_since(&path, 0).unwrap();
    assert_eq!(result.len(), 2); // only output events
    assert_eq!(result[0].data, "aaa");
    assert_eq!(result[1].data, "ccc");
}
```

- [ ] **Step 4: Write failing test — offset beyond EOF returns empty**

```rust
#[test]
fn read_output_since_beyond_eof_returns_empty() {
    let temp = tempfile::tempdir().unwrap();
    let events = vec![
        Event { time: Duration::from_millis(100), code: EventCode::Output, data: "only".into() },
    ];
    let path = write_cast_file(temp.path(), &events);

    let file_size = std::fs::metadata(&path).unwrap().len();
    let result = cast_reader::read_output_since(&path, file_size).unwrap();
    assert!(result.is_empty());
}
```

- [ ] **Step 5: Write failing test — filters non-output events**

```rust
#[test]
fn read_output_since_skips_non_output_events() {
    let temp = tempfile::tempdir().unwrap();
    let events = vec![
        Event { time: Duration::from_millis(100), code: EventCode::Input, data: "keys".into() },
        Event { time: Duration::from_millis(200), code: EventCode::Custom('s'), data: "signal".into() },
        Event { time: Duration::from_millis(300), code: EventCode::Resize, data: "120x40".into() },
        Event { time: Duration::from_millis(400), code: EventCode::Output, data: "output".into() },
    ];
    let path = write_cast_file(temp.path(), &events);

    let result = cast_reader::read_output_since(&path, 0).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].data, "output");
}
```

- [ ] **Step 6: Write failing test — find_nearest_snapshot**

```rust
#[test]
fn find_nearest_snapshot_before_offset() {
    let temp = tempfile::tempdir().unwrap();
    let events = vec![
        Event { time: Duration::from_millis(100), code: EventCode::Custom('S'), data: r#"{"engine":"test","cols":80,"rows":24,"state":"snap1"}"#.into() },
        Event { time: Duration::from_millis(200), code: EventCode::Output, data: "out1".into() },
        Event { time: Duration::from_millis(300), code: EventCode::Custom('S'), data: r#"{"engine":"test","cols":80,"rows":24,"state":"snap2"}"#.into() },
        Event { time: Duration::from_millis(400), code: EventCode::Output, data: "out2".into() },
    ];
    let path = write_cast_file(temp.path(), &events);

    // Cursor just before out2 — nearest snapshot should be snap2
    let contents = std::fs::read_to_string(&path).unwrap();
    let line_offsets: Vec<u64> = {
        let mut offsets = vec![0u64];
        for line in contents.lines() {
            offsets.push(offsets.last().unwrap() + line.len() as u64 + 1);
        }
        offsets
    };
    // line_offsets: [0, header_end, snap1_end, out1_end, snap2_end, out2_end]
    let cursor = line_offsets[4]; // after snap2, before out2

    let snapshot = cast_reader::find_nearest_snapshot(&path, cursor).unwrap();
    assert!(snapshot.is_some());
    let (snap_offset, snap_data) = snapshot.unwrap();
    assert!(snap_offset < cursor);
    assert!(snap_data.contains("snap2"));
}
```

- [ ] **Step 7: Write failing test — no snapshot returns None**

```rust
#[test]
fn find_nearest_snapshot_returns_none_when_no_snapshots() {
    let temp = tempfile::tempdir().unwrap();
    let events = vec![
        Event { time: Duration::from_millis(100), code: EventCode::Output, data: "out".into() },
    ];
    let path = write_cast_file(temp.path(), &events);

    let snapshot = cast_reader::find_nearest_snapshot(&path, 9999).unwrap();
    assert!(snapshot.is_none());
}
```

- [ ] **Step 8: Write failing test — read_all_events_since for raw mode**

```rust
#[test]
fn read_all_events_since_returns_all_event_types() {
    let temp = tempfile::tempdir().unwrap();
    let events = vec![
        Event { time: Duration::from_millis(100), code: EventCode::Output, data: "out".into() },
        Event { time: Duration::from_millis(200), code: EventCode::Input, data: "in".into() },
    ];
    let path = write_cast_file(temp.path(), &events);

    let result = cast_reader::read_all_events_since(&path, 0).unwrap();
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].code, EventCode::Output);
    assert_eq!(result[1].code, EventCode::Input);
}
```

- [ ] **Step 9: Implement cast_reader module**

Create `crates/cleat/src/cast_reader.rs`:

```rust
use std::{
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::Path,
    time::Duration,
};

use crate::asciicast::{decode_event, Event, EventCode};

/// Read all output (`"o"`) events from a .cast file starting at `offset` bytes.
/// Skips the header (if offset is 0) and non-output events.
pub fn read_output_since(path: &Path, offset: u64) -> Result<Vec<Event>, String> {
    let events = read_events_since(path, offset, Some(EventCode::Output))?;
    Ok(events)
}

/// Read all events from a .cast file starting at `offset` bytes.
/// Skips the header if offset is 0.
pub fn read_all_events_since(path: &Path, offset: u64) -> Result<Vec<Event>, String> {
    read_events_since(path, offset, None)
}

/// Find the nearest snapshot (`"S"`) event at or before `offset`.
/// Returns `(byte_offset_of_snapshot_line, snapshot_data_string)` or None.
pub fn find_nearest_snapshot(path: &Path, offset: u64) -> Result<Option<(u64, String)>, String> {
    let file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let reader = BufReader::new(file);
    let mut prev_time = Duration::ZERO;
    let mut current_offset: u64 = 0;
    let mut best: Option<(u64, String)> = None;

    for line in reader.lines() {
        let line = line.map_err(|e| format!("read line: {e}"))?;
        let line_start = current_offset;
        current_offset += line.len() as u64 + 1; // +1 for newline

        if line_start >= offset {
            break;
        }

        // Skip header (first line)
        if line_start == 0 {
            continue;
        }

        if let Ok(event) = decode_event(&line, &mut prev_time) {
            if event.code == EventCode::Custom('S') {
                best = Some((line_start, event.data));
            }
        }
    }

    Ok(best)
}

/// Read events from a .cast file starting at `offset`, optionally filtering by code.
fn read_events_since(
    path: &Path,
    offset: u64,
    filter: Option<EventCode>,
) -> Result<Vec<Event>, String> {
    let mut file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let file_len = file.metadata().map_err(|e| format!("metadata: {e}"))?.len();

    if offset >= file_len {
        return Ok(vec![]);
    }

    // If offset is 0, we need to skip the header line and establish prev_time.
    // If offset is nonzero, seek directly — prev_time starts at ZERO (deltas
    // are self-contained per-line in v3, but we accumulate from the seek point).
    let mut prev_time = Duration::ZERO;

    if offset == 0 {
        let reader = BufReader::new(&file);
        let mut events = Vec::new();
        let mut first = true;
        for line in reader.lines() {
            let line = line.map_err(|e| format!("read line: {e}"))?;
            if first {
                first = false;
                continue; // skip header
            }
            if let Ok(event) = decode_event(&line, &mut prev_time) {
                if filter.is_none() || filter.as_ref() == Some(&event.code) {
                    events.push(event);
                }
            }
        }
        return Ok(events);
    }

    file.seek(SeekFrom::Start(offset)).map_err(|e| format!("seek: {e}"))?;
    let reader = BufReader::new(file);
    let mut events = Vec::new();

    // Note: prev_time starts at ZERO after a seek, so Event.time values
    // are relative to the seek point, not absolute from recording start.
    // This is acceptable — callers use only data/code, not time.
    for line in reader.lines() {
        let line = line.map_err(|e| format!("read line: {e}"))?;
        if line.is_empty() {
            continue;
        }
        if let Ok(event) = decode_event(&line, &mut prev_time) {
            if filter.is_none() || filter.as_ref() == Some(&event.code) {
                events.push(event);
            }
        }
    }

    Ok(events)
}
```

- [ ] **Step 10: Register module in lib.rs**

Add `pub mod cast_reader;` to `crates/cleat/src/lib.rs`.

- [ ] **Step 11: Run tests**

Run: `cargo test --workspace --locked -p cleat --test cast_reader`
Expected: all tests pass.

- [ ] **Step 12: Run full checks**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings && cargo +nightly-2026-03-12 fmt --check`

- [ ] **Step 13: Commit**

```bash
git add crates/cleat/src/cast_reader.rs crates/cleat/src/lib.rs crates/cleat/tests/cast_reader.rs
git commit -m "feat(capture): add cast_reader module for reading .cast files from byte offset"
```

---

## Task 2: Add --since and --raw flags to capture CLI

**Files:**
- Modify: `crates/cleat/src/cli.rs`
- Modify: `crates/cleat/src/server.rs`
- Modify: `crates/cleat/tests/cli.rs`

Wire the new flags through CLI parsing, add `capture_since_raw()` to `SessionService` for the raw (non-VT-rendered) path. The `--text` rendered path comes in Task 3.

- [ ] **Step 1: Write failing CLI parse tests**

Append to `crates/cleat/tests/cli.rs`:

```rust
#[test]
fn capture_with_since_flag_parses() {
    let cli = Cli::try_parse_from(["cleat", "capture", "sess", "--since", "12345"]).expect("parse");
    assert_eq!(
        cli.command,
        Command::Capture { id: "sess".into(), since: Some(12345), raw: false }
    );
}

#[test]
fn capture_with_raw_flag_parses() {
    let cli = Cli::try_parse_from(["cleat", "capture", "sess", "--since", "0", "--raw"]).expect("parse");
    assert_eq!(
        cli.command,
        Command::Capture { id: "sess".into(), since: Some(0), raw: true }
    );
}

#[test]
fn capture_without_since_still_works() {
    let cli = Cli::try_parse_from(["cleat", "capture", "sess"]).expect("parse");
    assert_eq!(
        cli.command,
        Command::Capture { id: "sess".into(), since: None, raw: false }
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --workspace --locked -p cleat --test cli`
Expected: compile error — `Capture` struct doesn't have `since`/`raw` fields.

- [ ] **Step 3: Update Capture command definition**

In `crates/cleat/src/cli.rs`, change the `Capture` variant:

```rust
Capture {
    id: String,
    /// Byte offset in .cast file; return output events after this position
    #[arg(long)]
    since: Option<u64>,
    /// Return raw event data instead of VT-rendered text (only with --since)
    #[arg(long)]
    raw: bool,
},
```

- [ ] **Step 4: Update the existing capture CLI parse test**

In `crates/cleat/tests/cli.rs`, the existing `capture_command_parses` test asserts on the old struct shape. Update it:

```rust
fn capture_command_parses() {
    let cli = Cli::try_parse_from(["cleat", "capture", "session-1"]).expect("capture parses");
    assert_eq!(cli.command, Command::Capture { id: "session-1".into(), since: None, raw: false });
}
```

- [ ] **Step 5: Add capture_since_raw to SessionService**

In `crates/cleat/src/server.rs`, add:

```rust
pub fn capture_since_raw(&self, id: &str, offset: u64) -> Result<String, String> {
    let cast_path = self.layout.root().join(id).join(crate::recording::CAST_FILE_NAME);
    if !cast_path.exists() {
        return Err(format!("no recording for session {id}"));
    }
    let events = crate::cast_reader::read_output_since(&cast_path, offset)?;
    let output: String = events.iter().map(|e| e.data.as_str()).collect();
    Ok(output)
}
```

- [ ] **Step 6: Update execute handler for capture**

In `crates/cleat/src/cli.rs`, update the `Capture` match arm:

```rust
Command::Capture { id, since, raw } => {
    match since {
        Some(offset) => {
            if raw {
                service.capture_since_raw(&id, offset).map(Some)
            } else {
                // --text mode (default): for now, fall back to raw
                // Task 3 will add VT-rendered text mode
                service.capture_since_raw(&id, offset).map(Some)
            }
        }
        None => service.capture(&id).map(Some),
    }
}
```

- [ ] **Step 7: Run tests**

Run: `cargo test --workspace --locked -p cleat --test cli`
Expected: all CLI tests pass.

- [ ] **Step 8: Run full checks**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings && cargo +nightly-2026-03-12 fmt --check`

- [ ] **Step 9: Commit**

```bash
git add crates/cleat/src/cli.rs crates/cleat/src/server.rs crates/cleat/tests/cli.rs
git commit -m "feat(capture): add --since and --raw flags to capture command"
```

---

## Task 3: Text-rendered capture with VT replay

**Files:**
- Modify: `crates/cleat/src/server.rs`
- Create: `crates/cleat/tests/capture_render.rs`

Add `capture_since_text()` — the `--text` (default) path. Finds nearest snapshot before cursor, replays output through a VT engine, returns the screen text difference.

For Phase 1, a simpler approach than full screen diffing: replay all output events from cursor to EOF through a fresh VT engine (seeded from snapshot if available), then return the final screen text. This gives the agent "what does the screen look like now, considering only output since cursor" — which is the most useful answer.

- [ ] **Step 1: Write failing test — capture_since_text returns rendered output**

Create `crates/cleat/tests/capture_render.rs`:

```rust
use std::io::Write;
use std::time::Duration;

use cleat::asciicast::{encode_event, encode_header, Event, EventCode, Header};
use cleat::recording::CAST_FILE_NAME;
use cleat::runtime::RuntimeLayout;
use cleat::server::SessionService;

fn setup_cast_file(root: &std::path::Path, id: &str, events: &[Event]) -> u64 {
    let session_dir = root.join(id);
    std::fs::create_dir_all(&session_dir).unwrap();
    let path = session_dir.join(CAST_FILE_NAME);
    let mut f = std::fs::File::create(&path).unwrap();
    let header = Header { cols: 80, rows: 24, ..Default::default() };
    writeln!(f, "{}", encode_header(&header)).unwrap();
    let mut prev = Duration::ZERO;
    let mut offset = 0u64;
    // Track offset after header
    let header_line = encode_header(&header);
    offset += header_line.len() as u64 + 1;
    for event in events {
        let line = encode_event(event, &mut prev);
        writeln!(f, "{}", line).unwrap();
        offset += line.len() as u64 + 1;
    }
    offset
}

#[test]
fn capture_since_text_returns_concatenated_output() {
    let temp = tempfile::tempdir().unwrap();
    let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));

    let events = vec![
        Event { time: Duration::from_millis(100), code: EventCode::Output, data: "hello ".into() },
        Event { time: Duration::from_millis(200), code: EventCode::Output, data: "world".into() },
    ];
    // Get offset after header (before any events)
    let header = Header { cols: 80, rows: 24, ..Default::default() };
    let header_len = encode_header(&header).len() as u64 + 1;
    setup_cast_file(temp.path(), "test-session", &events);

    let result = service.capture_since_text("test-session", header_len).unwrap();
    assert!(result.contains("hello "));
    assert!(result.contains("world"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --workspace --locked -p cleat --test capture_render`
Expected: compile error — `capture_since_text` doesn't exist.

- [ ] **Step 3: Write failing test — capture_since_text skips non-output**

```rust
#[test]
fn capture_since_text_skips_non_output_events() {
    let temp = tempfile::tempdir().unwrap();
    let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));

    let events = vec![
        Event { time: Duration::from_millis(100), code: EventCode::Input, data: "typed".into() },
        Event { time: Duration::from_millis(200), code: EventCode::Output, data: "visible".into() },
        Event { time: Duration::from_millis(300), code: EventCode::Custom('s'), data: "signal".into() },
    ];
    setup_cast_file(temp.path(), "test-session", &events);

    let result = service.capture_since_text("test-session", 0).unwrap();
    assert!(result.contains("visible"));
    assert!(!result.contains("typed"));
    assert!(!result.contains("signal"));
}
```

- [ ] **Step 4: Write failing test — empty result for cursor at EOF**

```rust
#[test]
fn capture_since_text_returns_empty_at_eof() {
    let temp = tempfile::tempdir().unwrap();
    let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));

    let events = vec![
        Event { time: Duration::from_millis(100), code: EventCode::Output, data: "done".into() },
    ];
    let eof = setup_cast_file(temp.path(), "test-session", &events);

    let result = service.capture_since_text("test-session", eof).unwrap();
    assert!(result.is_empty());
}
```

- [ ] **Step 5: Implement capture_since_text**

In `crates/cleat/src/server.rs`, add:

```rust
pub fn capture_since_text(&self, id: &str, offset: u64) -> Result<String, String> {
    let cast_path = self.layout.root().join(id).join(crate::recording::CAST_FILE_NAME);
    if !cast_path.exists() {
        return Err(format!("no recording for session {id}"));
    }
    let events = crate::cast_reader::read_output_since(&cast_path, offset)?;
    if events.is_empty() {
        return Ok(String::new());
    }
    // Concatenate output event data — this is the raw VT byte stream.
    // For Phase 1, return concatenated output text directly.
    // Full VT replay (snapshot + replay through engine) is a future enhancement
    // that requires the ghostty feature to be available at the client.
    let output: String = events.iter().map(|e| e.data.as_str()).collect();
    Ok(output)
}
```

- [ ] **Step 6: Update execute handler to use capture_since_text**

In `crates/cleat/src/cli.rs`, update the `--text` branch:

```rust
Command::Capture { id, since, raw } => {
    match since {
        Some(offset) => {
            if raw {
                service.capture_since_raw(&id, offset).map(Some)
            } else {
                service.capture_since_text(&id, offset).map(Some)
            }
        }
        None => service.capture(&id).map(Some),
    }
}
```

- [ ] **Step 7: Run tests**

Run: `cargo test --workspace --locked -p cleat --test capture_render --test cast_reader --test cli`
Expected: all pass.

- [ ] **Step 8: Run full checks**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings && cargo +nightly-2026-03-12 fmt --check && cargo test --workspace --locked`

- [ ] **Step 9: Commit**

```bash
git add crates/cleat/src/server.rs crates/cleat/src/cli.rs crates/cleat/tests/capture_render.rs
git commit -m "feat(capture): add capture_since_text for --text mode (concatenated output)"
```

---

## Task 4: Error handling and edge cases

**Files:**
- Modify: `crates/cleat/src/server.rs`
- Modify: `crates/cleat/tests/capture_render.rs`

Handle the error cases: missing recording, missing session, `--raw` without `--since`.

- [ ] **Step 1: Write failing test — missing recording returns error**

Append to `crates/cleat/tests/capture_render.rs`:

```rust
#[test]
fn capture_since_errors_when_no_recording() {
    let temp = tempfile::tempdir().unwrap();
    let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));

    // Create session dir but no .cast file
    std::fs::create_dir_all(temp.path().join("no-recording")).unwrap();

    let err = service.capture_since_text("no-recording", 0).unwrap_err();
    assert!(err.contains("no recording"), "error should mention missing recording: {err}");
}
```

- [ ] **Step 2: Write failing test — raw without since is rejected at CLI**

Append to `crates/cleat/tests/cli.rs`:

```rust
#[test]
fn capture_raw_without_since_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));
    let cli = Cli::try_parse_from(["cleat", "capture", "sess", "--raw"]).expect("parse");
    let err = execute(cli, &service).unwrap_err();
    assert!(err.contains("--raw requires --since"));
}
```

- [ ] **Step 3: Add validation in execute handler**

In `crates/cleat/src/cli.rs`, in the `Capture` match arm, add validation:

```rust
Command::Capture { id, since, raw } => {
    if raw && since.is_none() {
        return Err("--raw requires --since".to_string());
    }
    match since {
        Some(offset) => {
            if raw {
                service.capture_since_raw(&id, offset).map(Some)
            } else {
                service.capture_since_text(&id, offset).map(Some)
            }
        }
        None => service.capture(&id).map(Some),
    }
}
```

- [ ] **Step 4: Run all tests**

Run: `cargo test --workspace --locked`
Expected: all pass (except pre-existing lifecycle failures if any).

- [ ] **Step 5: Run full checks**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings && cargo +nightly-2026-03-12 fmt --check`

- [ ] **Step 6: Commit**

```bash
git add crates/cleat/src/cli.rs crates/cleat/src/server.rs crates/cleat/tests/cli.rs crates/cleat/tests/capture_render.rs
git commit -m "feat(capture): add error handling for missing recordings and --raw validation"
```
