# Agent UX Improvements Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Split the overloaded `capture` and `wait` commands into screen-based and recording-based pairs (`capture`+`transcript`, `wait`+`expect`), add atomic `--mark-before` to send/send-keys, update help text to reference the new commands, and add a wrapper script for in-repo agents.

**Architecture:** Six tasks across four phases. Phase 1 adds the wrapper script (no code deps). Phase 2 splits `capture` into `capture` + `transcript` and adds `expect`. Phase 3 adds `--mark-before` to `send`/`send-keys`. Phase 4 updates help text (depends on phases 2-3 landing first). Each phase produces working, testable software independently.

**Tech Stack:** Rust, clap, serde, shell scripting

**Spec:** `docs/superpowers/specs/2026-04-02-agent-ux-improvements-design.md`

---

### Task 1: Wrapper script for in-repo agent convenience

**Files:**
- Create: `tools/cleat`

- [ ] **Step 1: Write the wrapper script**

Create `tools/cleat`:

```bash
#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
LIB_DIR="$REPO_ROOT/.tools/ghostty-install/lib"

if [[ ! -d "$LIB_DIR" ]]; then
    echo "error: ghostty libraries not found at $LIB_DIR" >&2
    echo "Run: ./tools/prepare-ghostty-vt.sh" >&2
    exit 1
fi

# Detect platform and set library path
case "$(uname -s)" in
    Darwin) export DYLD_LIBRARY_PATH="${DYLD_LIBRARY_PATH:+$DYLD_LIBRARY_PATH:}$LIB_DIR" ;;
    *)      export LD_LIBRARY_PATH="${LD_LIBRARY_PATH:+$LD_LIBRARY_PATH:}$LIB_DIR" ;;
esac

# Prefer a pre-built binary (release > debug)
for candidate in \
    "$REPO_ROOT/target/release/cleat" \
    "$REPO_ROOT/target/debug/cleat"; do
    if [[ -x "$candidate" ]]; then
        exec "$candidate" "$@"
    fi
done

# Fall back to cargo run
exec cargo run -p cleat --features ghostty-vt -q -- "$@"
```

- [ ] **Step 2: Make executable and test**

Run: `chmod +x tools/cleat && tools/cleat --help 2>&1 | head -5`
Expected: cleat help output (or "ghostty libraries not found" if not built yet).

- [ ] **Step 3: Commit**

```bash
git add tools/cleat
git commit -m "feat: add wrapper script for in-repo agent convenience

Detects platform, sets DYLD_LIBRARY_PATH or LD_LIBRARY_PATH, and
locates a pre-built binary or falls back to cargo run. Agents can
now use ./tools/cleat instead of manual library path setup."
```

---

### Task 2: Add `transcript` command (split recording-delta out of `capture`)

**Files:**
- Modify: `crates/cleat/src/cli.rs`
- Modify: `crates/cleat/tests/cli.rs`

This task moves the `--since`/`--since-marker`/`--raw` flags out of `Capture` into a new `Transcript` variant. The server methods (`capture_since_raw`, `capture_since_text`) are unchanged — only the CLI routing changes.

- [ ] **Step 1: Write tests for `transcript` CLI parsing**

Add to `crates/cleat/tests/cli.rs`, after the existing `capture_since_and_since_marker_are_mutually_exclusive` test:

```rust
#[test]
fn transcript_with_since_marker_parses() {
    let cli = Cli::try_parse_from(["cleat", "transcript", "sess", "--since-marker", "m1"]).expect("parse");
    assert_eq!(cli.command, Command::Transcript {
        id: "sess".into(),
        since: None,
        since_marker: Some("m1".into()),
        raw: false,
    });
}

#[test]
fn transcript_with_since_offset_parses() {
    let cli = Cli::try_parse_from(["cleat", "transcript", "sess", "--since", "500"]).expect("parse");
    assert_eq!(cli.command, Command::Transcript {
        id: "sess".into(),
        since: Some(500),
        since_marker: None,
        raw: false,
    });
}

#[test]
fn transcript_with_raw_parses() {
    let cli = Cli::try_parse_from(["cleat", "transcript", "sess", "--since-marker", "m1", "--raw"]).expect("parse");
    assert_eq!(cli.command, Command::Transcript {
        id: "sess".into(),
        since: None,
        since_marker: Some("m1".into()),
        raw: true,
    });
}

#[test]
fn transcript_requires_since_or_since_marker() {
    let temp = tempfile::tempdir().unwrap();
    let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));
    let cli = Cli::try_parse_from(["cleat", "transcript", "sess"]).expect("parse");
    let result = execute(cli, &service);
    let err = match result {
        ExecResult::Err(e) => e,
        _ => panic!("transcript without --since should fail"),
    };
    assert!(err.contains("--since or --since-marker"));
}

#[test]
fn transcript_since_and_since_marker_are_mutually_exclusive() {
    let result = Cli::try_parse_from(["cleat", "transcript", "sess", "--since", "100", "--since-marker", "m1"]);
    assert!(result.is_err(), "--since and --since-marker should be mutually exclusive");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --workspace --locked transcript_ 2>&1 | tail -5`
Expected: compilation error — `Command::Transcript` does not exist yet.

- [ ] **Step 3: Add `Transcript` variant to `Command` enum**

In `crates/cleat/src/cli.rs`, add the `Transcript` variant after the `Capture` variant (after line 99). Also update `Capture` to remove `--since`, `--since-marker`, and `--raw`:

Replace the existing `Capture` definition (lines 80-99):

```rust
    /// Capture terminal screen content
    #[command(after_long_help = "Returns the current rendered screen from the VT engine.\n\
                           Requires a functional VT engine (not passthrough).\n\
                           \n\
                           For recorded output since a checkpoint, use the transcript command.")]
    Capture {
        id: String,
    },
```

Add `Transcript` right after it:

```rust
    /// Read recorded output since a checkpoint
    #[command(after_long_help = "Returns recorded PTY output after the given byte offset or named\n\
                           marker. Requires an active recording.\n\
                           \n\
                           Use mark to set a named checkpoint, then transcript --since-marker\n\
                           to read output produced after that point.\n\
                           \n\
                           --raw is accepted but currently produces the same output as non-raw.\n\
                           VT-rendered replay for the non-raw path is planned.")]
    Transcript {
        id: String,
        /// Byte offset in .cast file; return output events after this position
        #[arg(long, conflicts_with = "since_marker")]
        since: Option<u64>,
        /// Named marker to use as the start offset
        #[arg(long, conflicts_with = "since")]
        since_marker: Option<String>,
        /// Return raw event data instead of VT-rendered text
        #[arg(long)]
        raw: bool,
    },
```

- [ ] **Step 4: Update `execute` to handle `Transcript` and simplified `Capture`**

In `crates/cleat/src/cli.rs`, replace the `Command::Capture` match arm (lines 293-319):

```rust
        Command::Capture { id } => {
            match service.capture(&id) {
                Ok(s) => ExecResult::Ok(Some(s)),
                Err(e) => ExecResult::Err(e),
            }
        }
        Command::Transcript { id, since, since_marker, raw } => {
            let offset = match (since, &since_marker) {
                (Some(o), _) => Some(o),
                (_, Some(name)) => match service.resolve_marker(&id, name) {
                    Ok(o) => Some(o),
                    Err(e) => return ExecResult::Err(e),
                },
                _ => None,
            };
            match offset {
                Some(o) => {
                    let result = if raw {
                        service.capture_since_raw(&id, o)
                    } else {
                        service.capture_since_text(&id, o)
                    };
                    match result {
                        Ok(s) => ExecResult::Ok(Some(s)),
                        Err(e) => ExecResult::Err(e),
                    }
                }
                None => ExecResult::Err("transcript requires --since or --since-marker".to_string()),
            }
        }
```

- [ ] **Step 5: Update `help_lists_expected_subcommands` test**

In `crates/cleat/tests/cli.rs`, update the expected subcommands list in `help_lists_expected_subcommands` (line 13). Add `"transcript"` after `"capture"`:

```rust
    assert_eq!(subcommands, vec![
        "attach",
        "launch",
        "list",
        "capture",
        "transcript",
        "detach",
        "kill",
        "send-keys",
        "inspect",
        "signal",
        "record",
        "mark",
        "send",
        "interrupt",
        "escape",
        "wait"
    ]);
```

- [ ] **Step 6: Update existing `capture` CLI tests for the simplified command**

In `crates/cleat/tests/cli.rs`, update the `capture_command_parses` test (line 132-135):

```rust
#[test]
fn capture_command_parses() {
    let cli = Cli::try_parse_from(["cleat", "capture", "session-1"]).expect("capture parses");
    assert_eq!(cli.command, Command::Capture { id: "session-1".into() });
}
```

Remove these tests that test flags no longer on `capture`:
- `capture_with_since_flag_parses` (lines 268-271)
- `capture_with_raw_flag_parses` (lines 274-277)
- `capture_without_since_still_works` (lines 280-283) — redundant with the updated `capture_command_parses`
- `capture_raw_without_since_is_rejected` (lines 286-296) — this validation moved to `transcript`
- `capture_with_since_marker_parses` (lines 311-314)
- `capture_since_and_since_marker_are_mutually_exclusive` (lines 317-320) — replaced by `transcript_since_and_since_marker_are_mutually_exclusive`

- [ ] **Step 7: Update `mark` help text to reference `transcript`**

In `crates/cleat/src/cli.rs`, update the `Mark` variant's `after_long_help` (lines 139-141):

```rust
    /// Set a named marker in the recording
    #[command(after_long_help = "Returns the byte offset in the .cast file. Named markers can be\n\
                           used with transcript --since-marker to get output recorded after\n\
                           that point. Requires an active recording.")]
    Mark {
```

- [ ] **Step 8: Update lifecycle test that uses `capture --since`**

In `crates/cleat/tests/lifecycle.rs`, the `detached_session_answers_da_queries` test (line 458) calls `service.capture_since_raw` directly — this is a server method call, not a CLI call, so it does NOT need changing. Verify no lifecycle tests use CLI-level `capture --since`. 

Run: `grep -n 'capture.*since' crates/cleat/tests/lifecycle.rs`
Expected: only `capture_since_raw` method calls, no CLI-level `--since` flags.

- [ ] **Step 9: Run all tests**

Run: `cargo test --workspace --locked 2>&1 | tail -10`
Expected: all pass.

- [ ] **Step 10: Run clippy and fmt**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings 2>&1 | tail -5`
Run: `cargo +nightly-2026-03-12 fmt --check 2>&1 | tail -5`
Expected: clean.

- [ ] **Step 11: Commit**

```bash
git add crates/cleat/src/cli.rs crates/cleat/tests/cli.rs
git commit -m "feat: add transcript command, simplify capture to screen-only

Split capture into two commands:
- capture: returns VT screen snapshot (tmux capture-pane equivalent)
- transcript: reads recorded output since a marker/offset

Agents familiar with tmux get expected screen-snapshot behavior from
capture. Recording-delta workflows use transcript explicitly."
```

---

### Task 3: Add `expect` command (recording-based text wait)

**Files:**
- Modify: `crates/cleat/src/cli.rs`
- Modify: `crates/cleat/src/protocol.rs`
- Modify: `crates/cleat/src/session.rs`
- Modify: `crates/cleat/src/server.rs`
- Modify: `crates/cleat/tests/cli.rs`

`expect` blocks until a text pattern appears in recorded output since a checkpoint. It polls the cast file on each daemon loop tick, reusing the pending-wait infrastructure.

- [ ] **Step 1: Write CLI parsing tests for `expect`**

Add to `crates/cleat/tests/cli.rs`:

```rust
#[test]
fn expect_with_since_marker_parses() {
    let cli = Cli::try_parse_from([
        "cleat", "expect", "sess", "--text", "PASS", "--since-marker", "m1", "--timeout", "10",
    ]).expect("parse");
    assert_eq!(cli.command, Command::Expect {
        id: "sess".into(),
        text: "PASS".into(),
        since: None,
        since_marker: Some("m1".into()),
        timeout: 10.0,
        json: false,
    });
}

#[test]
fn expect_with_since_offset_parses() {
    let cli = Cli::try_parse_from([
        "cleat", "expect", "sess", "--text", "DONE", "--since", "100",
    ]).expect("parse");
    assert_eq!(cli.command, Command::Expect {
        id: "sess".into(),
        text: "DONE".into(),
        since: Some(100),
        since_marker: None,
        timeout: 30.0,
        json: false,
    });
}

#[test]
fn expect_requires_since_or_since_marker() {
    let temp = tempfile::tempdir().unwrap();
    let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));
    let cli = Cli::try_parse_from(["cleat", "expect", "sess", "--text", "PASS"]).expect("parse");
    let result = execute(cli, &service);
    match result {
        ExecResult::Exit { code: 2, message: Some(msg), .. } => {
            assert!(msg.contains("--since or --since-marker"));
        }
        other => panic!("expect without checkpoint should exit 2, got: {other:?}"),
    }
}

#[test]
fn expect_json_flag_parses() {
    let cli = Cli::try_parse_from([
        "cleat", "expect", "sess", "--text", "OK", "--since-marker", "m1", "--json",
    ]).expect("parse");
    assert!(matches!(cli.command, Command::Expect { json: true, .. }));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --workspace --locked expect_ 2>&1 | tail -5`
Expected: compilation error — `Command::Expect` does not exist yet.

- [ ] **Step 3: Add `Expect` variant to `Command` enum**

In `crates/cleat/src/cli.rs`, add after the `Wait` variant (after line 183):

```rust
    /// Wait for text in recorded output since a checkpoint
    #[command(after_long_help = "Edge-triggered text wait: blocks until the given text appears in\n\
                           recorded output after the specified checkpoint. Unlike wait --text,\n\
                           which checks the current VT screen, expect only matches NEW output\n\
                           recorded since the marker.\n\
                           \n\
                           Requires an active recording and --since or --since-marker.\n\
                           \n\
                           Exit codes:\n\
                           \x20 0  Text found\n\
                           \x20 1  Timeout reached\n\
                           \x20 2  Error or session exited\n\
                           \n\
                           JSON output (--json): {\"status\": \"ready|timeout|session_gone\", \"elapsed_ms\": N}")]
    Expect {
        id: String,
        #[arg(long, required = true, help = "Text pattern to search for in recorded output")]
        text: String,
        /// Byte offset in .cast file to start searching from
        #[arg(long, conflicts_with = "since_marker")]
        since: Option<u64>,
        /// Named marker to use as the start offset
        #[arg(long, conflicts_with = "since")]
        since_marker: Option<String>,
        #[arg(long, default_value_t = 30.0, help = "Maximum seconds to wait (default: 30)")]
        timeout: f64,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
```

- [ ] **Step 4: Add `ExpectCondition` to protocol and extend `Frame`**

In `crates/cleat/src/protocol.rs`, add a new tag constant after `TAG_WAIT_RESULT` (line 100):

```rust
const TAG_EXPECT: u8 = 20;
const TAG_EXPECT_RESULT: u8 = 21;
```

Add the frame variants to the `Frame` enum (after `WaitResult`, line 135):

```rust
    Expect { text: String, since_offset: u64, timeout_ms: u64 },
    ExpectResult { status: WaitStatus, elapsed_ms: u64 },
```

Add encoding in `Frame::encode` (after the `WaitResult` arm):

```rust
            Frame::Expect { ref text, since_offset, timeout_ms } => {
                let mut payload = Vec::new();
                payload.extend_from_slice(&timeout_ms.to_le_bytes());
                payload.extend_from_slice(&since_offset.to_le_bytes());
                payload.extend_from_slice(&(text.len() as u32).to_le_bytes());
                payload.extend_from_slice(text.as_bytes());
                (TAG_EXPECT, payload)
            }
            Frame::ExpectResult { status, elapsed_ms } => {
                let mut payload = Vec::with_capacity(9);
                payload.push(*status as u8);
                payload.extend_from_slice(&elapsed_ms.to_le_bytes());
                (TAG_EXPECT_RESULT, payload)
            }
```

Add decoding in `Frame::decode` (before the `_ => Err(...)` arm):

```rust
            TAG_EXPECT => {
                if payload.len() < 20 {
                    return Err(Error::new(ErrorKind::InvalidData, "invalid expect frame: too short"));
                }
                let timeout_ms = u64::from_le_bytes([
                    payload[0], payload[1], payload[2], payload[3],
                    payload[4], payload[5], payload[6], payload[7],
                ]);
                let since_offset = u64::from_le_bytes([
                    payload[8], payload[9], payload[10], payload[11],
                    payload[12], payload[13], payload[14], payload[15],
                ]);
                let text_len = u32::from_le_bytes([
                    payload[16], payload[17], payload[18], payload[19],
                ]) as usize;
                if payload.len() < 20 + text_len {
                    return Err(Error::new(ErrorKind::InvalidData, "invalid expect frame: truncated text"));
                }
                let text = String::from_utf8(payload[20..20 + text_len].to_vec())
                    .map_err(|e| Error::new(ErrorKind::InvalidData, format!("invalid expect text utf-8: {e}")))?;
                Ok(Frame::Expect { text, since_offset, timeout_ms })
            }
            TAG_EXPECT_RESULT => {
                if payload.len() != 9 {
                    return Err(Error::new(ErrorKind::InvalidData, "invalid expect result frame"));
                }
                let status = match payload[0] {
                    0 => WaitStatus::Ready,
                    1 => WaitStatus::Timeout,
                    2 => WaitStatus::SessionGone,
                    _ => return Err(Error::new(ErrorKind::InvalidData, format!("invalid expect status: {}", payload[0]))),
                };
                let elapsed_ms = u64::from_le_bytes([
                    payload[1], payload[2], payload[3], payload[4],
                    payload[5], payload[6], payload[7], payload[8],
                ]);
                Ok(Frame::ExpectResult { status, elapsed_ms })
            }
```

- [ ] **Step 5: Add protocol round-trip tests**

Add to `crates/cleat/src/protocol.rs` tests module:

```rust
    #[test]
    fn expect_round_trip() {
        let frame = Frame::Expect {
            text: "PASS".to_string(),
            since_offset: 12345,
            timeout_ms: 5000,
        };
        let mut bytes = Vec::new();
        frame.write(&mut bytes).expect("write");
        let decoded = Frame::read(&mut bytes.as_slice()).expect("read");
        assert_eq!(decoded, frame);
    }

    #[test]
    fn expect_result_round_trip() {
        let frame = Frame::ExpectResult { status: WaitStatus::Ready, elapsed_ms: 42 };
        let mut bytes = Vec::new();
        frame.write(&mut bytes).expect("write");
        let decoded = Frame::read(&mut bytes.as_slice()).expect("read");
        assert_eq!(decoded, frame);
    }
```

- [ ] **Step 6: Add `expect` server method**

In `crates/cleat/src/server.rs`, add after the `wait` method (after line 332):

```rust
    pub fn expect(
        &self,
        id: &str,
        text: &str,
        since_offset: u64,
        timeout_ms: u64,
    ) -> Result<(crate::protocol::WaitStatus, u64), String> {
        if !self.layout.root().join(id).exists() {
            return Err(format!("missing session {id}"));
        }
        let socket_path = session_socket_path(self.layout.root(), id);
        let mut stream = connect_session_socket(&socket_path)?;
        stream.set_read_timeout(Some(Duration::from_millis(timeout_ms + 5000))).map_err(|err| format!("set read timeout: {err}"))?;
        Frame::Expect { text: text.to_string(), since_offset, timeout_ms }
            .write(&mut stream)
            .map_err(|err| format!("write expect request: {err}"))?;
        match Frame::read(&mut stream).map_err(|err| format!("read expect response: {err}"))? {
            Frame::ExpectResult { status, elapsed_ms } => Ok((status, elapsed_ms)),
            Frame::Error(message) => Err(message),
            other => Err(format!("unexpected expect response: {other:?}")),
        }
    }
```

- [ ] **Step 7: Add daemon-side `Expect` handler and pending-expect infrastructure**

In `crates/cleat/src/session.rs`, add a `PendingExpect` struct after `PendingWait` (line 452):

```rust
struct PendingExpect {
    stream: UnixStream,
    text: String,
    since_offset: u64,
    timeout_ms: u64,
    registered_at: Instant,
}
```

In `run_session_daemon`, add `let mut pending_expects: Vec<PendingExpect> = Vec::new();` next to the existing `let mut pending_waits` declaration (find it by searching for `pending_waits` — it's near line 475).

Add the `Frame::Expect` handler in the accepted-connection match block, after the `Frame::Wait` handler (after line 696):

```rust
                        Ok(Frame::Expect { text, since_offset, timeout_ms }) => 'expect: {
                            if recorder.is_none() {
                                let _ = Frame::Error("recording not active".to_string()).write(&mut stream);
                                break 'expect;
                            }
                            // Check immediately — text may already be in the recording
                            let cast_path = root.join(id).join(crate::recording::CAST_FILE_NAME);
                            if let Some(ref mut rec) = recorder {
                                rec.flush();
                            }
                            if cast_path.exists() {
                                if let Ok(events) = crate::cast_reader::read_output_since(&cast_path, since_offset) {
                                    let output: String = events.iter().map(|e| e.data.as_str()).collect();
                                    if output.contains(&text) {
                                        let _ = Frame::ExpectResult {
                                            status: crate::protocol::WaitStatus::Ready,
                                            elapsed_ms: 0,
                                        }.write(&mut stream);
                                        break 'expect;
                                    }
                                }
                            }
                            if let Err(err) = stream.set_nonblocking(true) {
                                let _ = Frame::Error(format!("set nonblocking: {err}")).write(&mut stream);
                                break 'expect;
                            }
                            pending_expects.push(PendingExpect {
                                stream,
                                text,
                                since_offset,
                                timeout_ms,
                                registered_at: Instant::now(),
                            });
                        }
```

Add the pending-expect evaluation loop after the existing `pending_waits.retain_mut` block (after line 865):

```rust
        // Evaluate pending expects by scanning the cast file for text matches.
        if !pending_expects.is_empty() {
            if let Some(ref mut rec) = recorder {
                rec.flush();
            }
            let cast_path = root.join(id).join(crate::recording::CAST_FILE_NAME);
            pending_expects.retain_mut(|expect| {
                let elapsed = expect.registered_at.elapsed();
                let elapsed_ms = elapsed.as_millis() as u64;

                if elapsed_ms >= expect.timeout_ms {
                    let _ = Frame::ExpectResult {
                        status: crate::protocol::WaitStatus::Timeout,
                        elapsed_ms,
                    }.write(&mut expect.stream);
                    return false;
                }

                if cast_path.exists() {
                    if let Ok(events) = crate::cast_reader::read_output_since(&cast_path, expect.since_offset) {
                        let output: String = events.iter().map(|e| e.data.as_str()).collect();
                        if output.contains(&expect.text) {
                            let _ = Frame::ExpectResult {
                                status: crate::protocol::WaitStatus::Ready,
                                elapsed_ms,
                            }.write(&mut expect.stream);
                            return false;
                        }
                    }
                }

                true
            });
        }
```

Also update the child-exited drain to include pending_expects (line 867-871). After the existing `pending_waits.drain` loop:

```rust
            for mut expect in pending_expects.drain(..) {
                let elapsed_ms = expect.registered_at.elapsed().as_millis() as u64;
                let _ = Frame::ExpectResult { status: crate::protocol::WaitStatus::SessionGone, elapsed_ms }.write(&mut expect.stream);
            }
```

- [ ] **Step 8: Add `execute_expect` function and wire it into `execute`**

In `crates/cleat/src/cli.rs`, add after the `execute_wait` function (after line 476):

```rust
fn execute_expect(
    service: &SessionService,
    id: String,
    text: String,
    since: Option<u64>,
    since_marker: Option<String>,
    timeout: f64,
    json: bool,
) -> ExecResult {
    let offset = match (since, &since_marker) {
        (Some(o), _) => o,
        (_, Some(name)) => match service.resolve_marker(&id, name) {
            Ok(o) => o,
            Err(e) => return ExecResult::Exit { code: 2, message: Some(e), output: None },
        },
        _ => {
            return ExecResult::Exit {
                code: 2,
                message: Some("expect requires --since or --since-marker".to_string()),
                output: None,
            };
        }
    };

    if !timeout.is_finite() || !(0.0..=86_400.0).contains(&timeout) {
        return ExecResult::Exit { code: 2, message: Some(format!("invalid timeout: {timeout} (max 86400)")), output: None };
    }
    let timeout_ms = (timeout * 1000.0) as u64;

    let (status, elapsed_ms) = match service.expect(&id, &text, offset, timeout_ms) {
        Ok(v) => v,
        Err(e) => return ExecResult::Exit { code: 2, message: Some(e), output: None },
    };

    match status {
        WaitStatus::Ready => {
            if json {
                ExecResult::Ok(Some(format!(r#"{{"status":"ready","elapsed_ms":{elapsed_ms}}}"#)))
            } else {
                ExecResult::Ok(None)
            }
        }
        WaitStatus::Timeout => {
            if json {
                ExecResult::Exit { code: 1, message: None, output: Some(format!(r#"{{"status":"timeout","elapsed_ms":{elapsed_ms}}}"#)) }
            } else {
                ExecResult::Exit { code: 1, message: Some("expect timed out".to_string()), output: None }
            }
        }
        WaitStatus::SessionGone => {
            if json {
                ExecResult::Exit {
                    code: 2,
                    message: None,
                    output: Some(format!(r#"{{"status":"session_gone","elapsed_ms":{elapsed_ms}}}"#)),
                }
            } else {
                ExecResult::Exit { code: 2, message: Some("session exited while waiting".to_string()), output: None }
            }
        }
    }
}
```

In the `execute` function's match block, add the `Expect` arm (before `Command::Serve`):

```rust
        Command::Expect { id, text, since, since_marker, timeout, json } => {
            execute_expect(service, id, text, since, since_marker, timeout, json)
        }
```

- [ ] **Step 9: Update `help_lists_expected_subcommands` test**

In `crates/cleat/tests/cli.rs`, add `"expect"` after `"wait"` in the expected subcommands list:

```rust
    assert_eq!(subcommands, vec![
        "attach",
        "launch",
        "list",
        "capture",
        "transcript",
        "detach",
        "kill",
        "send-keys",
        "inspect",
        "signal",
        "record",
        "mark",
        "send",
        "interrupt",
        "escape",
        "wait",
        "expect"
    ]);
```

- [ ] **Step 10: Run all tests**

Run: `cargo test --workspace --locked 2>&1 | tail -10`
Expected: all pass.

- [ ] **Step 11: Run clippy and fmt**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings 2>&1 | tail -5`
Run: `cargo +nightly-2026-03-12 fmt --check 2>&1 | tail -5`
Expected: clean.

- [ ] **Step 12: Commit**

```bash
git add crates/cleat/src/cli.rs crates/cleat/src/protocol.rs crates/cleat/src/session.rs crates/cleat/src/server.rs crates/cleat/tests/cli.rs
git commit -m "feat: add expect command for edge-triggered recording text wait

expect blocks until text appears in recorded output after a checkpoint.
Unlike wait --text (which checks the current VT screen), expect only
matches new output since the specified marker. This gives agents a
reliable edge-triggered text wait for recording-based workflows."
```

---

### Task 4: Add `--mark-before` to `send` and `send-keys`

**Files:**
- Modify: `crates/cleat/src/cli.rs`
- Modify: `crates/cleat/src/protocol.rs`
- Modify: `crates/cleat/src/session.rs`
- Modify: `crates/cleat/src/server.rs`
- Modify: `crates/cleat/tests/cli.rs`

- [ ] **Step 1: Write CLI parsing tests**

Add to `crates/cleat/tests/cli.rs`:

```rust
#[test]
fn send_mark_before_parses() {
    let cli = Cli::try_parse_from(["cleat", "send", "--mark-before", "m1", "sess", "echo hi"]).expect("parse");
    assert_eq!(cli.command, Command::Send {
        id: "sess".into(),
        text: "echo hi".into(),
        no_enter: false,
        mark_before: Some("m1".into()),
    });
}

#[test]
fn send_keys_mark_before_parses() {
    let cli = Cli::try_parse_from(["cleat", "send-keys", "--mark-before", "m1", "sess", "Enter"]).expect("parse");
    assert_eq!(cli.command, Command::SendKeys {
        id: "sess".into(),
        literal: false,
        hex: false,
        repeat: 1,
        keys: vec!["Enter".into()],
        mark_before: Some("m1".into()),
    });
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --workspace --locked mark_before 2>&1 | tail -5`
Expected: compilation error — `mark_before` field does not exist.

- [ ] **Step 3: Add `mark_before` field to `Send` and `SendKeys`**

In `crates/cleat/src/cli.rs`, add to the `Send` variant (after line 154):

```rust
        #[arg(long, value_name = "NAME", help = "Set a named marker before sending (requires recording)")]
        mark_before: Option<String>,
```

Add to the `SendKeys` variant (after line 118):

```rust
        #[arg(long, value_name = "NAME", help = "Set a named marker before sending (requires recording)")]
        mark_before: Option<String>,
```

- [ ] **Step 4: Add `SendKeysWithMark` frame variant**

In `crates/cleat/src/protocol.rs`, add a new tag (after `TAG_EXPECT_RESULT`):

```rust
const TAG_SEND_KEYS_WITH_MARK: u8 = 22;
```

Add the frame variant to the `Frame` enum:

```rust
    SendKeysWithMark { bytes: Vec<u8>, marker_name: String },
```

Add encoding:

```rust
            Frame::SendKeysWithMark { ref bytes, ref marker_name } => {
                let mut payload = Vec::new();
                payload.extend_from_slice(&(marker_name.len() as u32).to_le_bytes());
                payload.extend_from_slice(marker_name.as_bytes());
                payload.extend_from_slice(bytes);
                (TAG_SEND_KEYS_WITH_MARK, payload)
            }
```

Add decoding:

```rust
            TAG_SEND_KEYS_WITH_MARK => {
                if payload.len() < 4 {
                    return Err(Error::new(ErrorKind::InvalidData, "invalid send-keys-with-mark frame: too short"));
                }
                let name_len = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
                if payload.len() < 4 + name_len {
                    return Err(Error::new(ErrorKind::InvalidData, "invalid send-keys-with-mark frame: truncated name"));
                }
                let marker_name = String::from_utf8(payload[4..4 + name_len].to_vec())
                    .map_err(|e| Error::new(ErrorKind::InvalidData, format!("invalid marker name utf-8: {e}")))?;
                let bytes = payload[4 + name_len..].to_vec();
                Ok(Frame::SendKeysWithMark { bytes, marker_name })
            }
```

- [ ] **Step 5: Add protocol round-trip test**

Add to protocol tests:

```rust
    #[test]
    fn send_keys_with_mark_round_trip() {
        let frame = Frame::SendKeysWithMark {
            bytes: b"hello\r".to_vec(),
            marker_name: "m1".to_string(),
        };
        let mut bytes = Vec::new();
        frame.write(&mut bytes).expect("write");
        let decoded = Frame::read(&mut bytes.as_slice()).expect("read");
        assert_eq!(decoded, frame);
    }
```

- [ ] **Step 6: Add `send_keys_with_mark` server method**

In `crates/cleat/src/server.rs`, add after `send_keys`:

```rust
    pub fn send_keys_with_mark(&self, id: &str, bytes: &[u8], marker_name: &str) -> Result<u64, String> {
        if !self.layout.root().join(id).exists() {
            return Err(format!("missing session {id}"));
        }
        let socket_path = session_socket_path(self.layout.root(), id);
        let mut stream = connect_session_socket(&socket_path)?;
        Frame::SendKeysWithMark { bytes: bytes.to_vec(), marker_name: marker_name.to_string() }
            .write(&mut stream)
            .map_err(|err| format!("write send-keys-with-mark request: {err}"))?;
        match Frame::read(&mut stream).map_err(|err| format!("read send-keys-with-mark response: {err}"))? {
            Frame::MarkResult { offset } => Ok(offset),
            Frame::Error(message) => Err(message),
            other => Err(format!("unexpected send-keys-with-mark response: {other:?}")),
        }
    }
```

- [ ] **Step 7: Add daemon-side handler**

In `crates/cleat/src/session.rs`, add after the `Frame::SendKeys` handler (after line 559):

```rust
                        Ok(Frame::SendKeysWithMark { bytes, marker_name }) => {
                            if let Some(ref mut rec) = recorder {
                                rec.flush();
                                rec.event(crate::asciicast::EventCode::Marker, &marker_name, epoch.elapsed());
                                let offset = rec.bytes_written();
                                markers.insert(marker_name, offset);
                                rec.input(&bytes, epoch.elapsed());
                                if let Err(err) = write_fd_all(pty_fd, &bytes) {
                                    let _ = Frame::Error(err).write(&mut stream);
                                } else {
                                    let _ = Frame::MarkResult { offset }.write(&mut stream);
                                }
                            } else {
                                let _ = Frame::Error("recording not active".to_string()).write(&mut stream);
                            }
                        }
```

- [ ] **Step 8: Update `execute` to use `send_keys_with_mark`**

In `crates/cleat/src/cli.rs`, update the `Command::Send` match arm (lines 381-390):

```rust
        Command::Send { id, text, no_enter, mark_before } => {
            let mut bytes = text.into_bytes();
            if !no_enter {
                bytes.push(b'\r');
            }
            if let Some(marker_name) = mark_before {
                match service.send_keys_with_mark(&id, &bytes, &marker_name) {
                    Ok(offset) => ExecResult::Ok(Some(offset.to_string())),
                    Err(e) => ExecResult::Err(e),
                }
            } else {
                match service.send_keys(&id, &bytes) {
                    Ok(()) => ExecResult::Ok(None),
                    Err(e) => ExecResult::Err(e),
                }
            }
        }
```

Update the `Command::SendKeys` match arm (lines 329-338):

```rust
        Command::SendKeys { id, literal, hex, repeat, keys, mark_before } => {
            let bytes = match encode_send_keys(&keys, literal, hex, repeat) {
                Ok(v) => v,
                Err(e) => return ExecResult::Err(e),
            };
            if let Some(marker_name) = mark_before {
                match service.send_keys_with_mark(&id, &bytes, &marker_name) {
                    Ok(offset) => ExecResult::Ok(Some(offset.to_string())),
                    Err(e) => ExecResult::Err(e),
                }
            } else {
                match service.send_keys(&id, &bytes) {
                    Ok(()) => ExecResult::Ok(None),
                    Err(e) => ExecResult::Err(e),
                }
            }
        }
```

- [ ] **Step 9: Fix existing CLI tests that construct `Send`/`SendKeys` variants**

All existing tests that construct `Command::Send` or `Command::SendKeys` in assertions need the new `mark_before: None` field. Update each one. Search for `Command::Send {` and `Command::SendKeys {` in `crates/cleat/tests/cli.rs` and add `mark_before: None` to the struct literals.

For example, `send_command_parses` becomes:
```rust
    assert_eq!(cli.command, Command::Send { id: "demo".into(), text: "echo hello".into(), no_enter: false, mark_before: None });
```

And `send_keys_command_parses` becomes:
```rust
    assert_eq!(cli.command, Command::SendKeys { id: "demo".into(), literal: false, hex: false, repeat: 1, keys: vec!["Enter".into()], mark_before: None });
```

Apply the same to: `send_command_parses_no_enter`, `send_keys_command_parses_literal_mode`, `send_keys_command_parses_hex_mode`, `send_keys_command_parses_repeat`.

Also update the `send_keys_execute_reports_missing_session` test that constructs the variant directly.

- [ ] **Step 10: Run all tests**

Run: `cargo test --workspace --locked 2>&1 | tail -10`
Expected: all pass.

- [ ] **Step 11: Run clippy and fmt**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings 2>&1 | tail -5`
Run: `cargo +nightly-2026-03-12 fmt --check 2>&1 | tail -5`
Expected: clean.

- [ ] **Step 12: Commit**

```bash
git add crates/cleat/src/cli.rs crates/cleat/src/protocol.rs crates/cleat/src/session.rs crates/cleat/src/server.rs crates/cleat/tests/cli.rs
git commit -m "feat: add --mark-before to send and send-keys

Atomically places a named marker before writing to the PTY in a single
daemon round-trip. Eliminates the race condition between separate mark
and send calls when agents pipeline operations."
```

---

### Task 5: Update help text (depends on tasks 2-4)

**Files:**
- Modify: `crates/cleat/src/cli.rs`
- Modify: `crates/cleat/tests/cli.rs`

- [ ] **Step 1: Update top-level command help and `command()` builder**

The challenge: `BUILD_SUPPORT_MESSAGE` is a `const &str` and `concat!` won't work across `const` references in clap derive attributes. Solution: put the workflow snippet in both `after_help` and `after_long_help` via attributes (duplicated, but just 5 lines), then use the `command()` function to append `BUILD_SUPPORT_MESSAGE` at runtime.

In `crates/cleat/src/cli.rs`, replace the `#[command]` attribute on `Cli` (lines 13-25):

```rust
#[derive(Debug, Parser)]
#[command(
    name = "cleat",
    version,
    about = "Session daemon with a structured control plane for agents and terminal persistence",
    after_help = "Typical agent workflow:\n\
                  \x20 cleat launch --record my-session --cmd bash\n\
                  \x20 cleat send my-session 'make test' --mark-before m1\n\
                  \x20 cleat wait my-session --idle-time 2\n\
                  \x20 cleat transcript my-session --since-marker m1\n\
                  \x20 cleat kill my-session",
    after_long_help = "Typical agent workflow:\n\
                       \x20 cleat launch --record my-session --cmd bash\n\
                       \x20 cleat send my-session 'make test' --mark-before m1\n\
                       \x20 cleat wait my-session --idle-time 2\n\
                       \x20 cleat transcript my-session --since-marker m1\n\
                       \x20 cleat kill my-session"
)]
pub struct Cli {
```

Update `command()` (line 203) to append the build message to long help at runtime:

```rust
pub fn command() -> clap::Command {
    let cmd = Cli::command();
    let existing = cmd.get_after_long_help().map(|s| s.to_string()).unwrap_or_default();
    let combined = format!("{existing}\n\n{}", crate::vt::BUILD_SUPPORT_MESSAGE);
    cmd.after_long_help(combined)
}
```

Update `parse()` (line 199) to use our augmented command so `--help` renders correctly at runtime:

```rust
pub fn parse() -> Cli {
    Cli::from_arg_matches(&command().get_matches())
        .expect("clap arg parsing should not fail after get_matches succeeds")
}
```

- [ ] **Step 3: Update `wait` help text to document screen-state semantics**

In `crates/cleat/src/cli.rs`, update the `Wait` variant's `after_long_help`:

```rust
    /// Wait for a condition before continuing
    #[command(after_long_help = "Conditions (OR semantics — any match wins):\n\
                           \x20 --idle-time N  Wait until no PTY output for N seconds\n\
                           \x20 --text STR     Wait until STR appears on the VT screen\n\
                           \n\
                           At least one of --idle-time or --text is required.\n\
                           \n\
                           NOTE: --text matches against the current VT screen state. If the\n\
                           text is already visible when wait is called, it returns immediately.\n\
                           For edge-triggered text matching on new output, use the expect\n\
                           command with --since-marker.\n\
                           \n\
                           Exit codes:\n\
                           \x20 0  Condition met (ready)\n\
                           \x20 1  Timeout reached\n\
                           \x20 2  Error or session exited\n\
                           \n\
                           JSON output (--json): {\"status\": \"ready|timeout|session_gone\", \"elapsed_ms\": N}")]
    Wait {
```

- [ ] **Step 4: Update `help_surfaces_vt_support_policy` test**

In `crates/cleat/tests/cli.rs`, the `help_surfaces_vt_support_policy` test (line 34) calls `write_long_help` and checks for `BUILD_SUPPORT_MESSAGE`. Since we now build the long help at runtime via `command()`, update the test to use `cli::command()` instead of `Cli::command()`:

```rust
#[test]
fn help_surfaces_vt_support_policy() {
    let mut command = cli::command();
    let mut buffer = Vec::new();
    command.write_long_help(&mut buffer).expect("write help");
    let help = String::from_utf8(buffer).expect("help utf8");

    assert!(help.contains("Ghostty is currently the only functional VT engine"));
    assert!(help.contains(vt::BUILD_SUPPORT_MESSAGE));
    assert!(help.contains("Typical agent workflow"));

    let mut launch = cli::command().find_subcommand_mut("launch").expect("launch command").clone();
    let mut launch_buffer = Vec::new();
    launch.write_long_help(&mut launch_buffer).expect("write launch help");
    let launch_help = String::from_utf8(launch_buffer).expect("launch help utf8");
    assert!(launch_help.contains("placeholder engines are for testing/development only"));
}
```

Update the import at the top of `cli.rs` tests if needed — `cli` should already be in scope from the existing imports.

- [ ] **Step 5: Verify help text renders correctly**

Run: `cargo run --locked -- --help 2>&1 | head -20`
Run: `cargo run --locked -- -h 2>&1 | head -20`
Run: `cargo run --locked -- wait --help 2>&1`
Run: `cargo run --locked -- capture --help 2>&1`
Run: `cargo run --locked -- transcript --help 2>&1`
Run: `cargo run --locked -- expect --help 2>&1`
Expected: both `-h` and `--help` show the workflow snippet. `--help` also shows the build message. `wait --help` includes the NOTE about screen-state semantics.

- [ ] **Step 6: Run all tests**

Run: `cargo test --workspace --locked 2>&1 | tail -10`
Expected: all pass.

- [ ] **Step 7: Run clippy and fmt**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings 2>&1 | tail -5`
Run: `cargo +nightly-2026-03-12 fmt --check 2>&1 | tail -5`
Expected: clean.

- [ ] **Step 8: Commit**

```bash
git add crates/cleat/src/cli.rs crates/cleat/tests/cli.rs
git commit -m "docs: update help text for new command structure

Both -h and --help now show the agent workflow snippet. --help adds
the build message. wait --text documents screen-state semantics and
points to expect for edge-triggered matching."
```

---

### Task 6: File GitHub issues for all items

**Files:** none (GitHub operations only)

- [ ] **Step 1: File issues for the exploratory items**

Create two GitHub issues for the deferred items:

```bash
gh issue create --title "Investigate batch/pipeline primitive for atomic multi-step operations" \
  --label "enhancement" --label "agent" \
  --body "Agents orchestrate multi-step sequences (mark, send, wait, transcript) that could benefit from atomic execution. A convenience command was considered but introduces naming/hygiene issues with internal markers.

Possible directions:
- A batch command accepting a sequence of operations
- Auto-scoped markers with private names dropped on completion
- Daemon-side compound operations using byte offsets without named markers

Deferred until real usage patterns emerge from the capture/transcript + wait/expect split.

Ref: docs/superpowers/specs/2026-04-02-agent-ux-improvements-design.md (item 7)"
```

```bash
gh issue create --title "Agent-friendly binary distribution beyond in-repo convenience" \
  --label "enhancement" --label "agent" \
  --body "The ./tools/cleat wrapper addresses in-repo agents, but agents outside the repo still can't easily get a working cleat binary. tmux is typically available system-wide; cleat requires building from source with Ghostty.

Possible directions:
- Prebuilt binaries for Linux/macOS
- Homebrew formula
- Static linking to avoid runtime library path requirements

Depends on the Ghostty dependency story stabilizing.

Ref: docs/superpowers/specs/2026-04-02-agent-ux-improvements-design.md (item 8)"
```

- [ ] **Step 2: Commit the plan**

```bash
git add docs/superpowers/plans/2026-04-02-agent-ux-improvements.md
git commit -m "docs: add agent UX improvements implementation plan"
```
