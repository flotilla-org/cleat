# Named Markers Implementation Plan

**Goal:** Add named markers to cleat — label a byte offset in the recording with a name, resolve it later for incremental capture, emit it as an asciicast `"m"` event, and show it in inspect output.

**Architecture:** Extend the existing `Mark` protocol frame to carry an optional name. The daemon stores a `HashMap<String, u64>` of markers alongside the recorder. A new `ResolveMarker` frame lets clients look up a marker by name. The `Capture` CLI gains `--since-marker <name>`. Markers are emitted as standard asciicast `"m"` events in the .cast stream and included in `InspectResult`.

**Tech Stack:** Rust, existing protocol/CLI/recording infrastructure.

---

## File Structure

| File | Action | Responsibility |
|------|--------|----------------|
| `crates/cleat/src/protocol.rs` | Modify | Extend `Frame::Mark` to carry optional name. Add `Frame::ResolveMarker`/`Frame::ResolveMarkerResult`. Add `markers` field to `RecordingInspect`. |
| `crates/cleat/src/session.rs` | Modify | Daemon stores `HashMap<String, u64>` of markers. Handle named marks (store + emit `"m"` event). Handle `ResolveMarker`. Populate markers in inspect. |
| `crates/cleat/src/server.rs` | Modify | Add `named_mark()` and `resolve_marker()` methods to `SessionService`. |
| `crates/cleat/src/cli.rs` | Modify | Add optional `name` arg to `Mark`. Add `--since-marker` to `Capture`. |
| `crates/cleat/tests/cli.rs` | Modify | Parse tests for new mark/capture flags. |

---

## Task 1: Extend Mark frame with optional name

**Files:**
- Modify: `crates/cleat/src/protocol.rs`

The current `Frame::Mark` is an empty frame. Change it to carry an optional name. Also add `ResolveMarker` and `ResolveMarkerResult` frames. Add `markers` to `RecordingInspect`.

- [ ] **Step 1: Write failing tests — named mark round-trip**

Append to `protocol.rs` `mod tests`:

```rust
#[test]
fn named_mark_round_trip() {
    let frame = Frame::Mark { name: Some("test-start".to_string()) };
    let mut bytes = Vec::new();
    frame.write(&mut bytes).expect("write");
    let decoded = Frame::read(&mut bytes.as_slice()).expect("read");
    assert_eq!(decoded, Frame::Mark { name: Some("test-start".to_string()) });
}

#[test]
fn unnamed_mark_round_trip() {
    let frame = Frame::Mark { name: None };
    let mut bytes = Vec::new();
    frame.write(&mut bytes).expect("write");
    let decoded = Frame::read(&mut bytes.as_slice()).expect("read");
    assert_eq!(decoded, Frame::Mark { name: None });
}

#[test]
fn resolve_marker_round_trip() {
    let frame = Frame::ResolveMarker { name: "checkpoint".to_string() };
    let mut bytes = Vec::new();
    frame.write(&mut bytes).expect("write");
    let decoded = Frame::read(&mut bytes.as_slice()).expect("read");
    assert_eq!(decoded, Frame::ResolveMarker { name: "checkpoint".to_string() });
}
```

- [ ] **Step 2: Update Frame enum and encode/decode**

Change `Frame::Mark` from a unit variant to `Mark { name: Option<String> }`.

Add new variants:
```rust
ResolveMarker { name: String },
// ResolveMarkerResult reuses MarkResult { offset } — same response format
```

Add new tag constants:
```rust
const TAG_RESOLVE_MARKER: u8 = 17;
```

Update encode:
```rust
Frame::Mark { ref name } => {
    let payload = match name {
        Some(n) => n.as_bytes().to_vec(),
        None => vec![],
    };
    (TAG_MARK, payload)
}
Frame::ResolveMarker { ref name } => (TAG_RESOLVE_MARKER, name.as_bytes().to_vec()),
```

Update decode:
```rust
TAG_MARK => {
    let name = if payload.is_empty() {
        None
    } else {
        Some(String::from_utf8(payload).map_err(|e| Error::new(ErrorKind::InvalidData, format!("invalid mark name: {e}")))?)
    };
    Ok(Frame::Mark { name })
}
TAG_RESOLVE_MARKER => {
    let name = String::from_utf8(payload).map_err(|e| Error::new(ErrorKind::InvalidData, format!("invalid marker name: {e}")))?;
    Ok(Frame::ResolveMarker { name })
}
```

Add `markers` to `RecordingInspect`:
```rust
pub struct RecordingInspect {
    pub active: bool,
    pub bytes_written: u64,
    pub markers: std::collections::HashMap<String, u64>,
}
```

- [ ] **Step 3: Fix all compilation errors**

The `Frame::Mark` change from unit variant to struct variant will break:
- `session.rs` Mark handler — update `Ok(Frame::Mark)` to `Ok(Frame::Mark { name })`
- `server.rs` mark() — update `Frame::Mark.write(...)` to `Frame::Mark { name: None }.write(...)`
- `cli.rs` execute — no change needed yet (name arg added in Task 3)
- `protocol.rs` tests — update existing `mark_round_trip` test
- `build_inspect_result` in `session.rs` — add `markers: HashMap::new()` (placeholder, updated in Task 2)

- [ ] **Step 4: Run tests**

Run: `cargo test --workspace --locked`
Expected: all pass.

- [ ] **Step 5: Run full checks**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings && cargo +nightly-2026-03-12 fmt --check`

- [ ] **Step 6: Commit**

```bash
git add crates/cleat/src/protocol.rs crates/cleat/src/session.rs crates/cleat/src/server.rs
git commit -m "feat(markers): extend Mark frame with optional name, add ResolveMarker frame"
```

---

## Task 2: Daemon marker storage and "m" event emission

**Files:**
- Modify: `crates/cleat/src/session.rs`

The daemon stores markers in a `HashMap<String, u64>`, emits `"m"` events when named marks are created, handles `ResolveMarker` requests, and populates markers in inspect output.

- [ ] **Step 1: Add marker storage to daemon**

In `run_session_daemon`, after the `recorder` variable, add:

```rust
let mut markers: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
```

- [ ] **Step 2: Update Mark handler for named marks**

Replace the existing `Ok(Frame::Mark { name })` handler:

```rust
Ok(Frame::Mark { name }) => {
    if let Some(ref mut rec) = recorder {
        rec.flush();
        let offset = rec.bytes_written();
        if let Some(marker_name) = name {
            markers.insert(marker_name.clone(), offset);
            // Emit asciicast "m" event
            rec.event(
                crate::asciicast::EventCode::Marker,
                &marker_name,
                epoch.elapsed(),
            );
        }
        let _ = Frame::MarkResult { offset }.write(&mut stream);
    } else {
        let _ = Frame::Error("recording not active".to_string()).write(&mut stream);
    }
}
```

- [ ] **Step 3: Add ResolveMarker handler**

In the listener frame dispatch, add:

```rust
Ok(Frame::ResolveMarker { name }) => {
    if let Some(offset) = markers.get(&name) {
        let _ = Frame::MarkResult { offset: *offset }.write(&mut stream);
    } else {
        let _ = Frame::Error(format!("marker not found: {name}")).write(&mut stream);
    }
}
```

- [ ] **Step 4: Update build_inspect_result to include markers**

Pass the `markers` map to `build_inspect_result` and populate the field:

Update the function signature to accept `&HashMap<String, u64>`. Set `recording.markers` to a clone of the map.

- [ ] **Step 5: Run tests**

Run: `cargo test --workspace --locked`

- [ ] **Step 6: Run full checks and commit**

```bash
cargo clippy --workspace --all-targets --locked -- -D warnings && cargo +nightly-2026-03-12 fmt --check
git add crates/cleat/src/session.rs
git commit -m "feat(markers): daemon stores named markers, emits 'm' events, resolves lookups"
```

---

## Task 3: CLI and service methods for named markers

**Files:**
- Modify: `crates/cleat/src/cli.rs`
- Modify: `crates/cleat/src/server.rs`
- Modify: `crates/cleat/tests/cli.rs`

Add optional `name` argument to `Mark` command, `--since-marker` to `Capture`, and service methods for named mark and resolve.

- [ ] **Step 1: Write failing CLI parse tests**

Append to `crates/cleat/tests/cli.rs`:

```rust
#[test]
fn mark_with_name_parses() {
    let cli = Cli::try_parse_from(["cleat", "mark", "sess", "checkpoint"]).expect("parse");
    assert_eq!(cli.command, Command::Mark { id: "sess".into(), name: Some("checkpoint".into()) });
}

#[test]
fn mark_without_name_still_works() {
    let cli = Cli::try_parse_from(["cleat", "mark", "sess"]).expect("parse");
    assert_eq!(cli.command, Command::Mark { id: "sess".into(), name: None });
}

#[test]
fn capture_with_since_marker_parses() {
    let cli = Cli::try_parse_from(["cleat", "capture", "sess", "--since-marker", "checkpoint"]).expect("parse");
    assert_eq!(cli.command, Command::Capture { id: "sess".into(), since: None, since_marker: Some("checkpoint".into()), raw: false });
}
```

- [ ] **Step 2: Update Mark command**

In `cli.rs`, change the `Mark` variant:

```rust
Mark {
    id: String,
    /// Optional marker name — stores the current offset with this label
    name: Option<String>,
},
```

- [ ] **Step 3: Update Capture command**

Add `since_marker` field:

```rust
Capture {
    id: String,
    #[arg(long)]
    since: Option<u64>,
    #[arg(long)]
    since_marker: Option<String>,
    #[arg(long)]
    raw: bool,
},
```

- [ ] **Step 4: Update existing CLI parse tests**

The existing `mark_command_parses_session_id` test needs `name: None` added. The existing capture tests need `since_marker: None` added.

- [ ] **Step 5: Add service methods**

In `server.rs`:

```rust
pub fn named_mark(&self, id: &str, name: &str) -> Result<u64, String> {
    if !self.layout.root().join(id).exists() {
        return Err(format!("missing session {id}"));
    }
    let socket_path = session_socket_path(self.layout.root(), id);
    let mut stream = connect_session_socket(&socket_path)?;
    Frame::Mark { name: Some(name.to_string()) }.write(&mut stream).map_err(|e| format!("write mark: {e}"))?;
    match Frame::read(&mut stream).map_err(|e| format!("read mark response: {e}"))? {
        Frame::MarkResult { offset } => Ok(offset),
        Frame::Error(msg) => Err(msg),
        other => Err(format!("unexpected mark response: {other:?}")),
    }
}

pub fn resolve_marker(&self, id: &str, name: &str) -> Result<u64, String> {
    if !self.layout.root().join(id).exists() {
        return Err(format!("missing session {id}"));
    }
    let socket_path = session_socket_path(self.layout.root(), id);
    let mut stream = connect_session_socket(&socket_path)?;
    Frame::ResolveMarker { name: name.to_string() }.write(&mut stream).map_err(|e| format!("write resolve: {e}"))?;
    match Frame::read(&mut stream).map_err(|e| format!("read resolve response: {e}"))? {
        Frame::MarkResult { offset } => Ok(offset),
        Frame::Error(msg) => Err(msg),
        other => Err(format!("unexpected resolve response: {other:?}")),
    }
}
```

Also update existing `mark()` to send the name:
```rust
pub fn mark(&self, id: &str) -> Result<u64, String> {
    // ... same as before but with Frame::Mark { name: None }
}
```

- [ ] **Step 6: Update execute handlers**

Mark handler:
```rust
Command::Mark { id, name } => {
    let offset = match name {
        Some(ref n) => service.named_mark(&id, n)?,
        None => service.mark(&id)?,
    };
    Ok(Some(offset.to_string()))
}
```

Capture handler — add `--since-marker` resolution:
```rust
Command::Capture { id, since, since_marker, raw } => {
    if raw && since.is_none() && since_marker.is_none() {
        return Err("--raw requires --since or --since-marker".to_string());
    }
    let offset = match (since, since_marker) {
        (Some(_), Some(_)) => return Err("--since and --since-marker are mutually exclusive".to_string()),
        (Some(o), None) => Some(o),
        (None, Some(ref name)) => Some(service.resolve_marker(&id, name)?),
        (None, None) => None,
    };
    match offset {
        Some(o) => {
            if raw {
                service.capture_since_raw(&id, o).map(Some)
            } else {
                service.capture_since_text(&id, o).map(Some)
            }
        }
        None => service.capture(&id).map(Some),
    }
}
```

- [ ] **Step 7: Update inspect human formatting**

In `format_inspect_human`, add markers display:
```rust
if !result.recording.markers.is_empty() {
    let markers_str = result.recording.markers.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(", ");
    table.add_row(vec!["markers", &markers_str]);
}
```

- [ ] **Step 8: Run tests**

Run: `cargo test --workspace --locked`

- [ ] **Step 9: Run full checks and commit**

```bash
cargo clippy --workspace --all-targets --locked -- -D warnings && cargo +nightly-2026-03-12 fmt --check
git add crates/cleat/src/cli.rs crates/cleat/src/server.rs crates/cleat/tests/cli.rs
git commit -m "feat(markers): add named mark CLI, --since-marker capture, resolve service methods"
```
