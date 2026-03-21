# Cleat Control Plane and Session Infrastructure

Design spec for cleat's evolution from a PTY persistence daemon into a structured terminal control plane for agents and a robust session infrastructure with FD-based daemon collaboration.

## Context

Cleat landed in three phases over two days:
- Phase 1: daemon-per-session PTY lifecycle, CLI (`attach`, `create`, `list`, `kill`), passthrough VT engine, flotilla `TerminalPool` adapter
- Phase 2: Ghostty VT engine integration, capability-aware replay, client-side cleanup
- Phase 3: `send-keys` with tmux-style key parsing, `capture` for screen text

This gives cleat basic session persistence with reattach replay. What it lacks is everything agents need to drive interactive applications without terminal scraping: structured introspection, incremental capture, process-aware signaling, and event-driven control.

Separately, cleat's daemon-per-session model creates natural opportunities for daemon collaboration — spawning sibling sessions that inherit execution context, and transferring PTY ownership between daemons for upgrades.

## Goals

Two complementary capability tracks:

1. **Agent control plane** — Make cleat the best way for AI agents to drive interactive CLI applications. Replace tmux-cli screen scraping with structured APIs: `inspect` for state, cursors for incremental capture, `signal` for process control, event streams for reactive workflows.

2. **Session infrastructure** — Enable cleat daemons to collaborate through FD transfer: spawn sibling sessions in the same execution context, hand off running sessions for daemon upgrades.

## Non-goals

- Cross-host process migration (flotilla handles session mobility at the agent/conversation level — continue the conversation in a new process, let the agent recover)
- Full process tree introspection in the first cut (defer to follow-up; `tcgetpgrp` on the PTY master answers the critical "is the shell idle?" question)
- Stable public socket protocol (the CLI remains the stable interface; protocol evolves freely)
- Object store spill or cross-agent correlation (flotilla-level concerns that build on cleat's local primitives)

## Architectural principles

**Control plane / data plane separation.** `inspect` returns structured metadata. `capture` returns terminal content. They never mix. Agents should never parse terminal text to learn session state.

**Opt-in recording.** Output logging is off by default. Activated explicitly (CLI flag, env var, signal to running session, or implicitly on first `mark` call). Size-limited with cooperative reaping.

**Caller-held cursors.** Cleat stores no per-caller state for incremental capture. A cursor is a byte offset into the output log. The caller holds it, cleat validates it.

**PTY owner has unique data.** Only the PTY master fd owner can call `tcgetpgrp` to learn the foreground process group. This is the most valuable signal for agents ("is a command running?") and cannot be obtained externally. Cleat must expose it.

**FD transfer, not state reconstruction.** Sibling sessions inherit execution context because the parent daemon does the fork — the child is born in the right namespace, container, cwd, and env. No reconstruction. Handover transfers the actual PTY fd, not a description of how to recreate it.

## Design

### Issue #5: Session introspection and output recording

The foundation layer. Two new CLI commands and an opt-in recording subsystem.

**`cleat inspect <session> [--json]`** returns structured state:

- Session metadata: id, name, state, timestamps, VT engine type
- Terminal dimensions: rows, cols
- Process state: leader PID, foreground PGID (via `tcgetpgrp`)
- Attachment info: connected clients and roles
- Recording state: active flag, bytes written, current cursor

The key derived signal: `foreground_pgid != leader_pgid` means a command is running. Equal means the shell is idle. This single check replaces fragile prompt-detection heuristics.

**`cleat signal <session> <signal> [--target foreground|leader|tree]`** delivers OS-level signals. Default target is the foreground process group. This is distinct from `send-keys` — `signal INT` kills the foreground job; `send-keys C-c` sends a byte that the application may interpret differently. Signals are specified by symbolic POSIX name (`INT`, `TERM`, `HUP`, `KILL`, `USR1`, `USR2`, `STOP`, `CONT`).

**Opt-in output recording** writes an append-only byte log of PTY output. Activation paths:
- `cleat create --record` / `cleat attach --record`
- `CLEAT_RECORD=1` environment variable
- `cleat record <session>` to toggle on a running session
- Implicitly on first `cleat mark` call

Recording files live alongside existing session artifacts in the runtime directory:

```
$runtime/<session-id>/
  ├── socket          (existing)
  ├── meta.json       (existing)
  ├── daemon.pid      (existing)
  ├── foreground      (existing)
  ├── output.log      (new — append-only PTY output)
  └── snapshots/      (new — periodic VT engine snapshots)
```

Storage is tiered: raw bytes in `output.log`, periodic VT snapshots in `snapshots/`. The output log is the source of truth; snapshots enable efficient seek for replay.

**Size management:** Each session's recording grows until the session ends. When a session is killed, its recording files move to an archive directory rather than being deleted immediately. **Cooperative reaping** is a cross-session concern: when a daemon starts (or periodically while running), it checks the total size of archived recordings under the runtime root. If the total exceeds a configurable limit, it deletes the oldest archived recordings until the budget is met. This means any running daemon may clean up recordings left behind by other sessions — including sessions whose daemons have already exited. Callers that hold cursors referencing reaped data receive a truncation indicator.

### Issue #6: Capture cursors and markers

Builds on recording. A cursor is a byte offset into the output log.

**`cleat mark <session>`** returns the current cursor (byte offset). If recording is inactive, `mark` activates it.

**`cleat capture <session> --since <cursor>`** returns output written after that offset. Raw bytes by default; `--text` runs them through the VT engine for plain text.

If the cursor references reaped data, capture returns a truncation indicator plus whatever remains. Cursor 0 means "everything since recording started."

**Named markers** are convenience sugar: `cleat mark <session> test-start` stores a cursor value with a label in session metadata. `cleat capture --since-marker test-start` looks up the stored cursor. Useful for multi-agent coordination and human workflows.

### Issue #7: FD transfer and sibling sessions

**`cleat create --from <session>`** spawns a sibling session:

1. Client sends the request to the existing session's daemon
2. Daemon calls `forkpty()` to create a new PTY and fork a child process — the child inherits the daemon's container, namespace, cwd, and env naturally, then execs the requested shell/command
3. Daemon spawns a separate cleat daemon process (a new binary invocation, not a fork)
4. Daemon transfers the PTY master fd to the new daemon via SCM_RIGHTS over a Unix socketpair, along with a JSON manifest
5. New daemon takes ownership of the PTY, creates its own runtime directory, enters its event loop
6. If the child process fails to exec, the original daemon reports the failure back to the client

The original daemon does the `forkpty`, so context inheritance is automatic — no reconstruction. The new daemon process does not need to be in the same namespace; it just needs the PTY master fd.

The FD transfer protocol uses a JSON manifest alongside the fd bundle:

```json
{
  "fds": [{ "index": 0, "role": "pty_master" }],
  "session": { "name": "...", "cwd": "...", "cmd": "..." },
  "source_session": "parent-id"
}
```

This manifest format extends naturally for handover (#8), which adds VT state, attachment fds, and epoch information.

### Issue #8: Session handover

The primary driver is daemon upgrade without session loss. A new cleat binary takes over a running session's PTY from the old daemon.

**Epoch model:** Each session has a monotonic epoch counter. Handover increments it. Callers can detect stale daemons by comparing epochs. Only one daemon is authoritative per session at any time.

**Handover phases:**

1. **Quiesce** — freeze attachment and control changes; PTY output continues
2. **Prepare** — serialize manifest with VT state, attachment info, and epoch; collect fds
3. **Transfer** — send manifest + fd bundle to new daemon via SCM_RIGHTS
4. **Ready** — new daemon validates, reconstructs state, responds READY
5. **Commit** — old daemon increments epoch, yields authority
6. **Finalize** — old daemon exits

Failure before COMMIT rolls back cleanly — old daemon resumes. Failure after COMMIT relies on epoch correctness; the old daemon must not mutate state.

Shares the FD transfer infrastructure with sibling sessions (#7). The difference: sibling transfers a newly created PTY; handover transfers an existing one along with VT engine state and attachment fds.

### Issue #9: Structured event stream

Replaces polling with reactive subscriptions. Builds on recording (#5) and cursors (#6).

**`cleat events <session> [--since <cursor>] [--types <filter>]`** streams JSONL events:

- `output` — PTY bytes appended (with cursor position)
- `resize` — terminal dimensions changed
- `attach` / `detach` — client connected/disconnected
- `process_change` — foreground process group changed
- `lifecycle` — session created, recording toggled, handover, exit
- `marker` — named marker placed
- `signal` — signal delivered

Each event carries a monotonic sequence number, timestamp, and typed payload. Sequence numbers are byte offsets into the output log — the same value space as capture cursors from #6. An agent can use a sequence number from an event directly as a `--since` cursor for capture, and vice versa.

**Output coalescing:** The log captures every byte. The stream delivers at a configurable rate (e.g. 10 Hz) to avoid overwhelming subscribers. Subscribers choose their coalescing preference.

**Reconnection:** Clients hold their last sequence number. `--since <seq>` replays from the log. If events have been reaped, the stream begins with a synthetic snapshot event containing current state.

**Process change detection:** Periodic `tcgetpgrp` polling (~100ms) detects foreground PGID changes and emits `process_change` events. Sufficient for agent reaction times.

**Transport:** Event streams are delivered over a separate Unix socket connection to the session daemon, not the existing foreground attachment socket. The current protocol assumes a single active client; event subscribers are passive readers that must not interfere with the foreground attachment lifecycle. The CLI `cleat events` command opens this separate connection.

## Dependency chain

```
#5 Foundation (inspect + signal + recording)
 └─→ #6 Cursors & markers
      └─→ #9 Event stream

#7 FD transfer & sibling sessions
 └─→ #8 Handover
```

The two chains are independent. #5→#6→#9 is agent control plane. #7→#8 is session infrastructure. Both can proceed in parallel.

## Relationship to flotilla

Cleat is moving to `flotilla-org/cleat` as a standalone project. These features are cleat-native — they work without flotilla.

Flotilla adds a higher-level surface on top:
- Resolves work items to cleat sessions
- Routes commands across hosts via the multi-host protocol
- Correlates cleat event streams with agent hooks, git operations, and cross-session timelines
- Handles object store spill for archived recordings
- Manages placement and container context (cleat just runs wherever it's started)

The `TerminalPool` trait in flotilla-core delegates to cleat's CLI. New commands (`inspect`, `signal`, `mark`, `capture --since`, `events`) extend this delegation naturally.

## Key design decisions captured

1. **Agent control plane first, session infrastructure second.** Both matter, but the agent story is the immediate differentiator over tmux-cli.

2. **`tcgetpgrp` is the killer signal.** Only the PTY owner can report the foreground process group. This one syscall replaces all prompt-detection heuristics. Phase 1 exposes leader PID + foreground PGID; full process tree walking is a follow-up.

3. **Recording is opt-in with multiple activation paths.** Not every session needs a persistent output log. But cursors and event streams need one, so activating on first `mark` ensures it's there when needed.

4. **Caller-held cursors, not server-side subscriptions (for capture).** Cleat stores nothing per-caller. A cursor is a byte offset. This keeps the daemon stateless with respect to consumers.

5. **FD transfer, not context reconstruction.** The parent daemon spawns siblings, so they inherit everything naturally. Handover transfers the actual PTY fd. No serialization of "what the process looks like."

6. **Epoch for ownership correctness.** A monotonic counter makes stale daemon detection trivial and prevents split-brain after failed handover.

7. **Tiered storage with cooperative reaping.** Raw output log + periodic VT snapshots. Recordings from killed sessions move to an archive directory. Running daemons cooperatively reap the oldest archived recordings when total size exceeds a budget. The log is the truth; snapshots are seek optimization.

## Open questions

- **Recording size defaults.** 10 MB? 50 MB? Depends on use case. Agent debugging sessions produce less output than long-running build logs. Needs experimentation.
- **Reaping strategy.** Archive directory layout, how often running daemons check the budget, whether to keep snapshots longer than raw bytes. Needs design during #5 implementation.
- **Event stream backpressure.** What happens when a subscriber can't keep up? Drop events with gap notification? Buffer with limits? JSONL over Unix socket has natural backpressure but needs explicit policy.
- **Full process tree (future).** When cleat eventually reports children/descendants, should it call platform APIs directly (procfs, sysctl) or delegate to an external tool? The argument for inclusion: a single `cleat inspect` call avoids chattiness across remote boundaries.
