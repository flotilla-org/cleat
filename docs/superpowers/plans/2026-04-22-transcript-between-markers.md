# Transcript End-Bounds Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add four end-bound flags to `cleat transcript` (`--until`, `--until-marker`, `--until-next-marker`, `--until-idle`) and harmonise `wait --idle-time` to accept humantime durations. Closes #52.

**Architecture:** The cast file is already client-side accessible via `cast_reader`, and marker resolution already crosses the daemon socket. We extend `cast_reader` with range + idle-gap helpers, add one new protocol frame for "resolve next marker after offset," replace the existing `capture_since_*` client methods with `capture_slice_{raw,text}(StartBound, EndBound)`, and wire up the CLI flags with mutual-exclusion constraints.

**Tech Stack:** Rust (stable), `clap` for CLI, `humantime` crate (new dep, minimal) for duration parsing. All changes live in the `cleat` crate.

**Spec:** `docs/superpowers/specs/2026-04-22-transcript-between-markers-design.md`

**Conventions:**
- Run all commands from the repo root.
- Per `CLAUDE.md`, always run these gates: `cargo +nightly-2026-03-12 fmt --check`, `cargo build --locked`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test --workspace --locked`. For feature-on coverage additionally: `cargo build --features ghostty-vt --locked`, `cargo clippy --workspace --all-targets --features cleat/ghostty-vt --locked -- -D warnings`, `cargo test -p cleat --features ghostty-vt --locked`.
- `ghostty_terminal_set` and other FFI are established in prior PRs and untouched by this plan.
- Commits are individual per task; each commit passes all seven gates. We squash/rebase at merge time if needed.

---

## File Structure

| File | Responsibility | Action |
|---|---|---|
| `crates/cleat/Cargo.toml` | workspace/cleat package manifest | Modify: add `humantime` dependency |
| `crates/cleat/src/duration_parser.rs` | shared humantime+seconds parser used by `--until-idle` and `wait --idle-time` | Create |
| `crates/cleat/src/lib.rs` | crate root — module exports | Modify: add `pub mod duration_parser;` |
| `crates/cleat/src/cast_reader.rs` | cast-file parsing helpers | Modify: add `read_output_between` and `find_idle_gap_after` |
| `crates/cleat/src/protocol.rs` | socket frame types | Modify: add `Frame::ResolveNextMarker { after: u64 }` + tag + encode/decode |
| `crates/cleat/src/session.rs` | daemon session loop (serves protocol frames) | Modify: handle `ResolveNextMarker` branch |
| `crates/cleat/src/server.rs` | client `Service` struct (wraps socket calls + cast-file reads) | Modify: retire `capture_since_*`, add `capture_slice_{raw,text}`, `resolve_next_marker_after`, define `StartBound`/`EndBound`/`SliceOutcome` |
| `crates/cleat/src/cli.rs` | CLI flag parsing + dispatch | Modify: extend `Transcript` struct with end-bound flags, update dispatch, update `wait --idle-time` to use shared parser |
| `crates/cleat/tests/capture_render.rs` | unit tests for capture-to-text | Modify: migrate callers from `capture_since_text` to `capture_slice_text` |
| `crates/cleat/tests/lifecycle.rs` | end-to-end session tests | Modify: migrate `detached_session_answers_da_queries` caller; add new tests for each end-bound variant |

No other files are created. No workspace-level changes needed.

---

## Task 1: Add humantime dep and shared duration parser

**Files:**
- Modify: `crates/cleat/Cargo.toml` (add to `[dependencies]`)
- Create: `crates/cleat/src/duration_parser.rs`
- Modify: `crates/cleat/src/lib.rs` (add `pub mod duration_parser;`)

**Goal:** One shared parser function that accepts both humantime-suffixed strings (`500ms`, `2s`, `1m30s`) and plain numeric seconds (`2`, `0.5`). Used by `--until-idle` and `wait --idle-time`.

- [ ] **Step 1: Add humantime to Cargo.toml.**

Find the `[dependencies]` section of `crates/cleat/Cargo.toml`. Add:

```toml
humantime = "2"
```

Keep it alphabetically sorted if the existing deps are alphabetical.

- [ ] **Step 2: Create the parser module.**

Write to `crates/cleat/src/duration_parser.rs`:

```rust
//! Duration parser accepting both humantime-suffixed strings and plain
//! numeric seconds. Used by `--until-idle` on `transcript` and
//! `--idle-time` on `wait`.

use std::time::Duration;

/// Parse a duration string. Accepts:
/// - humantime forms: `500ms`, `2s`, `1m30s`, `250us`, etc.
/// - plain float seconds: `2`, `0.5`, `10.25`.
///
/// Humantime is tried first; falls back to float parsing on failure.
pub fn parse_humantime_or_seconds(s: &str) -> Result<Duration, String> {
    if let Ok(d) = humantime::parse_duration(s) {
        return Ok(d);
    }
    s.parse::<f64>()
        .map(Duration::from_secs_f64)
        .map_err(|_| format!("invalid duration: {s}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_humantime_milliseconds() {
        assert_eq!(parse_humantime_or_seconds("500ms").unwrap(), Duration::from_millis(500));
    }

    #[test]
    fn accepts_humantime_seconds() {
        assert_eq!(parse_humantime_or_seconds("2s").unwrap(), Duration::from_secs(2));
    }

    #[test]
    fn accepts_humantime_compound() {
        assert_eq!(parse_humantime_or_seconds("1m30s").unwrap(), Duration::from_secs(90));
    }

    #[test]
    fn accepts_plain_integer_seconds() {
        assert_eq!(parse_humantime_or_seconds("2").unwrap(), Duration::from_secs(2));
    }

    #[test]
    fn accepts_plain_float_seconds() {
        assert_eq!(parse_humantime_or_seconds("0.5").unwrap(), Duration::from_millis(500));
    }

    #[test]
    fn rejects_invalid_input() {
        let err = parse_humantime_or_seconds("not a duration").unwrap_err();
        assert!(err.contains("invalid duration"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_empty_string() {
        assert!(parse_humantime_or_seconds("").is_err());
    }
}
```

- [ ] **Step 3: Export the module from the crate root.**

In `crates/cleat/src/lib.rs`, add alongside the existing `pub mod` declarations (keep alphabetical order):

```rust
pub mod duration_parser;
```

- [ ] **Step 4: Run the tests.**

Run: `cargo test -p cleat --lib duration_parser --locked`

Expected: 7 tests pass, 0 failed.

- [ ] **Step 5: Run clippy.**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

Expected: clean.

- [ ] **Step 6: Commit.**

```bash
git add crates/cleat/Cargo.toml crates/cleat/src/duration_parser.rs crates/cleat/src/lib.rs
git commit -m "duration_parser: accept humantime or plain-seconds forms"
```

---

## Task 2: Extend cast_reader with range and idle-gap helpers

**Files:**
- Modify: `crates/cleat/src/cast_reader.rs`

**Goal:** Two new helpers. `read_output_between(path, start, end)` reads output events in byte range `[start, end)`. `find_idle_gap_after(path, start, threshold)` scans output events starting at `start` and returns the byte offset of the *last* event before the first gap ≥ `threshold`, or `None` if no such gap exists.

Existing `read_output_since` is retained — `read_output_between` is a superset but callers outside the new slice path should keep working.

- [ ] **Step 1: Write failing tests.**

Append to the `#[cfg(test)] mod tests { ... }` block at the end of `crates/cleat/src/cast_reader.rs` (or create one if absent):

```rust
#[cfg(test)]
mod tests_between_and_idle {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_cast(lines: &[&str]) -> NamedTempFile {
        let mut f = NamedTempFile::new().expect("tempfile");
        for line in lines {
            writeln!(f, "{line}").expect("write line");
        }
        f.flush().expect("flush");
        f
    }

    // Minimal asciicast v3 header + three output events at 0.0s, 0.1s, 0.4s.
    // Byte offsets computed from the known line lengths.
    const HEADER: &str = r#"{"version":3,"term":{"cols":80,"rows":24}}"#;
    const EVT_A: &str = r#"[0.0,"o","a"]"#;
    const EVT_B: &str = r#"[0.1,"o","b"]"#;
    const EVT_C: &str = r#"[0.4,"o","c"]"#;

    #[test]
    fn read_between_returns_events_in_range() {
        let f = write_cast(&[HEADER, EVT_A, EVT_B, EVT_C]);
        let header_len = (HEADER.len() + 1) as u64;
        let a_len = (EVT_A.len() + 1) as u64;
        let b_len = (EVT_B.len() + 1) as u64;

        // Range covers A and B only (ends just before C).
        let events = read_output_between(f.path(), header_len, header_len + a_len + b_len)
            .expect("read range");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "a");
        assert_eq!(events[1].data, "b");
    }

    #[test]
    fn read_between_empty_when_start_equals_end() {
        let f = write_cast(&[HEADER, EVT_A]);
        let header_len = (HEADER.len() + 1) as u64;
        let events = read_output_between(f.path(), header_len, header_len).expect("read");
        assert!(events.is_empty());
    }

    #[test]
    fn find_idle_gap_detects_gap_above_threshold() {
        let f = write_cast(&[HEADER, EVT_A, EVT_B, EVT_C]);
        // A→B is 0.1s; B→C is 0.3s. Threshold 0.2s should match B→C gap.
        // Slice should end at the offset *after* event B (first byte of C).
        let header_len = (HEADER.len() + 1) as u64;
        let a_len = (EVT_A.len() + 1) as u64;
        let b_len = (EVT_B.len() + 1) as u64;
        let end = find_idle_gap_after(f.path(), header_len, Duration::from_millis(200))
            .expect("find gap");
        assert_eq!(end, Some(header_len + a_len + b_len));
    }

    #[test]
    fn find_idle_gap_returns_none_when_no_gap_big_enough() {
        let f = write_cast(&[HEADER, EVT_A, EVT_B, EVT_C]);
        let header_len = (HEADER.len() + 1) as u64;
        // Threshold 1s; no gap in fixture is that large.
        let end = find_idle_gap_after(f.path(), header_len, Duration::from_secs(1))
            .expect("find gap");
        assert_eq!(end, None);
    }

    #[test]
    fn find_idle_gap_returns_none_on_empty_range() {
        let f = write_cast(&[HEADER]);
        let header_len = (HEADER.len() + 1) as u64;
        let end = find_idle_gap_after(f.path(), header_len, Duration::from_millis(100))
            .expect("find gap");
        assert_eq!(end, None);
    }
}
```

- [ ] **Step 2: Run the tests — expect fail.**

Run: `cargo test -p cleat --lib cast_reader::tests_between_and_idle --locked`

Expected: FAIL — functions `read_output_between` and `find_idle_gap_after` don't exist.

- [ ] **Step 3: Implement `read_output_between`.**

In `crates/cleat/src/cast_reader.rs`, near the top (after the module-level doc comment or after the existing `read_output_since`), add:

```rust
/// Read all output (`"o"`) events whose starting byte offset is in `[start, end)`.
///
/// When `start` is 0, the header line is skipped automatically.
/// Returns an empty vec if `start >= end` or `start` is at/beyond EOF.
pub fn read_output_between(path: &Path, start: u64, end: u64) -> Result<Vec<Event>, String> {
    if start >= end {
        return Ok(Vec::new());
    }
    read_events_between(path, start, end, Some(EventCode::Output))
}

fn read_events_between(
    path: &Path,
    start: u64,
    end: u64,
    filter: Option<EventCode>,
) -> Result<Vec<Event>, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("open {path:?}: {e}"))?;
    let mut reader = BufReader::new(file);

    if start > 0 {
        reader.seek(SeekFrom::Start(start)).map_err(|e| format!("seek: {e}"))?;
    }

    let mut events = Vec::new();
    let mut line = String::new();
    let mut byte_pos = start;
    let mut prev_time = Duration::ZERO;
    let mut first_line = start == 0;

    loop {
        if byte_pos >= end {
            break;
        }
        line.clear();
        let n = reader.read_line(&mut line).map_err(|e| format!("read line: {e}"))?;
        if n == 0 {
            break;
        }
        let line_start = byte_pos;
        byte_pos += n as u64;

        if first_line {
            // Skip header.
            first_line = false;
            continue;
        }

        // Only include events whose *line start* is inside the range.
        if line_start >= end {
            break;
        }

        let trimmed = line.trim_end_matches('\n');
        if trimmed.is_empty() {
            continue;
        }
        match decode_event(trimmed, prev_time) {
            Ok((event, next_time)) => {
                prev_time = next_time;
                let matches_filter = filter.map_or(true, |code| event.code == code);
                if matches_filter {
                    events.push(event);
                }
            }
            Err(_) => {
                // Skip unparseable lines (same behavior as existing read_events_since).
                continue;
            }
        }
    }

    Ok(events)
}
```

If the existing `read_events_since` has a similar private helper, factor out common code only if you can do it without changing observable behavior of the existing function. If in doubt, keep the two as separate private helpers — duplication is cheaper than subtle regressions here.

- [ ] **Step 4: Implement `find_idle_gap_after`.**

Add to `cast_reader.rs`:

```rust
/// Scan output events starting at `start` and return the byte offset of the
/// *first byte after the last output event before* the first inter-event gap
/// whose duration is ≥ `threshold`.
///
/// Returns `Ok(None)` if no such gap is found before EOF.
///
/// Only output (`"o"`) events participate in gap detection. Non-output events
/// (markers, snapshots, etc.) are skipped.
pub fn find_idle_gap_after(path: &Path, start: u64, threshold: Duration) -> Result<Option<u64>, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("open {path:?}: {e}"))?;
    let mut reader = BufReader::new(file);

    if start > 0 {
        reader.seek(SeekFrom::Start(start)).map_err(|e| format!("seek: {e}"))?;
    }

    let mut line = String::new();
    let mut byte_pos = start;
    let mut prev_time = Duration::ZERO;
    let mut first_line = start == 0;
    let mut last_output_end: Option<u64> = None;
    let mut last_output_time: Option<Duration> = None;

    loop {
        line.clear();
        let n = reader.read_line(&mut line).map_err(|e| format!("read line: {e}"))?;
        if n == 0 {
            break;
        }
        let line_start = byte_pos;
        byte_pos += n as u64;

        if first_line {
            first_line = false;
            continue;
        }

        let trimmed = line.trim_end_matches('\n');
        if trimmed.is_empty() {
            continue;
        }

        match decode_event(trimmed, prev_time) {
            Ok((event, next_time)) => {
                prev_time = next_time;
                if event.code != EventCode::Output {
                    continue;
                }
                // Compute absolute timestamp of *this* event. Since `prev_time`
                // tracks cumulative time relative to the seek point, event.time
                // relative to the previous is (next_time - prev_time_before).
                // But we just need inter-event deltas among output events.
                if let Some(prev_t) = last_output_time {
                    let gap = next_time.saturating_sub(prev_t);
                    if gap >= threshold {
                        // The gap between the previous output event and this one
                        // meets the threshold. Slice should end at the start of
                        // this event, i.e., last_output_end.
                        return Ok(last_output_end);
                    }
                }
                last_output_time = Some(next_time);
                last_output_end = Some(byte_pos);
            }
            Err(_) => continue,
        }
        // avoid unused assignment warnings if we never entered the Ok arm
        let _ = line_start;
    }

    Ok(None)
}
```

- [ ] **Step 5: Run the tests — expect pass.**

Run: `cargo test -p cleat --lib cast_reader::tests_between_and_idle --locked`

Expected: 5 tests pass, 0 failed.

If a test fails, most likely causes:
- Byte offset arithmetic off-by-one: the `+1` in `(HEADER.len() + 1)` accounts for the trailing newline written by `writeln!`. Verify the fixture matches.
- Gap-detection timing: `decode_event` returns cumulative time relative to the seek point, not delta. Two consecutive output events produce a gap equal to `next_time - prev_output_time`. Trace through the test values: A at 0.0, B at 0.1, C at 0.4. `threshold=200ms` ⇒ first matching gap is B→C (300ms ≥ 200ms), so end is offset-after-B.

- [ ] **Step 6: Run full gates.**

```bash
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

All green.

- [ ] **Step 7: Commit.**

```bash
git add crates/cleat/src/cast_reader.rs
git commit -m "cast_reader: add read_output_between and find_idle_gap_after"
```

---

## Task 3: Add `Frame::ResolveNextMarker` to protocol

**Files:**
- Modify: `crates/cleat/src/protocol.rs`

**Goal:** New request frame for "give me the marker with smallest offset strictly greater than N." Response reuses the existing `Frame::MarkResult { offset }` variant for success, or `Frame::Error` for "no such marker."

- [ ] **Step 1: Write a round-trip test for the new frame.**

Find the existing `mod tests` block in `crates/cleat/src/protocol.rs` (around the `resolve_marker_round_trip` test). Add:

```rust
    #[test]
    fn resolve_next_marker_round_trip() {
        let frame = Frame::ResolveNextMarker { after: 12345 };
        let mut buf = Vec::new();
        frame.write(&mut buf).expect("write");
        let decoded = Frame::read(&mut std::io::Cursor::new(buf)).expect("read");
        assert_eq!(frame, decoded);
    }
```

- [ ] **Step 2: Run the test — expect compile error.**

Run: `cargo test -p cleat --lib protocol::tests::resolve_next_marker --locked`

Expected: compile error — `Frame::ResolveNextMarker` doesn't exist.

- [ ] **Step 3: Declare the new tag.**

Near the other `const TAG_*` definitions in `crates/cleat/src/protocol.rs`, find the highest existing tag number and add the next sequential value:

```rust
const TAG_RESOLVE_NEXT_MARKER: u8 = 24;
```

(Use whatever value is one higher than the current maximum; 24 is illustrative — inspect the file and pick the next unused number.)

- [ ] **Step 4: Add the enum variant.**

In the `Frame` enum definition, add a new variant:

```rust
    ResolveNextMarker { after: u64 },
```

Place it alphabetically among the existing variants, or next to `ResolveMarker` if the existing ordering is semantic. Follow the file's existing convention.

- [ ] **Step 5: Add encode/decode arms.**

In `Frame::encode`, add the match arm:

```rust
            Frame::ResolveNextMarker { after } => {
                let mut payload = Vec::with_capacity(8);
                payload.extend_from_slice(&after.to_le_bytes());
                (TAG_RESOLVE_NEXT_MARKER, payload)
            }
```

In `Frame::decode`, add the match arm:

```rust
            TAG_RESOLVE_NEXT_MARKER => {
                if payload.len() != 8 {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        format!("ResolveNextMarker payload must be 8 bytes, got {}", payload.len()),
                    ));
                }
                let after = u64::from_le_bytes(payload[..8].try_into().expect("len checked"));
                Ok(Frame::ResolveNextMarker { after })
            }
```

- [ ] **Step 6: Run the test — expect pass.**

Run: `cargo test -p cleat --lib protocol::tests::resolve_next_marker --locked`

Expected: PASS.

- [ ] **Step 7: Full gates.**

```bash
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

All green.

- [ ] **Step 8: Commit.**

```bash
git add crates/cleat/src/protocol.rs
git commit -m "protocol: add ResolveNextMarker { after } frame"
```

---

## Task 4: Daemon handles `ResolveNextMarker`

**Files:**
- Modify: `crates/cleat/src/session.rs`

**Goal:** When the daemon receives `Frame::ResolveNextMarker { after }`, scan the in-memory marker table for the minimum offset strictly greater than `after`, and respond with `Frame::MarkResult { offset }` or `Frame::Error` if no such marker exists.

- [ ] **Step 1: Locate the existing `Frame::ResolveMarker` handler.**

In `crates/cleat/src/session.rs`, search for `Frame::ResolveMarker`. The surrounding match arm shows how marker responses are sent back to clients. Read a few lines of context before proceeding.

- [ ] **Step 2: Add the handler arm.**

Next to the existing `ResolveMarker` handler, add:

```rust
                Frame::ResolveNextMarker { after } => {
                    let next = markers
                        .iter()
                        .filter(|(_, &offset)| offset > after)
                        .map(|(_, &offset)| offset)
                        .min();
                    let reply = match next {
                        Some(offset) => Frame::MarkResult { offset },
                        None => Frame::Error(format!("no marker after offset {after}")),
                    };
                    if let Err(err) = reply.write(&mut stream) {
                        eprintln!("write resolve-next response: {err}");
                    }
                }
```

The exact variable name for the markers map may differ; use the same name the `ResolveMarker` arm uses (probably `markers`).

- [ ] **Step 3: Build to verify the branch compiles.**

Run: `cargo build --locked`

Expected: clean build.

- [ ] **Step 4: Commit the daemon handler (tests arrive in Task 5).**

```bash
git add crates/cleat/src/session.rs
git commit -m "session: handle ResolveNextMarker frame (client method lands in next commit)"
```

The handler is untested at this point; a lifecycle test exercising it through a client method arrives in Task 5 Step 6. This keeps every commit compiling cleanly.

---

## Task 5: Service client — new methods

**Files:**
- Modify: `crates/cleat/src/server.rs`

**Goal:** Three new methods on `Service`:
1. `resolve_next_marker_after(id, after) -> Result<u64, String>` — sends `Frame::ResolveNextMarker` and parses response.
2. `capture_slice_raw(id, start, end) -> Result<(String, SliceOutcome), String>` — the full slicing entry point.
3. `capture_slice_text(id, start, end)` — same shape, returns VT-rendered text (though today's implementation just concatenates output, same as `capture_slice_raw`; the separation is forward-looking).

Also introduce the `StartBound`, `EndBound`, and `SliceOutcome` types.

- [ ] **Step 1: Write failing tests.**

The existing file `crates/cleat/tests/capture_render.rs` already has a test helper `setup_session_with_cast(root, id, events)` (at line 10) that writes a cast file with an arbitrary event list, and uses `SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()))` to construct the client. Reuse these. Add:

```rust
#[test]
fn capture_slice_text_returns_bytes_through_eof_with_start_at_zero() {
    use cleat::server::{EndBound, StartBound};

    let temp = tempfile::tempdir().unwrap();
    let events = vec![
        Event { time: Duration::from_millis(100), code: EventCode::Output, data: "hello ".into() },
        Event { time: Duration::from_millis(200), code: EventCode::Output, data: "world".into() },
    ];
    setup_session_with_cast(temp.path(), "sess", &events);
    let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));

    let (text, outcome) = service
        .capture_slice_text("sess", StartBound::Offset(0), EndBound::EndOfRecording)
        .expect("slice");
    assert_eq!(text, "hello world");
    assert!(outcome.hit_intended_end);
    assert_eq!(outcome.start_offset, 0);
    // end_offset equals the file size; don't hardcode it — compute from the fixture
    let file_size = std::fs::metadata(temp.path().join("sess").join(CAST_FILE_NAME)).unwrap().len();
    assert_eq!(outcome.end_offset, file_size);
}

#[test]
fn capture_slice_text_idle_fallback_to_eof_populates_fallback_reason() {
    use cleat::server::{EndBound, StartBound};

    let temp = tempfile::tempdir().unwrap();
    // Two close-together events — no 10-second gap exists.
    let events = vec![
        Event { time: Duration::from_millis(100), code: EventCode::Output, data: "a".into() },
        Event { time: Duration::from_millis(200), code: EventCode::Output, data: "b".into() },
    ];
    setup_session_with_cast(temp.path(), "sess", &events);
    let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));

    let (text, outcome) = service
        .capture_slice_text(
            "sess",
            StartBound::Offset(0),
            EndBound::IdleGap(Duration::from_secs(10)),
        )
        .expect("slice");
    assert_eq!(text, "ab");  // all events returned (EOF fallback)
    assert!(!outcome.hit_intended_end);
    assert_eq!(outcome.fallback_reason.as_deref(), Some("no 10s idle found"));
}
```

These exercise the orchestration layer (`capture_slice_inner`). Fine-grained idle-gap math is already unit-tested in Task 2's cast_reader tests.

- [ ] **Step 2: Run the tests — expect compile errors (types don't exist).**

- [ ] **Step 3: Define the public types.**

In `crates/cleat/src/server.rs`, near the top (after existing imports):

```rust
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartBound {
    Offset(u64),
    Marker(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum EndBound {
    Offset(u64),
    Marker(String),
    NextMarker,
    IdleGap(Duration),
    EndOfRecording,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceOutcome {
    pub start_offset: u64,
    pub end_offset: u64,
    pub hit_intended_end: bool,
    pub fallback_reason: Option<String>,
}
```

- [ ] **Step 4: Implement `resolve_next_marker_after`.**

Next to the existing `resolve_marker` method in `server.rs`:

```rust
    pub fn resolve_next_marker_after(&self, id: &str, after: u64) -> Result<u64, String> {
        if !self.layout.root().join(id).exists() {
            return Err(format!("missing session {id}"));
        }
        let socket_path = session_socket_path(self.layout.root(), id);
        let mut stream = connect_session_socket(&socket_path)?;
        Frame::ResolveNextMarker { after }
            .write(&mut stream)
            .map_err(|e| format!("write resolve-next: {e}"))?;
        match Frame::read(&mut stream).map_err(|e| format!("read resolve-next response: {e}"))? {
            Frame::MarkResult { offset } => Ok(offset),
            Frame::Error(msg) => Err(msg),
            other => Err(format!("unexpected resolve-next response: {other:?}")),
        }
    }
```

- [ ] **Step 5: Implement `capture_slice_raw` and `capture_slice_text`.**

Add to `server.rs`, next to the existing `capture_since_*` methods:

```rust
    pub fn capture_slice_raw(
        &self,
        id: &str,
        start: StartBound,
        end: EndBound,
    ) -> Result<(String, SliceOutcome), String> {
        self.capture_slice_inner(id, start, end)
    }

    pub fn capture_slice_text(
        &self,
        id: &str,
        start: StartBound,
        end: EndBound,
    ) -> Result<(String, SliceOutcome), String> {
        // Today the two produce identical output; separation is for future
        // VT-rendered transcripts. Same body.
        self.capture_slice_inner(id, start, end)
    }

    fn capture_slice_inner(
        &self,
        id: &str,
        start: StartBound,
        end: EndBound,
    ) -> Result<(String, SliceOutcome), String> {
        let cast_path = self.layout.root().join(id).join(crate::recording::CAST_FILE_NAME);
        if !cast_path.exists() {
            return Err(format!("no recording for session {id}"));
        }

        let start_offset = match start {
            StartBound::Offset(o) => o,
            StartBound::Marker(name) => self.resolve_marker(id, &name)?,
        };

        // Resolve end to either a concrete byte offset or a signal that we
        // need to scan for an idle gap.
        let (end_offset, hit_intended_end, fallback_reason) = match end {
            EndBound::EndOfRecording => {
                let file_size = std::fs::metadata(&cast_path)
                    .map_err(|e| format!("stat cast file: {e}"))?
                    .len();
                (file_size, true, None)
            }
            EndBound::Offset(o) => (o, true, None),
            EndBound::Marker(name) => {
                let o = self.resolve_marker(id, &name)?;
                if o < start_offset {
                    return Err(format!("marker '{name}' precedes start"));
                }
                (o, true, None)
            }
            EndBound::NextMarker => {
                match self.resolve_next_marker_after(id, start_offset) {
                    Ok(o) => (o, true, None),
                    Err(msg) if msg.contains("no marker") => {
                        let file_size = std::fs::metadata(&cast_path)
                            .map_err(|e| format!("stat cast file: {e}"))?
                            .len();
                        (file_size, false, Some("no marker after start".to_string()))
                    }
                    Err(msg) => return Err(msg),
                }
            }
            EndBound::IdleGap(duration) => {
                match crate::cast_reader::find_idle_gap_after(&cast_path, start_offset, duration)? {
                    Some(o) => (o, true, None),
                    None => {
                        let file_size = std::fs::metadata(&cast_path)
                            .map_err(|e| format!("stat cast file: {e}"))?
                            .len();
                        (
                            file_size,
                            false,
                            Some(format!("no {} idle found", humantime::format_duration(duration))),
                        )
                    }
                }
            }
        };

        let events = crate::cast_reader::read_output_between(&cast_path, start_offset, end_offset)?;
        let output: String = events.iter().map(|e| e.data.as_str()).collect();
        Ok((
            output,
            SliceOutcome {
                start_offset,
                end_offset,
                hit_intended_end,
                fallback_reason,
            },
        ))
    }
```

- [ ] **Step 6: Add lifecycle test exercising `resolve_next_marker_after` end-to-end.**

In `crates/cleat/tests/lifecycle.rs`, using the pattern of `detached_session_answers_da_queries`:

```rust
#[test]
fn resolve_next_marker_returns_minimum_offset_above() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, None, Some("sh -c 'stty raw; exec cat'".into()), true).expect("create");

    std::thread::sleep(Duration::from_millis(500));

    let off_a = service.mark("alpha").expect("mark a");
    service.send_keys("alpha", b"x").expect("send x");
    std::thread::sleep(Duration::from_millis(300));
    let off_b = service.mark("alpha").expect("mark b");
    service.send_keys("alpha", b"y").expect("send y");
    std::thread::sleep(Duration::from_millis(300));
    let off_c = service.mark("alpha").expect("mark c");

    // Next marker after A should be B (smallest offset > A).
    assert_eq!(service.resolve_next_marker_after("alpha", off_a).expect("resolve"), off_b);
    // Next after B should be C.
    assert_eq!(service.resolve_next_marker_after("alpha", off_b).expect("resolve"), off_c);
    // Next after C should be a not-found error.
    let err = service.resolve_next_marker_after("alpha", off_c).unwrap_err();
    assert!(err.contains("no marker"), "expected not-found error, got: {err}");
}
```

- [ ] **Step 7: Run targeted tests.**

Run:

```bash
cargo test -p cleat --test capture_render --locked
cargo test -p cleat --test lifecycle resolve_next_marker --locked
```

Expected: new tests pass.

- [ ] **Step 8: Commit.**

```bash
git add crates/cleat/src/server.rs crates/cleat/tests/capture_render.rs crates/cleat/tests/lifecycle.rs
git commit -m "server: add StartBound/EndBound/SliceOutcome + capture_slice_{raw,text} + resolve_next_marker_after"
```

---

## Task 6: Retire `capture_since_*` methods and migrate callers

**Files:**
- Modify: `crates/cleat/src/server.rs` (remove `capture_since_raw`, `capture_since_text`)
- Modify: `crates/cleat/src/cli.rs` (update Transcript dispatch to use `capture_slice_text` / `capture_slice_raw`)
- Modify: `crates/cleat/tests/capture_render.rs` (migrate 4 existing test callers)
- Modify: `crates/cleat/tests/lifecycle.rs` (migrate `detached_session_answers_da_queries` caller)

**Goal:** Single entry point for all cast-file slicing, per the spec's "retire rather than keep both forms" decision.

- [ ] **Step 1: Migrate `tests/capture_render.rs` callers.**

Find the four `service.capture_since_text(...)` and `service.capture_since_raw(...)` calls. Replace each with the equivalent `capture_slice_text` / `capture_slice_raw` call:

```rust
// Before:
let result = service.capture_since_text("sess", 0).unwrap();

// After:
let (result, _outcome) = service.capture_slice_text(
    "sess",
    StartBound::Offset(0),
    EndBound::EndOfRecording,
).unwrap();
```

Add `use cleat::server::{StartBound, EndBound};` (or equivalent, matching the existing import pattern) at the top of the file if needed.

- [ ] **Step 2: Migrate `tests/lifecycle.rs` caller.**

In the test `detached_session_answers_da_queries` (around line 458), replace:

```rust
let output = service.capture_since_raw("alpha", offset).expect("capture since");
```

with:

```rust
let (output, _outcome) = service
    .capture_slice_raw("alpha", StartBound::Offset(offset), EndBound::EndOfRecording)
    .expect("capture slice");
```

- [ ] **Step 3: Migrate `cli.rs` Transcript dispatch.**

In `crates/cleat/src/cli.rs` around line 380, replace the existing Transcript handler with one using the new methods. Full replacement will be done as part of Task 7 (adding end-bound flags) — for now, just switch the call target, keeping the existing flag set:

```rust
Command::Transcript { id, since, since_marker, raw } => {
    let start = match (since, since_marker) {
        (Some(o), None) => StartBound::Offset(o),
        (None, Some(name)) => StartBound::Marker(name),
        (None, None) => {
            return ExecResult::Err("transcript requires --since or --since-marker".to_string());
        }
        _ => unreachable!("clap conflicts_with prevents this"),
    };
    let result = if raw {
        service.capture_slice_raw(&id, start, EndBound::EndOfRecording)
    } else {
        service.capture_slice_text(&id, start, EndBound::EndOfRecording)
    };
    match result {
        Ok((s, _outcome)) => ExecResult::Ok(Some(s)),
        Err(e) => ExecResult::Err(e),
    }
}
```

Add imports at the top of `cli.rs` for `StartBound` and `EndBound`.

- [ ] **Step 4: Remove `capture_since_raw` and `capture_since_text` from `server.rs`.**

Delete lines around server.rs:160-180 (the two method definitions). Keep `resolve_marker` and all other methods.

- [ ] **Step 5: Build and run all tests.**

```bash
cargo build --locked
cargo test --workspace --locked
```

Expected: clean build, all tests pass. If any test fails with "method not found," it's a missed migration — grep for `capture_since_` to find the remaining caller.

- [ ] **Step 6: Commit.**

```bash
git add crates/cleat/src/server.rs crates/cleat/src/cli.rs crates/cleat/tests/capture_render.rs crates/cleat/tests/lifecycle.rs
git commit -m "server: retire capture_since_* in favor of capture_slice_*; migrate callers"
```

---

## Task 7: CLI — add end-bound flags and dispatch

**Files:**
- Modify: `crates/cleat/src/cli.rs`

**Goal:** Extend `Command::Transcript` with four new end-bound flags (mutually exclusive), wire dispatch to build `EndBound`, and emit the stderr fallback note when appropriate.

- [ ] **Step 1: Extend the `Transcript` struct.**

Find `Command::Transcript { ... }` in `cli.rs` (around line 103). Add these fields alongside the existing `since`, `since_marker`, `raw`:

```rust
        /// Byte offset in .cast file; slice ends at this position.
        #[arg(long, conflicts_with_all = ["until_marker", "until_next_marker", "until_idle"])]
        until: Option<u64>,
        /// Named marker to use as the end offset.
        #[arg(long, conflicts_with_all = ["until", "until_next_marker", "until_idle"])]
        until_marker: Option<String>,
        /// Slice until the chronologically-next marker after the start.
        #[arg(long, conflicts_with_all = ["until", "until_marker", "until_idle"])]
        until_next_marker: bool,
        /// Slice until the recording is idle for this duration (e.g., 500ms, 2s).
        #[arg(long, value_parser = crate::duration_parser::parse_humantime_or_seconds, conflicts_with_all = ["until", "until_marker", "until_next_marker"])]
        until_idle: Option<std::time::Duration>,
```

- [ ] **Step 2: Extend the dispatch.**

Replace the Transcript handler implemented in Task 6 with one that covers all end-bound variants:

```rust
Command::Transcript { id, since, since_marker, until, until_marker, until_next_marker, until_idle, raw } => {
    let start = match (since, since_marker) {
        (Some(o), None) => StartBound::Offset(o),
        (None, Some(name)) => StartBound::Marker(name),
        (None, None) => {
            return ExecResult::Err("transcript requires --since or --since-marker".to_string());
        }
        _ => unreachable!("clap conflicts_with prevents this"),
    };

    let end = match (until, until_marker, until_next_marker, until_idle) {
        (Some(o), None, false, None) => EndBound::Offset(o),
        (None, Some(name), false, None) => EndBound::Marker(name),
        (None, None, true, None) => EndBound::NextMarker,
        (None, None, false, Some(d)) => EndBound::IdleGap(d),
        (None, None, false, None) => EndBound::EndOfRecording,
        _ => unreachable!("clap conflicts_with prevents this"),
    };

    let result = if raw {
        service.capture_slice_raw(&id, start, end)
    } else {
        service.capture_slice_text(&id, start, end)
    };
    match result {
        Ok((s, outcome)) => {
            if !outcome.hit_intended_end {
                if let Some(reason) = &outcome.fallback_reason {
                    eprintln!("# bounded by EOF ({reason})");
                }
            }
            ExecResult::Ok(Some(s))
        }
        Err(e) => ExecResult::Err(e),
    }
}
```

- [ ] **Step 3: Build.**

Run: `cargo build --locked`

Expected: clean build. If clap complains about `conflicts_with_all` on boolean flag, use the `conflicts_with` array syntax with `#[arg(long, action = ArgAction::SetTrue, conflicts_with_all = [...])]` — check clap 4.x docs for the right form.

- [ ] **Step 4: Add CLI-level integration tests.**

Append to `crates/cleat/tests/lifecycle.rs`:

```rust
#[test]
fn transcript_between_two_named_markers_returns_exact_range() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, None, Some("sh -c 'stty raw; exec cat'".into()), true).expect("create");

    std::thread::sleep(Duration::from_millis(500));

    service.named_mark("alpha", "m1").expect("mark m1");
    service.send_keys("alpha", b"first").expect("send first");
    std::thread::sleep(Duration::from_millis(300));
    service.named_mark("alpha", "m2").expect("mark m2");
    service.send_keys("alpha", b"second").expect("send second");
    std::thread::sleep(Duration::from_millis(300));

    // Slice between m1 and m2.
    let cli = Cli::try_parse_from([
        "cleat", "transcript", "alpha", "--since-marker", "m1", "--until-marker", "m2",
    ]).expect("parse");
    let result = cli::execute(cli, &service).expect("execute");
    let output = result.expect("output");
    assert!(output.contains("first"), "expected 'first' in output, got: {output:?}");
    assert!(!output.contains("second"), "did not expect 'second', got: {output:?}");
}

#[test]
fn transcript_until_idle_terminates_at_quiet_period() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("alpha".into()), None, None, Some("sh -c 'stty raw; exec cat'".into()), true).expect("create");

    std::thread::sleep(Duration::from_millis(500));

    service.named_mark("alpha", "start").expect("mark");
    service.send_keys("alpha", b"burst").expect("send burst");
    std::thread::sleep(Duration::from_millis(1000)); // idle gap
    service.send_keys("alpha", b"after").expect("send after");
    std::thread::sleep(Duration::from_millis(300));

    let cli = Cli::try_parse_from([
        "cleat", "transcript", "alpha", "--since-marker", "start", "--until-idle", "500ms",
    ]).expect("parse");
    let result = cli::execute(cli, &service).expect("execute");
    let output = result.expect("output");
    assert!(output.contains("burst"), "expected 'burst' in output");
    assert!(!output.contains("after"), "idle gap should have terminated slice before 'after'");
}
```

Note: `service.named_mark(id, name)` is the confirmed API (`crates/cleat/src/server.rs:282`). Returns the offset placed.

- [ ] **Step 5: Run tests.**

```bash
cargo test -p cleat --test lifecycle transcript_ --locked
```

Expected: new tests pass.

- [ ] **Step 6: Full gates.**

```bash
cargo +nightly-2026-03-12 fmt --check
cargo build --locked
cargo build --features ghostty-vt --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --all-targets --features cleat/ghostty-vt --locked -- -D warnings
cargo test --workspace --locked
cargo test -p cleat --features ghostty-vt --locked
```

All green.

- [ ] **Step 7: Commit.**

```bash
git add crates/cleat/src/cli.rs crates/cleat/tests/lifecycle.rs
git commit -m "cli: add end-bound flags to transcript (--until, --until-marker, --until-next-marker, --until-idle)"
```

---

## Task 8: `wait --idle-time` humantime harmonisation

**Files:**
- Modify: `crates/cleat/src/cli.rs`

**Goal:** `wait --idle-time` accepts both the existing plain-float-seconds form and humantime-suffixed durations, backwards-compatibly. Uses the same `parse_humantime_or_seconds` from Task 1.

- [ ] **Step 1: Locate the `Wait` variant.**

In `cli.rs`, search for `Wait {`. The `--idle-time` flag currently has type `Option<f64>` or similar.

- [ ] **Step 2: Change the flag type and parser.**

Replace the `idle_time` field with:

```rust
        /// Wait until output settles for this duration (e.g., 500ms, 2s, or plain seconds).
        #[arg(long, value_parser = crate::duration_parser::parse_humantime_or_seconds)]
        idle_time: Option<std::time::Duration>,
```

Update the help-text-only content in the `Wait` command help (the trailing doc-string under `Usage:`) to reflect the new format acceptance if applicable.

- [ ] **Step 3: Update the dispatch to use the Duration.**

Find the `Command::Wait { .. }` arm. Where `idle_time` is converted to `WaitCondition::IdleTime(seconds)`, adapt the conversion:

```rust
// Before (likely):
conditions.push(WaitCondition::IdleTime(t));  // t: f64 seconds

// After:
conditions.push(WaitCondition::IdleTime(d.as_secs_f64()));  // d: Duration
```

If `WaitCondition::IdleTime` already takes `f64`, keep the f64 interface (don't touch the wait protocol) — convert on the CLI side at dispatch time.

- [ ] **Step 4: Add a test.**

In `crates/cleat/tests/lifecycle.rs` or wherever wait is tested, add:

```rust
#[test]
fn wait_idle_time_accepts_humantime_and_seconds() {
    // Both forms parse and dispatch equivalently.
    let humantime_form = Cli::try_parse_from(["cleat", "wait", "x", "--idle-time", "500ms"]).expect("humantime parse");
    let seconds_form = Cli::try_parse_from(["cleat", "wait", "x", "--idle-time", "0.5"]).expect("seconds parse");
    // ... assert the parsed command carries Duration::from_millis(500) in both cases.
}
```

The exact assertion depends on how the parsed `Cli` exposes the inner fields; match the existing pattern for wait tests in the same file.

- [ ] **Step 5: Run tests.**

```bash
cargo test -p cleat wait --locked
```

Expected: existing wait tests still pass, new test passes.

- [ ] **Step 6: Full gates.**

Same seven-command gate sweep as Task 7, Step 6.

- [ ] **Step 7: Commit.**

```bash
git add crates/cleat/src/cli.rs crates/cleat/tests/lifecycle.rs
git commit -m "wait: accept humantime durations on --idle-time (backwards-compatible)"
```

---

## Task 9: Final validation sweep

**Goal:** Confirm every gate CLAUDE.md specifies is green.

- [ ] **Step 1: fmt.**

Run: `cargo +nightly-2026-03-12 fmt --check`

Expected: no output.

- [ ] **Step 2: Build, feature off.**

Run: `cargo build --locked`

Expected: clean.

- [ ] **Step 3: Build, feature on.**

Run: `cargo build --features ghostty-vt --locked`

Expected: clean.

- [ ] **Step 4: Clippy, feature off.**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

Expected: clean.

- [ ] **Step 5: Clippy, feature on.**

Run: `cargo clippy --workspace --all-targets --features cleat/ghostty-vt --locked -- -D warnings`

Expected: clean.

- [ ] **Step 6: Tests, feature off.**

Run: `cargo test --workspace --locked`

Expected: all pass.

- [ ] **Step 7: Tests, feature on.**

Run: `cargo test -p cleat --features ghostty-vt --locked`

Expected: all pass.

- [ ] **Step 8: Release build, feature on.**

Run: `cargo build -p cleat --features ghostty-vt --locked --release`

Expected: clean.

- [ ] **Step 9: Manual smoke (optional but valuable).**

```bash
./target/debug/cleat launch --record demo --cmd bash
./target/debug/cleat send demo 'echo hello' --mark-before m1
./target/debug/cleat send demo 'echo world'
sleep 1
./target/debug/cleat mark demo --name m2
./target/debug/cleat transcript demo --since-marker m1 --until-marker m2
./target/debug/cleat kill demo
```

Expect the transcript output to contain `hello` (the first echo) and the `echo world` command, but terminate at m2.

No commit for this task — it's the merge gate.
