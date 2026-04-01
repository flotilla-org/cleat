# CLI Agent Readiness Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make cleat's CLI discoverable and ergonomic for AI agents — add help text, align commands with tmux-cli, add `send`/`wait`/`interrupt`/`escape`, fix since-marker off-by-one.

**Architecture:** All changes are in the `cleat` crate. Tasks 1–3 are CLI-only (clap attributes and new `Command` variants wired to existing service methods). Task 4 adds new protocol frames and daemon-side wait logic. Task 5 changes `main.rs` to support typed exit codes. Task 6 investigates and fixes the since-marker off-by-one.

**Tech Stack:** Rust, clap (derive), unix sockets, nix (poll), serde_json

**Spec:** `docs/superpowers/specs/2026-04-01-cli-agent-readiness.md`

---

### Task 1: Add help text to all subcommands and arguments

**Files:**
- Modify: `crates/cleat/src/cli.rs`

- [ ] **Step 1: Add top-level about and subcommand descriptions**

Add `about` to the `#[command]` attribute on `Cli`, and `about` to each variant in `Command`:

```rust
#[derive(Debug, Parser)]
#[command(name = "cleat", version, about = "Session daemon with a structured control plane for agents and terminal persistence")]
pub struct Cli {
    #[arg(long, hide = true)]
    pub runtime_root: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum Command {
    #[command(about = "Create a new session", alias = "create", after_long_help = "\
Tip: launch a shell (e.g. zsh) and use `send` to run commands.\n\
Sessions exit when the launched process exits.")]
    Launch {
        // ... (args shown in step 2)
    },
    #[command(about = "Attach to a session interactively")]
    Attach { /* ... */ },
    #[command(about = "Send text to a session")]
    Send { /* ... */ },
    #[command(about = "Send key sequences using tmux-style names", after_long_help = "\
Key names: Enter, Escape (Esc), Tab, BSpace, Space,\n\
           Up, Down, Left, Right, Home, End,\n\
           PgUp (PageUp), PgDn (PageDown),\n\
           IC (Insert), DC (Delete),\n\
           F1-F12, BTab (Shift-Tab)\n\
\n\
Modifiers:  C-x (Ctrl), M-x (Meta/Alt), S-x (Shift)\n\
            ^x  (Ctrl, alternative syntax)\n\
\n\
Examples:   cleat send-keys myapp Enter\n\
            cleat send-keys myapp C-c\n\
            cleat send-keys myapp -l 'literal text'\n\
            cleat send-keys myapp -H 1b5b41")]
    SendKeys { /* ... */ },
    #[command(about = "Capture terminal screen content")]
    Capture { /* ... */ },
    #[command(about = "List all sessions")]
    List { /* ... */ },
    #[command(about = "Show session state and process info")]
    Inspect { /* ... */ },
    #[command(about = "Wait for a condition before continuing")]
    Wait { /* ... */ },
    #[command(about = "Terminate a session")]
    Kill { /* ... */ },
    #[command(about = "Detach from a session")]
    Detach { /* ... */ },
    #[command(about = "Send an OS signal to the session process")]
    Signal { /* ... */ },
    #[command(about = "Enable output recording")]
    Record { /* ... */ },
    #[command(about = "Set a named marker in the recording")]
    Mark { /* ... */ },
    #[command(about = "Send Ctrl-C to a session")]
    Interrupt { /* ... */ },
    #[command(about = "Send Escape to a session")]
    Escape { /* ... */ },
    #[command(hide = true)]
    Serve { /* ... */ },
}
```

- [ ] **Step 2: Add help text to all undocumented arguments**

Add `help` attributes to every argument that lacks one. These go on the existing args across all variants. Examples for the `Launch` (formerly `Create`) variant:

```rust
Launch {
    #[arg(value_name = "ID")]
    id: Option<String>,
    #[arg(long, help = "Output as JSON")]
    json: bool,
    #[arg(long, value_enum, help = "Virtual terminal engine")]
    vt: Option<VtEngineKind>,
    #[arg(long, help = "Working directory for the session")]
    cwd: Option<PathBuf>,
    #[arg(long, help = "Command to run (default: user's shell)")]
    cmd: Option<String>,
    #[arg(long, env = "CLEAT_RECORD", help = "Enable output recording")]
    record: bool,
},
```

Apply the same pattern to all variants:

- `Attach`: add `help` to `--no-create` ("Fail if the session does not exist"), `--vt`, `--cwd`, `--cmd`, `--record`
- `SendKeys`: add `help` to `-l` ("Send keys as literal characters"), `-H` ("Send keys as hex-encoded bytes"), `-N` ("Repeat the key sequence N times")
- `Signal`: add `help` to `--target` ("Signal target: foreground (default) or leader")
- `List`, `Inspect`: add `help` to `--json` ("Output as JSON")
- `Capture`: args already documented — no change needed

- [ ] **Step 3: Run tests to verify nothing breaks**

Run: `cargo test --workspace --locked`
Expected: All existing tests pass.

- [ ] **Step 4: Verify help output looks correct**

Run: `cargo run -- --help` and `cargo run -- send-keys --help`
Expected: All subcommands show descriptions. Arguments show help text. `send-keys` shows key name reference in long help.

- [ ] **Step 5: Commit**

```bash
git add crates/cleat/src/cli.rs
git commit -m "docs(cli): add help text to all subcommands and arguments"
```

---

### Task 2: Rename `create` to `launch`, update tests

**Files:**
- Modify: `crates/cleat/src/cli.rs`
- Modify: `crates/cleat/tests/cli.rs`

- [ ] **Step 1: Update test for subcommand list**

In `crates/cleat/tests/cli.rs`, update `help_lists_expected_subcommands` to expect `launch` instead of `create`:

```rust
#[test]
fn help_lists_expected_subcommands() {
    let command = Cli::command();
    let subcommands: Vec<_> = command.get_subcommands().filter(|sub| !sub.is_hide_set()).map(|sub| sub.get_name().to_string()).collect();
    assert!(subcommands.contains(&"launch".to_string()), "expected 'launch' in subcommands: {subcommands:?}");
    assert!(!subcommands.contains(&"create".to_string()), "'create' should be hidden alias, not visible: {subcommands:?}");
}
```

- [ ] **Step 2: Add test that `create` alias still parses**

```rust
#[test]
fn create_alias_still_parses() {
    let cli = Cli::try_parse_from(["cleat", "create", "--cmd", "bash"]).expect("create alias parses");
    assert!(matches!(cli.command, Command::Launch { .. }));
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test --workspace --locked`
Expected: FAIL — `Command::Create` still exists, new tests reference `Command::Launch` which doesn't exist yet.

- [ ] **Step 4: Rename `Create` to `Launch` in cli.rs**

In `crates/cleat/src/cli.rs`:

1. Rename the `Create` variant to `Launch`
2. Add `#[command(alias = "create")]` — the `about` was already added in Task 1
3. Update the `execute` match arm from `Command::Create { .. }` to `Command::Launch { .. }`

```rust
#[command(about = "Create a new session", alias = "create", after_long_help = "\
Tip: launch a shell (e.g. zsh) and use `send` to run commands.\n\
Sessions exit when the launched process exits.")]
Launch {
    #[arg(value_name = "ID")]
    id: Option<String>,
    #[arg(long, help = "Output as JSON")]
    json: bool,
    #[arg(long, value_enum, help = "Virtual terminal engine")]
    vt: Option<VtEngineKind>,
    #[arg(long, help = "Working directory for the session")]
    cwd: Option<PathBuf>,
    #[arg(long, help = "Command to run (default: user's shell)")]
    cmd: Option<String>,
    #[arg(long, env = "CLEAT_RECORD", help = "Enable output recording")]
    record: bool,
},
```

And in `execute`:

```rust
Command::Launch { id, json, vt, cwd, cmd, record } => {
    let created = service.create(id, vt, cwd, cmd, record)?;
    // ... rest unchanged
```

- [ ] **Step 5: Update existing tests that reference `Command::Create`**

In `crates/cleat/tests/cli.rs`, update all tests that construct or match `Command::Create` to use `Command::Launch`. There are 5 tests to update:

- `create_command_parses` → match `Command::Launch`
- `create_command_parses_positional_name` → match `Command::Launch`
- `create_command_parses_json` → match `Command::Launch`
- `create_command_parses_vt` → match `Command::Launch`
- `create_record_flag` → match `Command::Launch`

The parse input strings stay as `"create"` for some (to test the alias) and should add `"launch"` variants.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test --workspace --locked`
Expected: All tests pass.

- [ ] **Step 7: Commit**

```bash
git add crates/cleat/src/cli.rs crates/cleat/tests/cli.rs
git commit -m "refactor(cli): rename create to launch, keep create as hidden alias"
```

---

### Task 3: Add `send`, `interrupt`, `escape` subcommands

**Files:**
- Modify: `crates/cleat/src/cli.rs`
- Modify: `crates/cleat/tests/cli.rs`

- [ ] **Step 1: Write CLI parsing tests for `send`**

In `crates/cleat/tests/cli.rs`:

```rust
#[test]
fn send_command_parses() {
    let cli = Cli::try_parse_from(["cleat", "send", "demo", "echo hello"]).expect("send parses");
    assert_eq!(cli.command, Command::Send { id: "demo".into(), text: "echo hello".into(), no_enter: false });
}

#[test]
fn send_command_parses_no_enter() {
    let cli = Cli::try_parse_from(["cleat", "send", "--no-enter", "demo", "partial"]).expect("send --no-enter parses");
    assert_eq!(cli.command, Command::Send { id: "demo".into(), text: "partial".into(), no_enter: true });
}
```

- [ ] **Step 2: Write CLI parsing tests for `interrupt` and `escape`**

```rust
#[test]
fn interrupt_command_parses() {
    let cli = Cli::try_parse_from(["cleat", "interrupt", "demo"]).expect("interrupt parses");
    assert_eq!(cli.command, Command::Interrupt { id: "demo".into() });
}

#[test]
fn escape_command_parses() {
    let cli = Cli::try_parse_from(["cleat", "escape", "demo"]).expect("escape parses");
    assert_eq!(cli.command, Command::Escape { id: "demo".into() });
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test --workspace --locked`
Expected: FAIL — `Command::Send`, `Command::Interrupt`, `Command::Escape` don't exist yet.

- [ ] **Step 4: Add the three new variants to `Command` enum**

In `crates/cleat/src/cli.rs`, add these variants to the `Command` enum:

```rust
#[command(about = "Send text to a session")]
Send {
    id: String,
    #[arg(value_name = "TEXT", help = "Text to send")]
    text: String,
    #[arg(long, help = "Do not append Enter after the text")]
    no_enter: bool,
},
#[command(about = "Send Ctrl-C to a session")]
Interrupt {
    id: String,
},
#[command(about = "Send Escape to a session")]
Escape {
    id: String,
},
```

- [ ] **Step 5: Add execute arms for the three new commands**

In the `execute` function in `crates/cleat/src/cli.rs`:

```rust
Command::Send { id, text, no_enter } => {
    let mut bytes = text.into_bytes();
    if !no_enter {
        bytes.push(b'\r');
    }
    service.send_keys(&id, &bytes)?;
    Ok(None)
}
Command::Interrupt { id } => {
    service.send_keys(&id, &[0x03])?;
    Ok(None)
}
Command::Escape { id } => {
    service.send_keys(&id, &[0x1b])?;
    Ok(None)
}
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test --workspace --locked`
Expected: All tests pass.

- [ ] **Step 7: Verify help output includes new commands**

Run: `cargo run -- --help`
Expected: `send`, `interrupt`, `escape` appear in the command list with descriptions.

- [ ] **Step 8: Commit**

```bash
git add crates/cleat/src/cli.rs crates/cleat/tests/cli.rs
git commit -m "feat(cli): add send, interrupt, escape commands for agent ergonomics"
```

---

### Task 4: Add `wait` protocol frames and daemon handler

**Files:**
- Modify: `crates/cleat/src/protocol.rs`
- Modify: `crates/cleat/src/session.rs`

- [ ] **Step 1: Write round-trip tests for Wait and WaitResult frames**

In `crates/cleat/src/protocol.rs`, add to the `tests` module:

```rust
#[test]
fn wait_output_idle_round_trip() {
    let frame = Frame::Wait {
        conditions: vec![WaitCondition::OutputIdle { quiet_ms: 2000 }],
        timeout_ms: 30000,
    };
    let mut bytes = Vec::new();
    frame.write(&mut bytes).expect("write");
    let decoded = Frame::read(&mut bytes.as_slice()).expect("read");
    assert_eq!(decoded, frame);
}

#[test]
fn wait_text_match_round_trip() {
    let frame = Frame::Wait {
        conditions: vec![WaitCondition::TextMatch { text: "DONE".to_string() }],
        timeout_ms: 5000,
    };
    let mut bytes = Vec::new();
    frame.write(&mut bytes).expect("write");
    let decoded = Frame::read(&mut bytes.as_slice()).expect("read");
    assert_eq!(decoded, frame);
}

#[test]
fn wait_combined_conditions_round_trip() {
    let frame = Frame::Wait {
        conditions: vec![
            WaitCondition::OutputIdle { quiet_ms: 1000 },
            WaitCondition::TextMatch { text: "ready".to_string() },
        ],
        timeout_ms: 10000,
    };
    let mut bytes = Vec::new();
    frame.write(&mut bytes).expect("write");
    let decoded = Frame::read(&mut bytes.as_slice()).expect("read");
    assert_eq!(decoded, frame);
}

#[test]
fn wait_result_ready_round_trip() {
    let frame = Frame::WaitResult { status: WaitStatus::Ready, elapsed_ms: 342 };
    let mut bytes = Vec::new();
    frame.write(&mut bytes).expect("write");
    let decoded = Frame::read(&mut bytes.as_slice()).expect("read");
    assert_eq!(decoded, frame);
}

#[test]
fn wait_result_timeout_round_trip() {
    let frame = Frame::WaitResult { status: WaitStatus::Timeout, elapsed_ms: 30000 };
    let mut bytes = Vec::new();
    frame.write(&mut bytes).expect("write");
    let decoded = Frame::read(&mut bytes.as_slice()).expect("read");
    assert_eq!(decoded, frame);
}

#[test]
fn wait_result_session_gone_round_trip() {
    let frame = Frame::WaitResult { status: WaitStatus::SessionGone, elapsed_ms: 1204 };
    let mut bytes = Vec::new();
    frame.write(&mut bytes).expect("write");
    let decoded = Frame::read(&mut bytes.as_slice()).expect("read");
    assert_eq!(decoded, frame);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --workspace --locked`
Expected: FAIL — `Frame::Wait`, `WaitCondition`, `WaitStatus` don't exist.

- [ ] **Step 3: Add types and frame variants to protocol.rs**

Add the new types:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitCondition {
    OutputIdle { quiet_ms: u64 },
    TextMatch { text: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitStatus {
    Ready = 0,
    Timeout = 1,
    SessionGone = 2,
}
```

Add new tag constants:

```rust
const TAG_WAIT: u8 = 18;
const TAG_WAIT_RESULT: u8 = 19;
```

Add to the `Frame` enum:

```rust
Wait { conditions: Vec<WaitCondition>, timeout_ms: u64 },
WaitResult { status: WaitStatus, elapsed_ms: u64 },
```

Add encode logic in `Frame::encode`:

```rust
Frame::Wait { conditions, timeout_ms } => {
    let mut payload = Vec::new();
    payload.extend_from_slice(&timeout_ms.to_le_bytes());
    payload.push(conditions.len() as u8);
    for condition in conditions {
        match condition {
            WaitCondition::OutputIdle { quiet_ms } => {
                payload.push(0); // type tag
                payload.extend_from_slice(&quiet_ms.to_le_bytes());
            }
            WaitCondition::TextMatch { text } => {
                payload.push(1); // type tag
                let text_bytes = text.as_bytes();
                payload.extend_from_slice(&(text_bytes.len() as u32).to_le_bytes());
                payload.extend_from_slice(text_bytes);
            }
        }
    }
    (TAG_WAIT, payload)
}
Frame::WaitResult { status, elapsed_ms } => {
    let mut payload = Vec::with_capacity(9);
    payload.push(*status as u8);
    payload.extend_from_slice(&elapsed_ms.to_le_bytes());
    (TAG_WAIT_RESULT, payload)
}
```

Add decode logic in `Frame::decode`:

```rust
TAG_WAIT => {
    if payload.len() < 9 {
        return Err(Error::new(ErrorKind::InvalidData, "wait frame too short"));
    }
    let timeout_ms = u64::from_le_bytes([
        payload[0], payload[1], payload[2], payload[3],
        payload[4], payload[5], payload[6], payload[7],
    ]);
    let num_conditions = payload[8] as usize;
    let mut offset = 9;
    let mut conditions = Vec::with_capacity(num_conditions);
    for _ in 0..num_conditions {
        if offset >= payload.len() {
            return Err(Error::new(ErrorKind::InvalidData, "wait frame truncated"));
        }
        let condition_type = payload[offset];
        offset += 1;
        match condition_type {
            0 => {
                if offset + 8 > payload.len() {
                    return Err(Error::new(ErrorKind::InvalidData, "output idle condition truncated"));
                }
                let quiet_ms = u64::from_le_bytes([
                    payload[offset], payload[offset+1], payload[offset+2], payload[offset+3],
                    payload[offset+4], payload[offset+5], payload[offset+6], payload[offset+7],
                ]);
                offset += 8;
                conditions.push(WaitCondition::OutputIdle { quiet_ms });
            }
            1 => {
                if offset + 4 > payload.len() {
                    return Err(Error::new(ErrorKind::InvalidData, "text match condition truncated"));
                }
                let text_len = u32::from_le_bytes([
                    payload[offset], payload[offset+1], payload[offset+2], payload[offset+3],
                ]) as usize;
                offset += 4;
                if offset + text_len > payload.len() {
                    return Err(Error::new(ErrorKind::InvalidData, "text match text truncated"));
                }
                let text = String::from_utf8(payload[offset..offset+text_len].to_vec())
                    .map_err(|e| Error::new(ErrorKind::InvalidData, format!("invalid text match utf-8: {e}")))?;
                offset += text_len;
                conditions.push(WaitCondition::TextMatch { text });
            }
            other => {
                return Err(Error::new(ErrorKind::InvalidData, format!("unknown wait condition type {other}")));
            }
        }
    }
    Ok(Frame::Wait { conditions, timeout_ms })
}
TAG_WAIT_RESULT => {
    if payload.len() != 9 {
        return Err(Error::new(ErrorKind::InvalidData, "invalid wait result frame"));
    }
    let status = match payload[0] {
        0 => WaitStatus::Ready,
        1 => WaitStatus::Timeout,
        2 => WaitStatus::SessionGone,
        other => return Err(Error::new(ErrorKind::InvalidData, format!("unknown wait status {other}"))),
    };
    let elapsed_ms = u64::from_le_bytes([
        payload[1], payload[2], payload[3], payload[4],
        payload[5], payload[6], payload[7], payload[8],
    ]);
    Ok(Frame::WaitResult { status, elapsed_ms })
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --workspace --locked`
Expected: All tests pass including the new round-trip tests.

- [ ] **Step 5: Commit**

```bash
git add crates/cleat/src/protocol.rs
git commit -m "feat(protocol): add Wait and WaitResult frames with condition types"
```

- [ ] **Step 6: Add pending-waiter tracking to the daemon event loop**

In `crates/cleat/src/session.rs`, add a struct to track pending waiters and a helper to evaluate conditions:

```rust
use std::time::Instant;
use std::os::unix::net::UnixStream;

struct PendingWait {
    stream: UnixStream,
    conditions: Vec<crate::protocol::WaitCondition>,
    timeout_ms: u64,
    registered_at: Instant,
}
```

In `run_session_daemon`, add state tracking near the top of the event loop setup (before the `loop`):

```rust
let mut pending_waits: Vec<PendingWait> = Vec::new();
let mut last_pty_output_at: Option<Instant> = None;
```

- [ ] **Step 7: Handle the Wait frame in the listener-readable section**

In the `match Frame::read(&mut stream)` block that handles accepted socket connections, add a new arm:

```rust
Ok(Frame::Wait { conditions, timeout_ms }) => {
    // Validate all conditions before registering
    let has_text_match = conditions.iter().any(|c| matches!(c, crate::protocol::WaitCondition::TextMatch { .. }));
    if has_text_match {
        if let Err(err) = vt_engine.screen_text() {
            let _ = Frame::Error(format!("text matching not supported: {err}")).write(&mut stream);
            continue; // skip registration — this is inside the accept loop
        }
    }
    if conditions.is_empty() {
        let _ = Frame::Error("at least one wait condition is required".to_string()).write(&mut stream);
        continue;
    }

    // Check --text immediately at registration
    let mut already_met = false;
    for condition in &conditions {
        if let crate::protocol::WaitCondition::TextMatch { text } = condition {
            if let Ok(screen) = vt_engine.screen_text() {
                if screen.contains(text.as_str()) {
                    let elapsed = 0u64;
                    let _ = Frame::WaitResult {
                        status: crate::protocol::WaitStatus::Ready,
                        elapsed_ms: elapsed,
                    }.write(&mut stream);
                    already_met = true;
                    break;
                }
            }
        }
        // OutputIdle cannot succeed at registration (measured from registration time)
    }

    if !already_met {
        stream.set_nonblocking(true).ok();
        pending_waits.push(PendingWait {
            stream,
            conditions,
            timeout_ms,
            registered_at: Instant::now(),
        });
    }
}
```

Note: the exact placement depends on the accept loop structure. The `continue` keyword should skip to the next iteration of the accept error-handling — review the actual code structure when implementing.

- [ ] **Step 8: Evaluate pending waits on each loop tick**

After the PTY read section and before the child-exit check, add wait evaluation. Update `last_pty_output_at` in the PTY read section first:

In the PTY readable block (around line 729), after `record_pty_output`:

```rust
last_pty_output_at = Some(Instant::now());
```

Then add the wait evaluation block:

```rust
// Evaluate pending waits
pending_waits.retain_mut(|wait| {
    let elapsed = wait.registered_at.elapsed();
    let elapsed_ms = elapsed.as_millis() as u64;

    // Check timeout
    if elapsed_ms >= wait.timeout_ms {
        let _ = Frame::WaitResult {
            status: crate::protocol::WaitStatus::Timeout,
            elapsed_ms,
        }.write(&mut wait.stream);
        return false; // remove
    }

    // Check conditions (OR semantics — any match wins)
    for condition in &wait.conditions {
        match condition {
            crate::protocol::WaitCondition::OutputIdle { quiet_ms } => {
                let silence_since = match last_pty_output_at {
                    Some(t) if t > wait.registered_at => t,
                    _ => wait.registered_at,
                };
                let quiet_duration = silence_since.elapsed().as_millis() as u64;
                if quiet_duration >= *quiet_ms {
                    let _ = Frame::WaitResult {
                        status: crate::protocol::WaitStatus::Ready,
                        elapsed_ms,
                    }.write(&mut wait.stream);
                    return false;
                }
            }
            crate::protocol::WaitCondition::TextMatch { text } => {
                if let Ok(screen) = vt_engine.screen_text() {
                    if screen.contains(text.as_str()) {
                        let _ = Frame::WaitResult {
                            status: crate::protocol::WaitStatus::Ready,
                            elapsed_ms,
                        }.write(&mut wait.stream);
                        return false;
                    }
                }
            }
        }
    }

    true // keep waiting
});
```

- [ ] **Step 9: Handle session exit for pending waiters**

In the `child_exited` block (around line 776), before the `break`, drain pending waits:

```rust
if let Some(status) = child_exited(pty_child.pid)? {
    // Notify pending waiters that the session is gone
    for mut wait in pending_waits.drain(..) {
        let elapsed_ms = wait.registered_at.elapsed().as_millis() as u64;
        let _ = Frame::WaitResult {
            status: crate::protocol::WaitStatus::SessionGone,
            elapsed_ms,
        }.write(&mut wait.stream);
    }

    if let Some(ref mut rec) = recorder {
        // ... existing recording logic ...
```

- [ ] **Step 10: Run tests to verify nothing breaks**

Run: `cargo test --workspace --locked`
Expected: All existing tests pass. The daemon changes are only exercised at runtime.

- [ ] **Step 11: Commit**

```bash
git add crates/cleat/src/session.rs
git commit -m "feat(daemon): handle Wait frames with pending-waiter tracking and condition evaluation"
```

---

### Task 5: Add `wait` CLI command with typed exit codes

**Files:**
- Modify: `crates/cleat/src/cli.rs`
- Modify: `crates/cleat/src/main.rs`
- Modify: `crates/cleat/tests/cli.rs`

- [ ] **Step 1: Write CLI parsing tests for `wait`**

In `crates/cleat/tests/cli.rs`:

```rust
#[test]
fn wait_requires_at_least_one_condition() {
    let result = Cli::try_parse_from(["cleat", "wait", "sess"]);
    assert!(result.is_err(), "wait without --idle-time or --text should fail");
}

#[test]
fn wait_idle_time_parses() {
    let cli = Cli::try_parse_from(["cleat", "wait", "sess", "--idle-time", "2.0"]).expect("parse");
    assert!(matches!(cli.command, Command::Wait { ref id, idle_time: Some(_), text: None, timeout, json: false } if id == "sess" && timeout == 30.0));
}

#[test]
fn wait_text_parses() {
    let cli = Cli::try_parse_from(["cleat", "wait", "sess", "--text", "DONE"]).expect("parse");
    assert!(matches!(cli.command, Command::Wait { text: Some(ref t), idle_time: None, .. } if t == "DONE"));
}

#[test]
fn wait_combined_parses() {
    let cli = Cli::try_parse_from(["cleat", "wait", "sess", "--idle-time", "1.0", "--text", "ready", "--timeout", "10"]).expect("parse");
    assert!(matches!(cli.command, Command::Wait { idle_time: Some(_), text: Some(_), timeout, .. } if timeout == 10.0));
}

#[test]
fn wait_json_flag() {
    let cli = Cli::try_parse_from(["cleat", "wait", "sess", "--idle-time", "1", "--json"]).expect("parse");
    assert!(matches!(cli.command, Command::Wait { json: true, .. }));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --workspace --locked`
Expected: FAIL — `Command::Wait` doesn't exist yet.

- [ ] **Step 3: Add `Wait` variant to the `Command` enum**

In `crates/cleat/src/cli.rs`:

```rust
#[command(about = "Wait for a condition before continuing")]
Wait {
    id: String,
    #[arg(long, help = "Wait until output settles for this many seconds")]
    idle_time: Option<f64>,
    #[arg(long, help = "Wait until this text appears on screen")]
    text: Option<String>,
    #[arg(long, default_value_t = 30.0, help = "Maximum seconds to wait (default: 30)")]
    timeout: f64,
    #[arg(long, help = "Output as JSON")]
    json: bool,
},
```

- [ ] **Step 4: Change `cli::execute` return type to support typed exit codes**

Replace the return type `Result<Option<String>, String>` with a new enum:

```rust
/// Result of executing a CLI command.
pub enum ExecResult {
    /// Success, optional output to print to stdout.
    Ok(Option<String>),
    /// Error, message to print to stderr. Exit code 1.
    Err(String),
    /// Wait timed out or session gone. Message for stderr, specific exit code.
    Exit { code: i32, message: Option<String>, output: Option<String> },
}
```

Update all existing match arms to return `ExecResult::Ok(...)` or `ExecResult::Err(...)` — mechanical find-and-replace of `Ok(...)` to `ExecResult::Ok(...)` and `Err(...)` to `ExecResult::Err(...)` within the `execute` function.

- [ ] **Step 5: Add the wait execute arm**

```rust
Command::Wait { id, idle_time, text, timeout, json } => {
    if idle_time.is_none() && text.is_none() {
        return ExecResult::Err("at least one of --idle-time or --text is required".to_string());
    }

    let mut conditions = Vec::new();
    if let Some(secs) = idle_time {
        conditions.push(crate::protocol::WaitCondition::OutputIdle {
            quiet_ms: (secs * 1000.0) as u64,
        });
    }
    if let Some(ref t) = text {
        conditions.push(crate::protocol::WaitCondition::TextMatch {
            text: t.clone(),
        });
    }
    let timeout_ms = (timeout * 1000.0) as u64;

    let socket_path = service.session_socket_path(&id)?;
    let mut stream = service.connect_session_socket(&socket_path)?;
    Frame::Wait { conditions, timeout_ms }.write(&mut stream)
        .map_err(|e| format!("write wait request: {e}"))?;
    let result = Frame::read(&mut stream)
        .map_err(|e| format!("read wait response: {e}"))?;

    match result {
        Frame::WaitResult { status, elapsed_ms } => {
            let status_str = match status {
                crate::protocol::WaitStatus::Ready => "ready",
                crate::protocol::WaitStatus::Timeout => "timeout",
                crate::protocol::WaitStatus::SessionGone => "session_gone",
            };
            if json {
                let output = format!(r#"{{"status":"{}","elapsed_ms":{}}}"#, status_str, elapsed_ms);
                match status {
                    crate::protocol::WaitStatus::Ready => ExecResult::Ok(Some(output)),
                    crate::protocol::WaitStatus::Timeout => ExecResult::Exit { code: 1, message: None, output: Some(output) },
                    crate::protocol::WaitStatus::SessionGone => ExecResult::Exit { code: 2, message: None, output: Some(output) },
                }
            } else {
                match status {
                    crate::protocol::WaitStatus::Ready => ExecResult::Ok(None),
                    crate::protocol::WaitStatus::Timeout => ExecResult::Exit { code: 1, message: Some("wait timed out".to_string()), output: None },
                    crate::protocol::WaitStatus::SessionGone => ExecResult::Exit { code: 2, message: Some("session exited while waiting".to_string()), output: None },
                }
            }
        }
        Frame::Error(msg) => ExecResult::Err(msg),
        other => ExecResult::Err(format!("unexpected wait response: {other:?}")),
    }
}
```

Note: this assumes `service.session_socket_path()` and `service.connect_session_socket()` are exposed. If they are private, expose them — they are already used internally by other service methods. Check the actual method signatures when implementing and adapt accordingly.

- [ ] **Step 6: Update main.rs to handle ExecResult**

```rust
use cleat::{cli, server::SessionService};

fn main() {
    let cli = cli::parse();
    let service = if let Some(root) = cli.runtime_root.clone() {
        SessionService::new(cleat::runtime::RuntimeLayout::new(root))
    } else {
        SessionService::discover()
    };
    match cli::execute(cli, &service) {
        cli::ExecResult::Ok(Some(output)) => println!("{output}"),
        cli::ExecResult::Ok(None) => {}
        cli::ExecResult::Err(err) => {
            eprintln!("{err}");
            std::process::exit(1);
        }
        cli::ExecResult::Exit { code, message, output } => {
            if let Some(output) = output {
                println!("{output}");
            }
            if let Some(message) = message {
                eprintln!("{message}");
            }
            std::process::exit(code);
        }
    }
}
```

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test --workspace --locked`
Expected: All tests pass.

- [ ] **Step 8: Run clippy**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`
Expected: No warnings.

- [ ] **Step 9: Commit**

```bash
git add crates/cleat/src/cli.rs crates/cleat/src/main.rs crates/cleat/tests/cli.rs
git commit -m "feat(cli): add wait command with typed exit codes and JSON output"
```

---

### Task 6: Investigate and fix since-marker capture off-by-one

**Files:**
- Modify: `crates/cleat/src/cast_reader.rs` (likely) or `crates/cleat/src/session.rs` (likely)
- Modify: `crates/cleat/tests/cli.rs` or create integration test

This task requires investigation. The steps below are diagnostic — the fix depends on what the investigation reveals.

- [ ] **Step 1: Write a diagnostic test**

This test needs a running daemon, so it should be either an integration test or a manual investigation. Create a script that:

1. Builds cleat with ghostty-vt
2. Creates a session with `--record`
3. Waits for the prompt to settle (sleep 1)
4. Sets a named marker
5. Sends known text: `echo TESTVALUE`
6. Sends Enter
7. Waits (sleep 1)
8. Captures `--since-marker` output
9. Captures `--raw --since-marker` output
10. Dumps the cast file hex around the marker offset

Create `tests/scripts/debug-since-marker.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail

CLEAT="${1:?Usage: $0 <cleat-binary>}"

# Clean up on exit
cleanup() { "$CLEAT" kill test-offbyone 2>/dev/null || true; }
trap cleanup EXIT

"$CLEAT" launch test-offbyone --cmd zsh --cwd /tmp --record --json
sleep 1

OFFSET=$("$CLEAT" mark test-offbyone before-test)
echo "Marker offset: $OFFSET"

"$CLEAT" send test-offbyone 'echo TESTVALUE'
sleep 1

echo "=== capture --since-marker ==="
"$CLEAT" capture test-offbyone --since-marker before-test
echo "=== end ==="

echo "=== capture --since-marker --raw ==="
"$CLEAT" capture test-offbyone --since-marker before-test --raw
echo "=== end ==="

# Find the cast file and dump around the offset
RUNTIME_ROOT="${TMPDIR:-/tmp}/cleat-$(id -u)"
CAST_FILE="$RUNTIME_ROOT/test-offbyone/session.cast"
echo "=== cast file around offset $OFFSET ==="
if [ -f "$CAST_FILE" ]; then
    dd if="$CAST_FILE" bs=1 skip=$((OFFSET > 20 ? OFFSET - 20 : 0)) count=200 2>/dev/null | cat -v
    echo
    echo "=== lines around offset ==="
    head -c $((OFFSET + 200)) "$CAST_FILE" | tail -c $((200 + 20))
fi
```

- [ ] **Step 2: Run the diagnostic script and analyze output**

Run: `DYLD_LIBRARY_PATH=.tools/ghostty-install/lib bash tests/scripts/debug-since-marker.sh target/debug/cleat`

Examine:
- Does the marker offset land at a NDJSON line boundary?
- Is the first event after the marker offset an output event containing the echoed text?
- Does the text start with a duplicated character?
- Are there two separate output events where the split creates the duplication?

- [ ] **Step 3: Identify and fix the root cause**

Based on the investigation, apply the fix. Possible outcomes:

**If the offset is off by one byte:** Fix in `session.rs` — the `bytes_written()` call may need adjustment relative to the flush.

**If the reader seeks to a mid-line position:** Fix in `cast_reader.rs` — `read_events_since` should skip to the next complete line when the offset doesn't land on a line boundary.

**If it's a coalescing race:** Fix in `session.rs` — ensure the event loop doesn't interleave PTY output processing with the mark handler's flush/offset sequence.

- [ ] **Step 4: Run all tests**

Run: `cargo test --workspace --locked`
Expected: All tests pass.

- [ ] **Step 5: Run the diagnostic script again to verify the fix**

Run the same script from Step 2.
Expected: `capture --since-marker` output starts with `echo TESTVALUE` (no duplicated first character).

- [ ] **Step 6: Commit**

```bash
git add -A  # add the changed files identified during investigation
git commit -m "fix(capture): fix since-marker off-by-one in cast reader"
```

---

### Task 7: Final verification

- [ ] **Step 1: Run the full test suite**

Run: `cargo test --workspace --locked`
Expected: All tests pass.

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`
Expected: No warnings.

- [ ] **Step 3: Run fmt check**

Run: `cargo +nightly-2026-03-12 fmt --check`
Expected: No formatting issues.

- [ ] **Step 4: Verify help output end-to-end**

Run:
```bash
cargo run -- --help
cargo run -- launch --help
cargo run -- send --help
cargo run -- send-keys --help
cargo run -- wait --help
cargo run -- interrupt --help
cargo run -- escape --help
```

Verify all commands show descriptions and all arguments show help text.

- [ ] **Step 5: Commit any final fixes**

If any issues were found, fix and commit.
