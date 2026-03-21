# Cleat Session Introspection and Output Recording Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add structured session introspection (`cleat inspect`), process-aware signal delivery (`cleat signal`), and opt-in output recording to cleat, establishing the agent control plane foundation.

**Architecture:** Three new CLI commands route through `SessionService` to the session daemon via the existing Unix socket protocol. `inspect` and `signal` are stateless request/response operations. Recording adds an optional append-only output log that the daemon tees PTY output into, activated via CLI flags, env var, or runtime control frame.

**Tech Stack:** Rust, clap (CLI), serde_json (inspect serialization), nix (tcgetpgrp, signal delivery), libc (killpg)

**Spec:** `docs/superpowers/specs/2026-03-20-cleat-control-plane-design.md`
**GitHub issue:** flotilla-org/cleat#5

---

## File Structure

### New files

| File | Responsibility |
|------|---------------|
| `crates/cleat/src/recording.rs` | `OutputRecorder` struct: append-only log writer, byte tracking |
| `crates/cleat/tests/recording.rs` | Unit tests for `OutputRecorder` |

### Modified files

| File | Changes |
|------|---------|
| `crates/cleat/src/protocol.rs` | `InspectResult` types, `SignalTarget` enum, 4 new `Frame` variants + tags |
| `crates/cleat/src/cli.rs` | `Inspect`, `Signal`, `Record` CLI subcommands + execute routing |
| `crates/cleat/src/server.rs` | `inspect()`, `signal()`, `record()` service methods |
| `crates/cleat/src/session.rs` | Daemon handlers for Inspect, Signal, RecordControl frames; recording integration in PTY output path |
| `crates/cleat/src/runtime.rs` | `record` field on `SessionMetadata` |
| `crates/cleat/src/lib.rs` | `pub mod recording;` declaration |
| `crates/cleat/Cargo.toml` | Add `comfy-table` dependency for human-readable inspect output |
| `crates/cleat/tests/cli.rs` | CLI parsing tests for new commands |
| `crates/cleat/tests/lifecycle.rs` | Integration tests for inspect, signal, recording |

---

### Task 1: Inspect data model and protocol frames

**Files:**
- Modify: `crates/cleat/src/protocol.rs`

- [ ] **Step 1: Write protocol round-trip test for InspectResult frame**

Add to the existing `mod tests` in `protocol.rs`:

```rust
#[test]
fn inspect_result_round_trip_preserves_json_payload() {
    let json = br#"{"session":{"id":"test"}}"#.to_vec();
    let frame = Frame::InspectResult(json.clone());
    let mut bytes = Vec::new();
    frame.write(&mut bytes).expect("write frame");
    let decoded = Frame::read(&mut bytes.as_slice()).expect("read frame");
    assert_eq!(decoded, Frame::InspectResult(json));
}

#[test]
fn inspect_round_trip_is_empty() {
    let frame = Frame::Inspect;
    let mut bytes = Vec::new();
    frame.write(&mut bytes).expect("write frame");
    let decoded = Frame::read(&mut bytes.as_slice()).expect("read frame");
    assert_eq!(decoded, Frame::Inspect);
}

#[test]
fn signal_round_trip_preserves_target_and_signal() {
    let frame = Frame::Signal { signal: libc::SIGINT, target: SignalTarget::Foreground };
    let mut bytes = Vec::new();
    frame.write(&mut bytes).expect("write frame");
    let decoded = Frame::read(&mut bytes.as_slice()).expect("read frame");
    assert_eq!(decoded, frame);
}

#[test]
fn record_control_round_trip() {
    let frame = Frame::RecordControl { enable: true };
    let mut bytes = Vec::new();
    frame.write(&mut bytes).expect("write frame");
    let decoded = Frame::read(&mut bytes.as_slice()).expect("read frame");
    assert_eq!(decoded, frame);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p cleat --locked --lib`
Expected: compilation errors — `Frame::Inspect`, `Frame::InspectResult`, `Frame::Signal`, `Frame::RecordControl`, `SignalTarget` don't exist yet.

- [ ] **Step 3: Add InspectResult types, SignalTarget, and new Frame variants**

Add the `InspectResult` types and `SignalTarget` to `protocol.rs` (above the `Frame` enum):

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectResult {
    pub session: SessionInspect,
    pub terminal: TerminalInspect,
    pub process: ProcessInspect,
    pub attachments: Vec<AttachmentInspect>,
    pub recording: RecordingInspect,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInspect {
    pub id: String,
    pub name: Option<String>,
    pub state: String,
    pub vt_engine: String,
    pub cwd: Option<PathBuf>,
    pub cmd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalInspect {
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessInspect {
    pub leader_pid: u32,
    pub foreground_pgid: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentInspect {
    pub role: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingInspect {
    pub active: bool,
    pub bytes_written: u64,
    pub cursor: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalTarget {
    Foreground = 0,
    Leader = 1,
    Tree = 2,
}
```

Add tag constants:

```rust
const TAG_INSPECT: u8 = 11;
const TAG_INSPECT_RESULT: u8 = 12;
const TAG_SIGNAL: u8 = 13;
const TAG_RECORD_CONTROL: u8 = 14;
```

Add variants to the `Frame` enum:

```rust
Inspect,
InspectResult(Vec<u8>),
Signal { signal: i32, target: SignalTarget },
RecordControl { enable: bool },
```

Add encode arms in `Frame::encode()`:

```rust
Frame::Inspect => (TAG_INSPECT, vec![]),
Frame::InspectResult(bytes) => (TAG_INSPECT_RESULT, bytes.clone()),
Frame::Signal { signal, target } => {
    let mut payload = Vec::with_capacity(5);
    payload.extend_from_slice(&signal.to_le_bytes());
    payload.push(*target as u8);
    (TAG_SIGNAL, payload)
}
Frame::RecordControl { enable } => (TAG_RECORD_CONTROL, vec![if *enable { 1 } else { 0 }]),
```

Add decode arms in `Frame::decode()`:

```rust
TAG_INSPECT => Ok(Frame::Inspect),
TAG_INSPECT_RESULT => Ok(Frame::InspectResult(payload)),
TAG_SIGNAL => {
    if payload.len() != 5 {
        return Err(Error::new(ErrorKind::InvalidData, "invalid signal frame"));
    }
    let signal = i32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let target = match payload[4] {
        0 => SignalTarget::Foreground,
        1 => SignalTarget::Leader,
        2 => SignalTarget::Tree,
        _ => return Err(Error::new(ErrorKind::InvalidData, "invalid signal target")),
    };
    Ok(Frame::Signal { signal, target })
}
TAG_RECORD_CONTROL => {
    if payload.len() != 1 {
        return Err(Error::new(ErrorKind::InvalidData, "invalid record control frame"));
    }
    Ok(Frame::RecordControl { enable: payload[0] != 0 })
}
```

Add `use` for `libc` in the test module for the `SIGINT` constant.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p cleat --locked --lib`
Expected: all protocol tests pass including the 4 new ones.

- [ ] **Step 5: Commit**

```bash
git add crates/cleat/src/protocol.rs
git commit -m "feat(cleat): add InspectResult types and protocol frames for inspect, signal, record-control"
```

---

### Task 2: Inspect and Signal CLI commands, service methods, and daemon handlers

**Files:**
- Modify: `crates/cleat/src/cli.rs`
- Modify: `crates/cleat/src/server.rs`
- Modify: `crates/cleat/src/session.rs`
- Modify: `crates/cleat/Cargo.toml`
- Modify: `crates/cleat/tests/cli.rs`
- Modify: `crates/cleat/tests/lifecycle.rs`

- [ ] **Step 1: Write CLI parsing tests for inspect and signal**

Add to `crates/cleat/tests/cli.rs`:

```rust
#[test]
fn inspect_parses_session_id() {
    let cli = Cli::try_parse_from(["cleat", "inspect", "alpha"]).expect("parse inspect");
    assert!(matches!(cli.command, Command::Inspect { ref id, json: false } if id == "alpha"));
}

#[test]
fn inspect_json_flag() {
    let cli = Cli::try_parse_from(["cleat", "inspect", "alpha", "--json"]).expect("parse inspect --json");
    assert!(matches!(cli.command, Command::Inspect { json: true, .. }));
}

#[test]
fn signal_parses_session_and_signal_name() {
    let cli = Cli::try_parse_from(["cleat", "signal", "alpha", "INT"]).expect("parse signal");
    assert!(matches!(cli.command, Command::Signal { ref id, ref signal, ref target }
        if id == "alpha" && signal == "INT" && target == "foreground"));
}

#[test]
fn signal_with_target() {
    let cli = Cli::try_parse_from(["cleat", "signal", "alpha", "TERM", "--target", "leader"]).expect("parse signal --target");
    assert!(matches!(cli.command, Command::Signal { ref target, .. } if target == "leader"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p cleat --locked --test cli`
Expected: compilation errors — `Command::Inspect` and `Command::Signal` don't exist yet.

- [ ] **Step 3: Add CLI subcommands**

Add to the `Command` enum in `crates/cleat/src/cli.rs`:

```rust
Inspect {
    id: String,
    #[arg(long)]
    json: bool,
},
Signal {
    id: String,
    signal: String,
    #[arg(long, default_value = "foreground")]
    target: String,
},
```

Add execute routing in the `execute` function:

```rust
Command::Inspect { id, json } => {
    let result = service.inspect(&id)?;
    if json {
        serde_json::to_string_pretty(&result).map(Some).map_err(|err| format!("serialize inspect result: {err}"))
    } else {
        Ok(Some(format_inspect_human(&result)))
    }
}
Command::Signal { id, signal, target } => {
    let sig = parse_signal_name(&signal)?;
    let tgt = parse_signal_target(&target)?;
    service.signal(&id, sig, tgt)?;
    Ok(None)
}
```

Add helper functions in `cli.rs`:

```rust
fn format_inspect_human(result: &crate::protocol::InspectResult) -> String {
    use comfy_table::{Table, presets::NOTHING};

    let mut table = Table::new();
    table.load_preset(NOTHING);

    table.add_row(vec!["session", &result.session.id]);
    table.add_row(vec!["state", &result.session.state]);
    table.add_row(vec!["terminal", &format!("{}x{}", result.terminal.cols, result.terminal.rows)]);
    table.add_row(vec!["leader_pid", &result.process.leader_pid.to_string()]);
    if let Some(fg) = result.process.foreground_pgid {
        table.add_row(vec!["fg_pgid", &fg.to_string()]);
    }
    table.add_row(vec!["recording", if result.recording.active { "active" } else { "off" }]);

    table.to_string()
}

fn parse_signal_name(name: &str) -> Result<i32, String> {
    match name.to_uppercase().trim_start_matches("SIG").as_ref() {
        "INT" => Ok(libc::SIGINT),
        "TERM" => Ok(libc::SIGTERM),
        "HUP" => Ok(libc::SIGHUP),
        "KILL" => Ok(libc::SIGKILL),
        "USR1" => Ok(libc::SIGUSR1),
        "USR2" => Ok(libc::SIGUSR2),
        "STOP" => Ok(libc::SIGSTOP),
        "CONT" => Ok(libc::SIGCONT),
        other => Err(format!("unknown signal: {other}")),
    }
}

fn parse_signal_target(target: &str) -> Result<crate::protocol::SignalTarget, String> {
    match target {
        "foreground" => Ok(crate::protocol::SignalTarget::Foreground),
        "leader" => Ok(crate::protocol::SignalTarget::Leader),
        "tree" => Ok(crate::protocol::SignalTarget::Tree),
        other => Err(format!("unknown signal target: {other}")),
    }
}
```

- [ ] **Step 4: Run CLI parsing tests to verify they pass**

Run: `cargo test -p cleat --locked --test cli`
Expected: PASS (the 4 new tests + existing tests).

- [ ] **Step 5: Add service methods**

Add to `SessionService` in `crates/cleat/src/server.rs`:

```rust
pub fn inspect(&self, id: &str) -> Result<crate::protocol::InspectResult, String> {
    if !self.layout.root().join(id).join("meta.json").exists() {
        return Err(format!("missing session {id}"));
    }
    let socket_path = session_socket_path(self.layout.root(), id);
    let mut stream =
        std::os::unix::net::UnixStream::connect(&socket_path).map_err(|err| format!("connect {}: {err}", socket_path.display()))?;
    Frame::Inspect.write(&mut stream).map_err(|err| format!("write inspect request: {err}"))?;
    match Frame::read(&mut stream).map_err(|err| format!("read inspect response: {err}"))? {
        Frame::InspectResult(json) => {
            serde_json::from_slice(&json).map_err(|err| format!("parse inspect response: {err}"))
        }
        Frame::Error(message) => Err(message),
        other => Err(format!("unexpected inspect response: {other:?}")),
    }
}

pub fn signal(&self, id: &str, signal: i32, target: crate::protocol::SignalTarget) -> Result<(), String> {
    if !self.layout.root().join(id).join("meta.json").exists() {
        return Err(format!("missing session {id}"));
    }
    let socket_path = session_socket_path(self.layout.root(), id);
    let mut stream =
        std::os::unix::net::UnixStream::connect(&socket_path).map_err(|err| format!("connect {}: {err}", socket_path.display()))?;
    Frame::Signal { signal, target }.write(&mut stream).map_err(|err| format!("write signal request: {err}"))?;
    match Frame::read(&mut stream).map_err(|err| format!("read signal response: {err}"))? {
        Frame::Ack => Ok(()),
        Frame::Error(message) => Err(message),
        other => Err(format!("unexpected signal response: {other:?}")),
    }
}
```

- [ ] **Step 6: Add daemon handlers in session.rs**

In `run_session_daemon`, inside the `match Frame::read(&mut stream)` block for new connections (the `listener_readable` section, after the `Ok(Frame::SendKeys(bytes))` arm), add:

```rust
Ok(Frame::Inspect) => {
    let result = build_inspect_result(&session, vt_engine.as_ref(), &active_client, &pty_child, &recorder);
    match serde_json::to_vec(&result) {
        Ok(json) => { let _ = Frame::InspectResult(json).write(&mut stream); }
        Err(err) => { let _ = Frame::Error(format!("serialize inspect: {err}")).write(&mut stream); }
    }
}
Ok(Frame::Signal { signal, target }) => {
    match dispatch_signal(&pty_child, signal, target) {
        Ok(()) => { let _ = Frame::Ack.write(&mut stream); }
        Err(err) => { let _ = Frame::Error(err).write(&mut stream); }
    }
}
```

Add a `recorder` placeholder variable initialized to `None` near `active_client`. Use `Option<()>` as a placeholder until the recording module exists (Task 3):

```rust
let mut recorder: Option<()> = None;
```

The type will be changed to `Option<crate::recording::OutputRecorder>` in Task 4 after the recording module is created in Task 3.

Add the helper functions in `session.rs`:

```rust
fn build_inspect_result(
    session: &SessionMetadata,
    vt_engine: &dyn VtEngine,
    active_client: &Option<ActiveClient>,
    pty_child: &PtyChild,
    _recorder: &Option<()>,
) -> crate::protocol::InspectResult {
    // Note: recorder parameter is a placeholder (Option<()>) until Task 4 changes it to Option<OutputRecorder>.
    let (cols, rows) = vt_engine.size();
    let foreground_pgid = nix::unistd::tcgetpgrp(unsafe { std::os::fd::BorrowedFd::borrow_raw(pty_child.master_fd) })
        .ok()
        .map(|pid| pid.as_raw() as u32);

    crate::protocol::InspectResult {
        session: crate::protocol::SessionInspect {
            id: session.id.clone(),
            name: session.name.clone(),
            state: "running".to_string(),
            vt_engine: session.vt_engine.as_str().to_string(),
            cwd: session.cwd.clone(),
            cmd: session.cmd.clone(),
        },
        terminal: crate::protocol::TerminalInspect { rows, cols },
        process: crate::protocol::ProcessInspect {
            leader_pid: pty_child.pid.as_raw() as u32,
            foreground_pgid,
        },
        attachments: if active_client.is_some() {
            vec![crate::protocol::AttachmentInspect { role: "controller".to_string() }]
        } else {
            vec![]
        },
        recording: crate::protocol::RecordingInspect {
            active: false, // Placeholder until Task 4 wires up the real recorder
            bytes_written: 0,
            cursor: 0,
        },
    }
}

fn dispatch_signal(pty_child: &PtyChild, signal: i32, target: crate::protocol::SignalTarget) -> Result<(), String> {
    let result = match target {
        crate::protocol::SignalTarget::Foreground => {
            let fg_pgid = nix::unistd::tcgetpgrp(unsafe { std::os::fd::BorrowedFd::borrow_raw(pty_child.master_fd) })
                .map_err(|err| format!("tcgetpgrp: {err}"))?;
            // SAFETY: fg_pgid is a valid process group from tcgetpgrp on our PTY.
            unsafe { libc::killpg(fg_pgid.as_raw(), signal) }
        }
        crate::protocol::SignalTarget::Leader => {
            // SAFETY: pty_child.pid is the leader process we spawned.
            unsafe { libc::kill(pty_child.pid.as_raw(), signal) }
        }
        crate::protocol::SignalTarget::Tree => {
            return Err("tree signal target is not yet implemented".to_string());
        }
    };
    if result == 0 {
        Ok(())
    } else {
        Err(format!("signal delivery failed: {}", std::io::Error::last_os_error()))
    }
}
```

Add `comfy-table` to `crates/cleat/Cargo.toml` dependencies (matching the version used in `flotilla-tui`):

```toml
comfy-table = "7.2.2"
```

Note: `dispatch_signal` uses `libc::killpg` and `libc::kill` directly (libc is already a dependency), and `tcgetpgrp` comes from `nix::unistd` (covered by the existing `"process"` feature).

- [ ] **Step 7: Write lifecycle test for inspect**

Add to `crates/cleat/tests/lifecycle.rs`:

```rust
#[test]
fn inspect_returns_structured_session_state() {
    let _lock = env_lock().lock().expect("env lock");
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let info = service.create(Some("alpha".into()), None, None, Some("bash".into())).expect("create session");

    // Wait for daemon to be ready
    let socket_path = session_socket_path(temp.path(), &info.id);
    wait_for_socket(&socket_path);

    let result = service.inspect(&info.id).expect("inspect session");

    assert_eq!(result.session.id, "alpha");
    assert_eq!(result.session.state, "running");
    assert!(result.process.leader_pid > 0);
    assert!(result.process.foreground_pgid.is_some());
    assert_eq!(result.terminal.cols, 80);
    assert_eq!(result.terminal.rows, 24);
    assert!(!result.recording.active);

    service.kill(&info.id).expect("kill session");
}
```

You'll need to add a `wait_for_socket` helper to the test file if it doesn't already exist (check first — it may be importable from `cleat::session`):

```rust
fn wait_for_socket(path: &std::path::Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for socket {}", path.display());
}
```

- [ ] **Step 8: Write lifecycle test for signal**

Add to `crates/cleat/tests/lifecycle.rs`:

```rust
#[test]
fn signal_term_to_leader_terminates_session() {
    let _lock = env_lock().lock().expect("env lock");
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let info = service.create(Some("beta".into()), None, None, Some("sleep 60".into())).expect("create session");

    let socket_path = session_socket_path(temp.path(), &info.id);
    wait_for_socket(&socket_path);

    // Verify session is alive
    let result = service.inspect(&info.id).expect("inspect before signal");
    assert!(result.process.leader_pid > 0);

    // Signal TERM to leader (the shell running sleep)
    service.signal(&info.id, libc::SIGTERM, cleat::protocol::SignalTarget::Leader).expect("signal session");

    // Session should terminate — daemon exits when child dies. Poll with deadline.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if !socket_path.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!socket_path.exists(), "socket should be gone after SIGTERM to leader");
}
```

- [ ] **Step 9: Run all tests**

Run: `cargo test -p cleat --locked`
Expected: all tests pass including the new inspect and signal tests.

- [ ] **Step 10: Run clippy and fmt**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings && cargo +nightly-2026-03-12 fmt --check`
Expected: clean.

- [ ] **Step 11: Commit**

```bash
git add crates/cleat/
git commit -m "feat(cleat): add inspect and signal commands with daemon handlers"
```

---

### Task 3: Output recorder module

**Files:**
- Create: `crates/cleat/src/recording.rs`
- Create: `crates/cleat/tests/recording.rs`
- Modify: `crates/cleat/src/lib.rs`

- [ ] **Step 1: Write unit tests for OutputRecorder**

Create `crates/cleat/tests/recording.rs`:

```rust
use std::fs;

use cleat::recording::OutputRecorder;

#[test]
fn new_recorder_creates_output_log() {
    let temp = tempfile::tempdir().expect("tempdir");
    let _recorder = OutputRecorder::new(temp.path()).expect("create recorder");
    assert!(temp.path().join("output.log").exists());
}

#[test]
fn record_appends_bytes_and_tracks_count() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut recorder = OutputRecorder::new(temp.path()).expect("create recorder");

    recorder.record(b"hello").expect("record hello");
    assert_eq!(recorder.bytes_written(), 5);

    recorder.record(b" world").expect("record world");
    assert_eq!(recorder.bytes_written(), 11);

    let contents = fs::read(temp.path().join("output.log")).expect("read log");
    assert_eq!(contents, b"hello world");
}

#[test]
fn bytes_written_starts_at_zero() {
    let temp = tempfile::tempdir().expect("tempdir");
    let recorder = OutputRecorder::new(temp.path()).expect("create recorder");
    assert_eq!(recorder.bytes_written(), 0);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p cleat --locked --test recording`
Expected: compilation error — `cleat::recording` module doesn't exist.

- [ ] **Step 3: Implement OutputRecorder**

Add `pub mod recording;` to `crates/cleat/src/lib.rs`.

Create `crates/cleat/src/recording.rs`:

```rust
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::Path,
};

const OUTPUT_LOG_NAME: &str = "output.log";

pub struct OutputRecorder {
    log_file: File,
    bytes_written: u64,
}

impl OutputRecorder {
    pub fn new(session_dir: &Path) -> Result<Self, String> {
        let log_path = session_dir.join(OUTPUT_LOG_NAME);
        let log_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|err| format!("open output log {}: {err}", log_path.display()))?;

        let bytes_written = log_file
            .metadata()
            .map(|m| m.len())
            .unwrap_or(0);

        Ok(Self { log_file, bytes_written })
    }

    pub fn record(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.log_file.write_all(bytes).map_err(|err| format!("write output log: {err}"))?;
        self.bytes_written += bytes.len() as u64;
        Ok(())
    }

    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p cleat --locked --test recording`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/cleat/src/recording.rs crates/cleat/src/lib.rs crates/cleat/tests/recording.rs
git commit -m "feat(cleat): add OutputRecorder module for append-only PTY output logging"
```

---

### Task 4: Recording daemon integration and activation

**Files:**
- Modify: `crates/cleat/src/session.rs`
- Modify: `crates/cleat/src/runtime.rs`
- Modify: `crates/cleat/src/cli.rs`
- Modify: `crates/cleat/src/server.rs`
- Modify: `crates/cleat/tests/lifecycle.rs`

- [ ] **Step 1: Add `record` field to SessionMetadata**

In `crates/cleat/src/runtime.rs`, add to `SessionMetadata`:

```rust
#[serde(default)]
pub record: bool,
```

- [ ] **Step 2: Add `--record` flag to Create and Attach CLI commands**

In `crates/cleat/src/cli.rs`, add to both `Create` and `Attach` variants:

```rust
#[arg(long)]
record: bool,
```

**Important: do NOT change the `SessionService::create()` or `RuntimeLayout::create_session()` method signatures.** Instead, handle `record` in the CLI execute layer by setting it on the metadata after creation. The `record` field on `SessionMetadata` is `#[serde(default)]` so existing code and test call sites are unaffected.

In `cli.rs` `execute()`, update the `Create` arm to write the record flag to metadata after creation:

```rust
Command::Create { id, json, vt, cwd, cmd, record } => {
    let created = service.create(id, vt, cwd, cmd)?;
    if record {
        // Update metadata to enable recording — the daemon reads this on startup
        let meta_path = service.layout_root().join(&created.id).join("meta.json");
        if let Ok(contents) = std::fs::read_to_string(&meta_path) {
            if let Ok(mut meta) = serde_json::from_str::<crate::runtime::SessionMetadata>(&contents) {
                meta.record = true;
                let _ = std::fs::write(&meta_path, serde_json::to_string_pretty(&meta).expect("serialize"));
            }
        }
    }
    // ... existing json/id output logic
}
```

Add a `layout_root()` accessor to `SessionService` in `server.rs`:

```rust
pub fn layout_root(&self) -> &std::path::Path {
    self.layout.root()
}
```

Similarly update the `Attach` arm to pass the `record` flag. For `Attach`, the daemon may already be running, so use the `RecordControl` frame instead (same as `cleat record`):

```rust
Command::Attach { id, no_create, vt, cwd, cmd, record } => {
    let (_attached, guard) = service.attach(id, vt, cwd, cmd, no_create)?;
    if record {
        // Best-effort: activate recording on the now-attached session
        if let Some(ref session_id) = _attached.id.clone().into() {
            let _ = service.record(&session_id, true);
        }
    }
    guard.relay_stdio()?;
    Ok(None)
}
```

**Existing tests are unaffected** because `record: bool` defaults to `false` via clap, and the `Command` enum's `PartialEq` assertions in `cli.rs` tests use `matches!` patterns (not struct literal equality) for the existing `Create` and `Attach` tests. However, check that any `assert_eq!(cli.command, Command::Create { ... })` tests in `cli.rs` include `record: false` — if they use struct literal equality, add the field.

**Also update the `help_lists_expected_subcommands` test** in `crates/cleat/tests/cli.rs` to include the new subcommands (`inspect`, `signal`, `record`) in the expected list.

- [ ] **Step 3: Wire recorder into daemon event loop**

In `run_session_daemon` in `crates/cleat/src/session.rs`, **replace** the placeholder `let mut recorder: Option<()> = None;` (from Task 2) with the real type and activation logic:

```rust
let mut recorder: Option<crate::recording::OutputRecorder> = None;
if session.record || std::env::var("CLEAT_RECORD").map(|v| v == "1").unwrap_or(false) {
    match crate::recording::OutputRecorder::new(&root.join(id)) {
        Ok(r) => recorder = Some(r),
        Err(err) => eprintln!("failed to start recording: {err}"),
    }
}
```

Also update `build_inspect_result` to accept the real recorder type and report actual state:

```rust
// Change parameter type from Option<()> to Option<crate::recording::OutputRecorder>
fn build_inspect_result(
    session: &SessionMetadata,
    vt_engine: &dyn VtEngine,
    active_client: &Option<ActiveClient>,
    pty_child: &PtyChild,
    recorder: &Option<crate::recording::OutputRecorder>,
) -> crate::protocol::InspectResult {
    // ... (same as Task 2, but update the recording field):
    recording: crate::protocol::RecordingInspect {
        active: recorder.is_some(),
        bytes_written: recorder.as_ref().map(|r| r.bytes_written()).unwrap_or(0),
        cursor: recorder.as_ref().map(|r| r.bytes_written()).unwrap_or(0),
    },
}
```

In the PTY output section (inside `if poll_result.pty_readable`), after `record_pty_output(vt_engine.as_mut(), &buf[..n])?;`, add:

```rust
if let Some(ref mut rec) = recorder {
    if let Err(err) = rec.record(&buf[..n]) {
        eprintln!("recording error: {err}");
        recorder = None;
    }
}
```

- [ ] **Step 4: Add RecordControl handler in daemon**

In the new-connection frame dispatch (same block as Inspect and Signal), add:

```rust
Ok(Frame::RecordControl { enable }) => {
    if enable && recorder.is_none() {
        match crate::recording::OutputRecorder::new(&root.join(id)) {
            Ok(r) => {
                recorder = Some(r);
                let _ = Frame::Ack.write(&mut stream);
            }
            Err(err) => {
                let _ = Frame::Error(err).write(&mut stream);
            }
        }
    } else if !enable {
        recorder = None;
        let _ = Frame::Ack.write(&mut stream);
    } else {
        let _ = Frame::Ack.write(&mut stream);
    }
}
```

- [ ] **Step 5: Add `record` CLI command and service method**

Add to the `Command` enum in `cli.rs`:

```rust
Record {
    id: String,
},
```

Add execute routing:

```rust
Command::Record { id } => {
    service.record(&id, true)?;
    Ok(None)
}
```

Add service method in `server.rs`:

```rust
pub fn record(&self, id: &str, enable: bool) -> Result<(), String> {
    if !self.layout.root().join(id).join("meta.json").exists() {
        return Err(format!("missing session {id}"));
    }
    let socket_path = session_socket_path(self.layout.root(), id);
    let mut stream =
        std::os::unix::net::UnixStream::connect(&socket_path).map_err(|err| format!("connect {}: {err}", socket_path.display()))?;
    Frame::RecordControl { enable }.write(&mut stream).map_err(|err| format!("write record control: {err}"))?;
    match Frame::read(&mut stream).map_err(|err| format!("read record response: {err}"))? {
        Frame::Ack => Ok(()),
        Frame::Error(message) => Err(message),
        other => Err(format!("unexpected record response: {other:?}")),
    }
}
```

- [ ] **Step 6: Write lifecycle test for recording with --record flag**

Add to `crates/cleat/tests/lifecycle.rs`:

```rust
#[test]
fn create_with_record_flag_activates_recording() {
    let _lock = env_lock().lock().expect("env lock");
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());

    // Create with --record via CLI
    let cli = Cli::try_parse_from(["cleat", "create", "gamma", "--record"]).expect("parse create --record");
    cli::execute(cli, &service).expect("execute create --record");

    let socket_path = session_socket_path(temp.path(), "gamma");
    wait_for_socket(&socket_path);

    let result = service.inspect("gamma").expect("inspect session");
    assert!(result.recording.active, "recording should be active with --record flag");

    service.kill("gamma").expect("kill session");
}
```

- [ ] **Step 7: Write lifecycle test for runtime recording activation via `cleat record`**

Add to `crates/cleat/tests/lifecycle.rs`:

```rust
#[test]
fn record_command_activates_recording_on_running_session() {
    let _lock = env_lock().lock().expect("env lock");
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let info = service.create(Some("delta".into()), None, None, None).expect("create session");

    let socket_path = session_socket_path(temp.path(), &info.id);
    wait_for_socket(&socket_path);

    // Verify recording is off
    let result = service.inspect(&info.id).expect("inspect before record");
    assert!(!result.recording.active);

    // Activate recording
    service.record(&info.id, true).expect("activate recording");

    // Verify recording is now on
    let result = service.inspect(&info.id).expect("inspect after record");
    assert!(result.recording.active);

    service.kill(&info.id).expect("kill session");
}
```

- [ ] **Step 8: Run all tests**

Run: `cargo test -p cleat --locked`
Expected: all tests pass.

- [ ] **Step 9: Run clippy and fmt**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings && cargo +nightly-2026-03-12 fmt --check`
Expected: clean.

- [ ] **Step 10: Commit**

```bash
git add crates/cleat/
git commit -m "feat(cleat): integrate output recording with daemon, --record flag, and record command"
```

---

### Task 5: VT snapshots

**Files:**
- Modify: `crates/cleat/src/recording.rs`
- Modify: `crates/cleat/src/session.rs`
- Modify: `crates/cleat/tests/recording.rs`

- [ ] **Step 1: Write unit test for snapshot writing**

Add to `crates/cleat/tests/recording.rs`:

```rust
#[test]
fn take_snapshot_writes_to_snapshots_directory() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut recorder = OutputRecorder::new(temp.path()).expect("create recorder");
    recorder.record(b"hello world").expect("record bytes");

    recorder.take_snapshot(b"screen state data").expect("take snapshot");

    let snapshot_dir = temp.path().join("snapshots");
    assert!(snapshot_dir.exists());
    let entries: Vec<_> = fs::read_dir(&snapshot_dir).expect("read snapshots").collect();
    assert_eq!(entries.len(), 1);
}

#[test]
fn snapshot_filename_includes_byte_offset() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut recorder = OutputRecorder::new(temp.path()).expect("create recorder");
    recorder.record(b"12345").expect("record 5 bytes");

    recorder.take_snapshot(b"snap").expect("take snapshot");

    let snapshot_path = temp.path().join("snapshots").join("at-5.bin");
    assert!(snapshot_path.exists());
    assert_eq!(fs::read(&snapshot_path).expect("read snapshot"), b"snap");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p cleat --locked --test recording`
Expected: compilation error — `take_snapshot` doesn't exist.

- [ ] **Step 3: Implement snapshot support in OutputRecorder**

Add to `OutputRecorder` in `recording.rs`:

```rust
pub fn take_snapshot(&mut self, data: &[u8]) -> Result<(), String> {
    let snapshot_dir = self.session_dir.join("snapshots");
    std::fs::create_dir_all(&snapshot_dir).map_err(|err| format!("create snapshot dir: {err}"))?;
    let snapshot_path = snapshot_dir.join(format!("at-{}.bin", self.bytes_written));
    std::fs::write(&snapshot_path, data).map_err(|err| format!("write snapshot {}: {err}", snapshot_path.display()))
}
```

Add a `session_dir: PathBuf` field to `OutputRecorder` and initialize it in `new()`:

```rust
pub struct OutputRecorder {
    log_file: File,
    bytes_written: u64,
    session_dir: PathBuf,
}

// In new():
Ok(Self { log_file, bytes_written, session_dir: session_dir.to_path_buf() })
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p cleat --locked --test recording`
Expected: PASS.

- [ ] **Step 5: Wire periodic snapshots into daemon**

In `run_session_daemon` in `session.rs`, add a snapshot counter after the recorder initialization:

```rust
let mut bytes_since_snapshot: u64 = 0;
const SNAPSHOT_INTERVAL_BYTES: u64 = 256 * 1024; // 256 KB
```

In the PTY output section, after recording bytes, add:

```rust
if let Some(ref mut rec) = recorder {
    bytes_since_snapshot += n as u64;
    if bytes_since_snapshot >= SNAPSHOT_INTERVAL_BYTES {
        if let Ok(text) = vt_engine.screen_text() {
            let _ = rec.take_snapshot(text.as_bytes());
        }
        bytes_since_snapshot = 0;
    }
}
```

Note: `screen_text()` returns `Err` for passthrough engine, which is fine — the `if let Ok` skips snapshots when the engine doesn't support them.

- [ ] **Step 6: Run all tests**

Run: `cargo test -p cleat --locked`
Expected: all tests pass.

- [ ] **Step 7: Run clippy and fmt**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings && cargo +nightly-2026-03-12 fmt --check`
Expected: clean.

- [ ] **Step 8: Commit**

```bash
git add crates/cleat/
git commit -m "feat(cleat): add periodic VT snapshots during recording"
```

---

### Task 6: CLI parsing test coverage for new commands

**Files:**
- Modify: `crates/cleat/tests/cli.rs`

- [ ] **Step 1: Add CLI parsing tests for record command**

Add to `crates/cleat/tests/cli.rs`:

```rust
#[test]
fn record_parses_session_id() {
    let cli = Cli::try_parse_from(["cleat", "record", "alpha"]).expect("parse record");
    assert!(matches!(cli.command, Command::Record { ref id } if id == "alpha"));
}

#[test]
fn create_record_flag() {
    let cli = Cli::try_parse_from(["cleat", "create", "alpha", "--record"]).expect("parse create --record");
    assert!(matches!(cli.command, Command::Create { record: true, .. }));
}

#[test]
fn inspect_missing_session_is_an_error() {
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let err = service.inspect("missing").expect_err("missing session should error");
    assert!(err.contains("missing"));
}

#[test]
fn signal_missing_session_is_an_error() {
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    let err = service
        .signal("missing", libc::SIGINT, cleat::protocol::SignalTarget::Foreground)
        .expect_err("missing session should error");
    assert!(err.contains("missing"));
}
```

The `inspect_missing_session_is_an_error` and `signal_missing_session_is_an_error` tests use `service_for()` which is defined in `lifecycle.rs`. **Add these two tests to `crates/cleat/tests/lifecycle.rs`**, not `cli.rs`. The `record_parses_session_id` and `create_record_flag` tests are pure CLI parsing and belong in `cli.rs`.

- [ ] **Step 2: Run tests**

Run: `cargo test -p cleat --locked`
Expected: all pass.

- [ ] **Step 3: Commit**

```bash
git add crates/cleat/tests/
git commit -m "test(cleat): add CLI parsing and error handling tests for inspect, signal, record"
```

---

### Task 7: Final verification and cleanup

**Files:**
- All modified files

- [ ] **Step 1: Run full CI gate**

```bash
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Expected: all clean.

- [ ] **Step 2: Fix any issues found**

Address any clippy warnings, unused imports, or test failures.

- [ ] **Step 3: Verify inspect output end-to-end**

Build and run manually if possible — create a session, inspect it, verify JSON output looks correct:

```bash
cargo build -p cleat
./target/debug/cleat create test-session
./target/debug/cleat inspect test-session --json
./target/debug/cleat kill test-session
```

**Deferred from this plan:** Size limits and cooperative reaping across sessions (described in the spec) are deferred to a follow-up. This plan lands unbounded recording; size management will be added before recording sees heavy use.

- [ ] **Step 4: Commit any cleanup**

```bash
git add -A
git commit -m "chore(cleat): final cleanup for inspect, signal, and recording"
```
