# Agent UX Improvements — Design Spec

Date: 2026-04-02

## Context

A capable agent (Pi Agent) tested cleat for agent-style terminal workflows and
produced a detailed usability report. The previous fix-agent-issues work (PR #33)
addressed daemon version mismatch bugs and added initial help text. This spec
covers the UX and missing-feature findings from that report.

The core finding: cleat's primitives work well, but two commands (`capture` and
`wait`) each hide two distinct data sources behind one interface, confusing
agents that expect tmux-like semantics. Several convenience gaps also add
friction.

## Findings summary

| # | Finding | Category |
|---|---------|----------|
| 1 | `--help` drops the agent workflow snippet that `-h` shows | Help text |
| 2 | Functional binary setup is painful for in-repo agents | Tooling |
| 3 | No atomic mark+send; sequential mark then send can race | Missing feature |
| 4 | `wait --text` matches screen state, not new output since a checkpoint | Semantic gap |
| 5 | `capture` hides two unrelated data paths behind one command | Semantic gap |
| 6 | `--raw` on capture produces identical output to text mode | Known; tracked in #29 |
| 7 | No "run command and collect result" convenience primitive | Missing feature |
| 8 | No pane/window abstraction | Out of scope |

## Design

### 1. Split `capture` into `capture` + `transcript`

**Problem.** `capture` today does two unrelated things depending on flags:

- Without `--since`/`--since-marker`: returns the VT engine's rendered screen
  (daemon-side, requires functional VT).
- With `--since`/`--since-marker`: reads recorded output from the `.cast` file
  (client-side, requires active recording).

These share no implementation plumbing. The daemon handles screen capture; the
client reads the cast file directly for recording deltas. Agents familiar with
tmux's `capture-pane` expect a screen snapshot. The flag-based mode switch
surprises them.

**Change.** Split into two commands:

**`capture`** — screen snapshot, tmux `capture-pane` equivalent.
- No flags change its fundamental behavior.
- Returns the current rendered screen from the VT engine.
- Requires a functional VT engine.
- Existing `Frame::Capture` protocol unchanged.

**`transcript`** — recorded output since a checkpoint.
- Requires `--since <offset>` or `--since-marker <name>` (one is mandatory).
- Reads the `.cast` file client-side, same as today's `capture --since`.
- Accepts `--raw` (currently identical to text; VT replay is planned per #29).
- No daemon involvement beyond marker resolution.

```
cleat capture my-session                              # screen now
cleat transcript my-session --since-marker checkpoint  # output since checkpoint
```

**Migration.** `capture --since` and `capture --since-marker` become errors with
a message directing users to `transcript`. No silent aliasing — a clean break
keeps the mental model sharp.

### 2. Add `expect` command (recording-based text wait)

**Problem.** `wait --text` checks whether text is visible on the VT screen right
now. If the text is already on screen, it returns immediately with
`elapsed_ms: 0`. Agents expect "wait for new output containing X" but get
"check if X is already visible." The only workaround is mark + wait --idle-time
+ transcript --since-marker, which doesn't support text matching at all.

**Change.** Add `expect` — a recording-stream text wait that is edge-triggered
by design.

```
cleat expect my-session --text 'PASS' --since-marker m1 --timeout 30
```

Semantics:
- Requires an active recording.
- Requires `--since-marker` or `--since` to establish the checkpoint.
  This makes it inherently edge-triggered: only output recorded after the
  checkpoint is searched.
- `--text` is required (the whole point is text matching on the stream).
- `--timeout` works the same as `wait --timeout`.
- Exit codes match `wait`: 0 = found, 1 = timeout, 2 = error/session gone.
- `--json` produces `{"status": "ready|timeout|session_gone", "elapsed_ms": N}`.

Implementation approach: the daemon already has the pending-wait polling loop.
`expect` adds a new `WaitCondition::RecordingTextMatch { text, since_offset }`
that the daemon evaluates by scanning new cast file events on each loop tick.
Alternatively, `expect` could poll client-side against the cast file. The
implementation plan should evaluate both approaches.

**`wait` stays unchanged** — it remains the screen-state and PTY-idle blocker.
Its `--text` flag continues to match against the current VT screen, which is the
right behavior for "is the prompt visible?" style checks. The help text should
document this explicitly (see item 5 below).

### 3. `send --mark-before` and `send-keys --mark-before`

**Problem.** The idiomatic recording workflow is mark, send, wait, transcript.
When an agent pipelines these calls, a race between mark and send can produce
empty or misleading deltas. The mark and send are two separate daemon round-trips
with no atomicity guarantee.

**Change.** Add `--mark-before <NAME>` to both `send` and `send-keys`.

```
cleat send my-session 'make test' --mark-before m1
cleat send-keys my-session Enter --mark-before m1
```

Semantics:
- The daemon places the marker *before* writing to the PTY, in the same
  connection handler. One round-trip, no race.
- Returns the marker offset in the response (or in JSON output).
- The marker name is stored in the daemon's marker map, same as `mark`.
- If recording is not active, the command fails with an error (same as `mark`).

Protocol: extend `Frame::SendKeys` and the send frame to carry an optional
marker name. The daemon handler checks for the marker, writes it, then proceeds
with the PTY write.

### 4. `--help` includes the agent workflow snippet

**Problem.** clap's `after_help` appears with `-h`; `after_long_help` replaces
it with `--help`. The current code puts the workflow snippet in `after_help` and
only the build-support message in `after_long_help`. Agents that run `--help`
(the more common flag) miss the workflow guidance.

**Change.** `after_long_help` should include both the workflow snippet and the
build-support message. Since `BUILD_SUPPORT_MESSAGE` is a `const &str` and
`concat!` won't work, build the combined string at runtime or use a `const fn`.

Additionally, update the workflow snippet to reflect the new command names once
`transcript` exists:

```
Typical agent workflow:
  cleat launch --record my-session --cmd bash
  cleat send my-session 'make test' --mark-before m1
  cleat wait my-session --idle-time 2
  cleat transcript my-session --since-marker m1
  cleat kill my-session
```

### 5. Document `wait --text` screen-state semantics

**Problem.** The `wait --text` help says "Wait until this text appears on
screen" but does not warn that it matches text already visible. Agents expecting
edge-triggered behavior get surprising instant-return results.

**Change.** Update the `wait` `after_long_help` to include:

```
NOTE: --text matches against the current VT screen. If the text is
already visible when wait is called, it returns immediately. For
edge-triggered text matching on new output, use the expect command
with --since-marker.
```

### 6. Wrapper script for in-repo agent convenience

**Problem.** Using the functional Ghostty-backed binary requires setting
`DYLD_LIBRARY_PATH` (macOS) or `LD_LIBRARY_PATH` (Linux) and passing
`--features ghostty-vt`. This is the biggest usability hurdle for agents
working in the repo.

**Change.** Add `./tools/cleat` — a wrapper script that:

1. Detects the platform and sets the appropriate library path variable.
2. Points at `.tools/ghostty-install/lib`.
3. Locates the built binary (checks `target/debug/cleat` and
   `target/release/cleat`, preferring release if both exist).
4. Falls back to `cargo run -p cleat --features ghostty-vt --` if no
   pre-built binary is found.
5. Passes all arguments through.

Agent experience becomes:
```bash
./tools/cleat launch --record my-session --cmd bash
```

### 7. Investigate batch/pipeline primitive (exploratory)

**Problem.** Agents orchestrate multi-step sequences (mark, send, wait,
transcript) that would benefit from atomic execution. A convenience command
like `exec` was considered but introduces naming/hygiene issues: an internal
marker needs a name, and both caller-invented and auto-generated names leak
into shared state or risk collision.

**Status.** Deferred. File as an exploratory issue to revisit once agents use
the split primitives in practice. Possible directions include:

- A `batch` command that accepts a sequence of operations.
- Auto-scoped markers with private names that are dropped on completion.
- A daemon-side compound operation that uses byte offsets internally
  without creating named markers.

No design commitment at this time. Real usage patterns from the split commands
should inform the approach.

### 8. Agent-friendly binary distribution (exploratory)

**Problem.** Beyond in-repo convenience, agents outside the repo cannot easily
obtain a working cleat binary. tmux is typically available system-wide; cleat
requires building from source with Ghostty.

**Status.** Deferred. The wrapper script (item 6) addresses the immediate
in-repo pain. Broader distribution (prebuilt binaries, Homebrew formula,
static linking) is a separate effort that depends on the Ghostty dependency
story stabilizing.

## Not in scope

- **Pane/window abstraction.** cleat is one PTY per session. Multi-pane layouts
  are a different tool. Agents that need concurrent sessions launch multiple
  cleat sessions.
- **`--raw` differentiation.** Tracked in #29 (VT stream transcoding). The
  `transcript` command inherits the `--raw` flag so it benefits automatically
  when #29 lands.
- **Batch/pipeline primitive.** Recorded as exploratory (item 7) but not
  designed here.

## Phasing

**Phase 1 — quick fix (do now):**
- Item 6: `./tools/cleat` wrapper script (no dependencies)

**Phase 2 — command split:**
- Item 1: Split `capture` into `capture` + `transcript`
- Item 2: Add `expect` command

**Phase 3 — atomic operations:**
- Item 3: `send --mark-before` and `send-keys --mark-before`

**Phase 4 — help text (depends on phases 2 and 3):**
- Item 4: `--help` includes workflow snippet (references `transcript`, `--mark-before`)
- Item 5: Document `wait --text` screen-state semantics (references `expect`)

**Phase 5 — exploratory:**
- Item 7: Batch/pipeline primitive
- Item 8: Binary distribution

## Testing

- Existing lifecycle tests (`tests/lifecycle.rs`) cover capture, wait, mark,
  and send. These must continue to pass after the split.
- `transcript` inherits the existing `capture --since` test coverage and adds
  cases for the new command name and the error when `capture --since` is used.
- `expect` needs integration tests: text found after marker, timeout when text
  absent, edge-trigger verification (text already on screen does NOT match
  unless it also appears in recording after the checkpoint).
- `send --mark-before` needs a test verifying the marker offset is returned
  and that `transcript --since-marker` produces the expected delta.
- Wrapper script needs a smoke test (can be a shell-based test that verifies
  it locates the binary and passes `--help` through).
