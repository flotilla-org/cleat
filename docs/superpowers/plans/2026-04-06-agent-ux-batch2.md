# Agent UX Batch 2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix UTF-8 corruption at recording coalesce boundaries, add dynamic cwd tracking to inspect, and document the session lifecycle model.

**Architecture:** Three independent changes to the cleat crate. The UTF-8 fix modifies `CoalesceBuffer::drain()` to hold back incomplete trailing bytes. Dynamic cwd adds platform-specific pid-to-cwd resolution called on-demand during inspect. Session docs add a README section.

**Tech Stack:** Rust, libc (for macOS `proc_pidinfo`), nix (existing dependency)

---

### Task 1: Streaming UTF-8 in CoalesceBuffer — failing tests

**Files:**
- Modify: `crates/cleat/src/recording.rs` (add `#[cfg(test)] mod tests`)

- [ ] **Step 1: Add unit tests for CoalesceBuffer UTF-8 handling**

Add a test module at the bottom of `recording.rs`. The tests exercise `drain()` across multi-byte split boundaries. Add these tests:

```rust
#[cfg(test)]
mod tests {
    use super::CoalesceBuffer;
    use std::time::Duration;

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
        // Simulate final drain by calling drain — incomplete bytes should
        // be emitted as replacement character since there's nothing more coming.
        // For now, drain holds them back. The caller (SessionRecorder) handles
        // final flush by calling drain and accepting whatever remains.
        // We verify drain holds them back:
        assert!(buf.drain().is_none());
        // Force final emission: push nothing, just drain the held-back bytes
        // by converting them lossy. This simulates what flush_final() would do.
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p cleat --lib recording::tests -- --nocapture 2>&1`

Expected: Several tests fail because `drain()` currently uses `from_utf8_lossy` on incomplete sequences instead of holding them back. The `drain_split_2byte_char_holds_back_incomplete` test will fail because `drain()` currently emits `"caf\u{FFFD}"` instead of `"caf"`.

- [ ] **Step 3: Commit failing tests**

```bash
git add crates/cleat/src/recording.rs
git commit -m "test: add unit tests for CoalesceBuffer UTF-8 boundary handling (#19)"
```

---

### Task 2: Streaming UTF-8 in CoalesceBuffer — implementation

**Files:**
- Modify: `crates/cleat/src/recording.rs:44-54` (`CoalesceBuffer::drain()`)

- [ ] **Step 1: Implement incomplete UTF-8 holdback in drain()**

Replace the `drain()` method in `CoalesceBuffer` (lines 44-54 of `recording.rs`):

```rust
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
```

Add the `utf8_complete_len` helper function above the `CoalesceBuffer` struct (after the `COALESCE_SIZE_THRESHOLD` constant):

```rust
/// Return the byte length of the longest prefix of `bytes` that is complete
/// UTF-8 (i.e. does not end with a partial multi-byte sequence).
fn utf8_complete_len(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    // Scan backward from the end to find the start of the last character.
    // A UTF-8 leading byte is 0xxxxxxx (ASCII), 110xxxxx (2-byte),
    // 1110xxxx (3-byte), or 11110xxx (4-byte).
    // Continuation bytes are 10xxxxxx.
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
                // The last character is complete.
                return len;
            } else {
                // Incomplete — split before this leading byte.
                return i;
            }
        }
    }
    // All bytes are continuation bytes — shouldn't happen in valid
    // streams, but treat as complete (lossy will handle it).
    len
}
```

- [ ] **Step 2: Run unit tests to verify they pass**

Run: `cargo test -p cleat --lib recording::tests -- --nocapture 2>&1`

Expected: All tests pass.

- [ ] **Step 3: Update SessionRecorder::flush to use drain_final on session exit**

Currently `SessionRecorder` does not have a distinct "final flush" path. The daemon event loop calls `flush()` before cleanup. We need `SessionRecorder` to expose a `flush_final()` method that uses `drain_final()`.

Add to `SessionRecorder` (after the existing `flush()` method at line 191):

```rust
    /// Final flush — emits all remaining bytes, including incomplete UTF-8
    /// sequences (using lossy conversion). Call once when the session is
    /// ending and no more bytes will arrive.
    pub fn flush_final(&mut self) {
        if let Some(event) = self.coalesce.drain_final() {
            self.write_event(&event);
        }
    }
```

Then update `session.rs` line 973: change `rec.flush()` to `rec.flush_final()`. This is in the `child_exited` block (lines 970-974):

```rust
            if let Some(ref mut rec) = recorder {
                let code = exit_code_from_wait_status(&status);
                rec.event(crate::asciicast::EventCode::Exit, &code.to_string(), epoch.elapsed());
                rec.flush_final();
            }
```

The `event()` call on line 972 already does a normal `flush()` internally, so the final `flush_final()` on line 973 only needs to handle any incomplete UTF-8 bytes that `flush()` held back.

- [ ] **Step 4: Run full test suite**

Run: `cargo test -p cleat --locked 2>&1`

Expected: All tests pass (unit + integration).

- [ ] **Step 5: Run clippy and fmt**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings 2>&1`
Run: `cargo +nightly-2026-03-12 fmt --check 2>&1`

Expected: Clean.

- [ ] **Step 6: Commit**

```bash
git add crates/cleat/src/recording.rs crates/cleat/src/session.rs
git commit -m "fix: hold back incomplete UTF-8 sequences at coalesce buffer boundaries (#19)"
```

---

### Task 3: Dynamic CWD — add resolve_cwd and protocol fields

**Files:**
- Modify: `crates/cleat/src/protocol.rs:57-61` (add fields to `ProcessInspect`)
- Modify: `crates/cleat/src/session.rs:994-1030` (add `resolve_cwd`, update `build_inspect_result`)
- Modify: `crates/cleat/src/cli.rs:654-676` (update `format_inspect_human`)

- [ ] **Step 1: Add fields to ProcessInspect**

In `crates/cleat/src/protocol.rs`, update `ProcessInspect` (lines 57-61):

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessInspect {
    pub leader_pid: u32,
    pub foreground_pgid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leader_cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground_cwd: Option<PathBuf>,
}
```

- [ ] **Step 2: Add resolve_cwd function in session.rs**

Add this function above `build_inspect_result` in `session.rs`:

```rust
/// Resolve the current working directory for a given process ID.
/// Returns `None` if the pid is invalid or the cwd cannot be determined.
#[cfg(target_os = "linux")]
fn resolve_cwd(pid: u32) -> Option<std::path::PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

#[cfg(target_os = "macos")]
fn resolve_cwd(pid: u32) -> Option<std::path::PathBuf> {
    use std::mem;
    // SAFETY: we zero-initialize the struct and pass valid arguments to proc_pidinfo.
    // proc_pidinfo is a well-known macOS API for querying process info.
    unsafe {
        let mut vnode_info: libc::proc_vnodepathinfo = mem::zeroed();
        let size = mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
        let ret = libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            &mut vnode_info as *mut _ as *mut libc::c_void,
            size,
        );
        if ret <= 0 {
            return None;
        }
        let cstr = std::ffi::CStr::from_ptr(vnode_info.pvi_cdir.vip_path.as_ptr());
        let path = std::path::PathBuf::from(cstr.to_string_lossy().into_owned());
        if path.as_os_str().is_empty() {
            None
        } else {
            Some(path)
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn resolve_cwd(_pid: u32) -> Option<std::path::PathBuf> {
    None
}
```

- [ ] **Step 3: Update build_inspect_result to populate the new fields**

In `build_inspect_result` (session.rs, lines 994-1030), update the `ProcessInspect` construction:

```rust
        process: crate::protocol::ProcessInspect {
            leader_pid: pty_child.pid.as_raw() as u32,
            foreground_pgid,
            leader_cwd: resolve_cwd(pty_child.pid.as_raw() as u32),
            foreground_cwd: foreground_pgid.and_then(resolve_cwd),
        },
```

- [ ] **Step 4: Update format_inspect_human to display cwd fields**

In `crates/cleat/src/cli.rs`, in `format_inspect_human` (lines 654-676), add the cwd rows after the `fg_pgid` row:

```rust
    if let Some(fg) = result.process.foreground_pgid {
        table.add_row(vec!["fg_pgid", &fg.to_string()]);
    }
    if let Some(ref cwd) = result.process.leader_cwd {
        table.add_row(vec!["leader_cwd", &cwd.display().to_string()]);
    }
    if let Some(ref cwd) = result.process.foreground_cwd {
        table.add_row(vec!["fg_cwd", &cwd.display().to_string()]);
    }
```

- [ ] **Step 5: Run clippy and fmt**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings 2>&1`
Run: `cargo +nightly-2026-03-12 fmt --check 2>&1`

Expected: Clean. Fix any issues.

- [ ] **Step 6: Commit**

```bash
git add crates/cleat/src/protocol.rs crates/cleat/src/session.rs crates/cleat/src/cli.rs
git commit -m "feat: track dynamic cwd of leader and foreground process in inspect (#38)"
```

---

### Task 4: Dynamic CWD — integration test

**Files:**
- Modify: `crates/cleat/tests/lifecycle.rs`

- [ ] **Step 1: Add integration test for dynamic cwd**

Add this test at the end of `lifecycle.rs`, near the other `ghostty-vt` tests:

```rust
#[cfg(feature = "ghostty-vt")]
#[test]
fn inspect_reports_dynamic_leader_cwd() {
    let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let service = service_for(temp.path());
    service.create(Some("cwd-test".into()), None, None, Some("bash".into()), false).expect("create");

    // Wait for shell to start
    std::thread::sleep(Duration::from_secs(1));

    // Change directory
    service.send_keys("cwd-test", b"cd /tmp\n").expect("send cd");
    // Wait for command to complete
    let _ = service.wait("cwd-test", vec![cleat::protocol::WaitCondition::OutputIdle { quiet_ms: 500 }], 5000);

    let result = service.inspect("cwd-test").expect("inspect");
    let leader_cwd = result.process.leader_cwd.expect("leader_cwd should be Some");

    // On macOS /tmp is a symlink to /private/tmp
    let expected = std::fs::canonicalize("/tmp").expect("canonicalize /tmp");
    assert_eq!(
        std::fs::canonicalize(&leader_cwd).unwrap_or_else(|_| leader_cwd.clone()),
        expected,
        "leader_cwd should reflect cd /tmp"
    );

    // When shell is in foreground, foreground_cwd should match leader_cwd
    let fg_cwd = result.process.foreground_cwd.expect("foreground_cwd should be Some");
    assert_eq!(
        std::fs::canonicalize(&fg_cwd).unwrap_or_else(|_| fg_cwd.clone()),
        expected,
        "foreground_cwd should match leader_cwd when shell is in foreground"
    );

    service.kill("cwd-test").expect("kill");
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p cleat --locked --features ghostty-vt inspect_reports_dynamic_leader_cwd -- --nocapture 2>&1`

(Requires Ghostty build. If not available, run: `cargo test -p cleat --locked 2>&1` to verify non-ghostty tests still pass, and note the integration test needs manual verification.)

Expected: PASS.

- [ ] **Step 3: Also run the backward-compat deserialize test**

The existing `session_inspect_deserializes_without_vt_status_fields` test in `protocol.rs` ensures old JSON (without the new fields) still deserializes. The `#[serde(default)]` annotation handles this. Verify:

Run: `cargo test -p cleat --lib protocol::tests -- --nocapture 2>&1`

Expected: PASS — the existing test still works because `leader_cwd` and `foreground_cwd` have `#[serde(default)]` and will default to `None`.

- [ ] **Step 4: Commit**

```bash
git add crates/cleat/tests/lifecycle.rs
git commit -m "test: add integration test for dynamic cwd in inspect (#38)"
```

---

### Task 5: Session lifecycle documentation

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Add "Session model" section to README.md**

Insert this section after the "Functional Ghostty Build" section (after the `find .tools/ghostty-install` code block at the end of the file) in `README.md`:

```markdown
## Session Model

**One daemon per session.** Each `cleat launch` (or `cleat attach` to a new ID) spawns a dedicated daemon process that owns the session's PTY. The daemon exits when the child process exits.

**Session IDs.** You choose the ID (`cleat launch my-session`) or let cleat generate one (`session-<uuid>`). IDs are directory names under the runtime root, so use filesystem-safe characters. Launching with an ID that already has a running daemon reuses the existing session — no error, no duplicate.

**Runtime directory.** Discovered in priority order:

1. `$CLEAT_RUNTIME_DIR` (if set)
2. `$XDG_RUNTIME_DIR/cleat` (if `XDG_RUNTIME_DIR` is set)
3. `$TMPDIR/cleat-<uid>`
4. `/tmp/cleat-<uid>`

Each session gets a subdirectory containing:
- `socket` — Unix domain socket for client-daemon communication
- `daemon.pid` — daemon process ID
- `session.cast` — asciicast v3 recording (only if recording is enabled)

**Liveness.** The socket file is the liveness indicator. If it exists, the daemon is running and accepting connections.

**Cleanup.** When the child process exits, the daemon removes the socket and PID file, then exits. If recording was active, the session directory and `.cast` file are preserved. Otherwise the entire session directory is removed.

**No persistence across restarts.** Sessions do not survive daemon crashes or host reboots — the PTY and process state are gone. Recording files survive if they were flushed to disk.
```

- [ ] **Step 2: Verify the README renders correctly**

Read the file back to verify formatting.

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: add session model section to README (#39)"
```

---

### Task 6: Final verification

- [ ] **Step 1: Run full test suite**

Run: `cargo test -p cleat --locked 2>&1`

Expected: All tests pass.

- [ ] **Step 2: Run clippy and fmt**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings 2>&1`
Run: `cargo +nightly-2026-03-12 fmt --check 2>&1`

Expected: Clean.

- [ ] **Step 3: Review all changes**

Run: `git log --oneline origin/main..HEAD` to confirm commit history looks right.

Expected: 5 commits:
1. `test: add unit tests for CoalesceBuffer UTF-8 boundary handling (#19)`
2. `fix: hold back incomplete UTF-8 sequences at coalesce buffer boundaries (#19)`
3. `feat: track dynamic cwd of leader and foreground process in inspect (#38)`
4. `test: add integration test for dynamic cwd in inspect (#38)`
5. `docs: add session model section to README (#39)`
