# Agent UX Improvements Batch 2 — Design Spec

Date: 2026-04-06

## Context

Follows the first agent UX round (PR #36, design spec 2026-04-02). A second
agent usability evaluation surfaced three improvements that can be addressed
without external dependencies: a recording correctness fix, a missing inspect
capability, and missing operational documentation.

## 1. Streaming UTF-8 in CoalesceBuffer (#19)

### Problem

`CoalesceBuffer::drain()` calls `String::from_utf8_lossy(&self.bytes)`, which
replaces incomplete trailing multi-byte sequences with U+FFFD. If a multi-byte
UTF-8 character arrives split across two PTY reads and the buffer flushes
between them, the character is permanently corrupted in the recording.

Flush triggers that can cause this: type change (input vs output), the 4KB size
threshold, and explicit flushes from `event()`, `pause()`, `emit_gap()`, and
`write_snapshot()`.

### Change

Modify `drain()` to hold back incomplete trailing bytes:

1. Scan backward from the end of `self.bytes` to find any incomplete UTF-8
   sequence — a leading byte (2/3/4-byte start) without enough continuation
   bytes following it. At most 3 bytes can be incomplete.
2. Split the buffer: emit everything up to the last complete character via
   `from_utf8_lossy`. Move the trailing incomplete bytes to the front of
   `self.bytes` so they survive across the drain.
3. On the next `push()` + `drain()` cycle, the held-back bytes receive their
   remaining continuation bytes and convert cleanly.

When the session ends and the buffer is drained for the last time, any
remaining incomplete bytes get the lossy treatment — a truncated stream
cannot be completed, and this matches current behavior.

### Files

- `crates/cleat/src/recording.rs` — `CoalesceBuffer::drain()`

### Testing

Unit tests on `CoalesceBuffer`:
- 2-byte character split at byte boundary across two drain cycles.
- 3-byte character split at both possible boundaries.
- 4-byte character (emoji) split at each of the three possible boundaries.
- No incomplete bytes — drain emits everything (regression).
- Final drain with incomplete trailing bytes — emits U+FFFD (matches today).

## 2. Dynamic CWD in inspect (#38)

### Problem

`inspect` reports `cwd` from `SessionMetadata`, set at launch time. After the
shell runs `cd`, the reported cwd is stale. Agents need the current working
directory to construct file paths.

### Change

Add two new fields to `ProcessInspect`:

```rust
pub struct ProcessInspect {
    pub leader_pid: u32,
    pub foreground_pgid: Option<u32>,
    pub leader_cwd: Option<PathBuf>,      // new
    pub foreground_cwd: Option<PathBuf>,   // new
}
```

Add a platform-specific `resolve_cwd(pid: u32) -> Option<PathBuf>` function:

- **Linux:** `std::fs::read_link(format!("/proc/{pid}/cwd"))`.
- **macOS:** `proc_pidinfo` with `PROC_PIDVNODEPATHINFO` via FFI. The `libc`
  crate (transitive dependency through `nix`) provides the necessary types.
  Falls back to `None` on error or permission denial.

Both calls happen on-demand in `build_inspect_result()`, alongside the
existing `tcgetpgrp()` call. No polling, no caching.

- `leader_cwd` resolves from `pty_child.pid` (the shell process).
- `foreground_cwd` resolves from the `tcgetpgrp()` PGID, treating it as a PID
  (the process group leader's PID equals the PGID). When the foreground
  process is the shell itself, both fields are equal.

The existing `session.cwd` field in `SessionInspect` is unchanged — it
remains the launch-time cwd.

### Files

- `crates/cleat/src/protocol.rs` — add fields to `ProcessInspect`
- `crates/cleat/src/session.rs` — `build_inspect_result()`, new
  `resolve_cwd()` function with `#[cfg(target_os)]` branches
- `crates/cleat/src/cli.rs` — `format_inspect_human()` to display new fields

### Testing

Integration test in `tests/lifecycle.rs`:
- Launch a session, send `cd /tmp`, wait for idle, inspect.
- Verify `leader_cwd` is `/tmp` (or the resolved path, e.g.
  `/private/tmp` on macOS).
- Verify `foreground_cwd` matches `leader_cwd` when shell is in foreground.

## 3. Session Lifecycle Documentation (#39)

### Problem

The README and help text do not explain the operational model. Agents need to
know session naming rules, collision behavior, storage location, daemon
lifecycle, and cleanup semantics.

### Change

Add a "Session model" section to `README.md`, placed after build instructions
and before any agent workflow examples. Content:

**One daemon per session.** Each session runs its own daemon process. The
daemon owns the PTY master fd and exits when the child process exits.

**Session IDs.** User-chosen or auto-generated (`session-{uuid}`). IDs become
directory names under the runtime root — use filesystem-safe characters.
Launching with an ID that already has a running daemon reuses the existing
session (no error, no duplicate).

**Runtime directory.** Discovered in order: `$CLEAT_RUNTIME_DIR` →
`$XDG_RUNTIME_DIR/cleat` → `$TMPDIR/cleat-{uid}` → `/tmp/cleat-{uid}`. Each
session gets a subdirectory containing `socket`, `daemon.pid`, and optionally
`session.cast`.

**Liveness.** The Unix socket is the liveness indicator. If the socket file
exists, the daemon is running.

**Cleanup.** When the child process exits, the daemon removes the socket and
pid file and exits. If recording was active, the session directory and `.cast`
file are preserved for later retrieval. Otherwise the entire session directory
is removed.

**No persistence across restarts.** Sessions do not survive daemon crashes or
host reboots. Recording files survive if written to disk.

### Files

- `README.md`

### Testing

No automated test. Verify by reading the rendered README.

## Not in scope

- `send --mark-after-echo` (#37) — needs separate design for echo detection.
- Batch/pipeline primitive (#34) — deferred until usage patterns emerge.
- VT stream transcoding (#29) — blocked on Ghostty C API (#22).

## Phasing

All three items are independent and can be implemented in any order. Suggested
sequence:

1. **#19 (UTF-8 decoder)** — smallest, self-contained, has clear unit tests.
2. **#38 (dynamic cwd)** — platform-specific code, integration test.
3. **#39 (lifecycle docs)** — pure documentation, no code risk.
