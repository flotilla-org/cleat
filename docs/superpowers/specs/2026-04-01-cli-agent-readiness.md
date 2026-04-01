# CLI Agent Readiness

Improve cleat's CLI for agent consumers: add help text, align command names with tmux-cli conventions, add high-level agent verbs (`send`, `wait`, `interrupt`, `escape`), and fix the since-marker capture off-by-one.

## Context

An audit of cleat's CLI from an agent's perspective revealed five gaps:

1. **No help text.** Every subcommand shows as a bare name. Arguments lack descriptions. An agent reading `--help` cannot discover what cleat does.

2. **`send-keys` is awkward for agents.** Agents almost always send literal text followed by Enter. With `send-keys`, this requires two calls (`send-keys -l 'echo hello'` then `send-keys Enter`) because `-l` makes `Enter` literal.

3. **No `wait` primitive.** Agents sleep and hope the command finished. No way to block until output settles or a condition is met.

4. **Command names diverge from tmux-cli.** Agents trained on tmux-cli expect `launch`, `send`, `interrupt`. Cleat uses `create` and lacks the high-level verbs.

5. **`capture --since-marker` has an off-by-one.** Output starts with a duplicated first character (`eecho` instead of `echo`).

A sixth issue — `capture --since-marker` returns raw VT escapes instead of rendered text — requires VT stream transcoding infrastructure that depends on the Ghostty RenderState C API (#22). Filed separately as #29.

## Design

### 1. Help text

Add clap `about` and `help` attributes throughout.

**Top-level about:** "Session daemon with a structured control plane for agents and terminal persistence."

**Subcommand descriptions:**

| Command | About |
|---------|-------|
| `launch` | Create a new session |
| `attach` | Attach to a session interactively |
| `send` | Send text to a session |
| `send-keys` | Send key sequences using tmux-style names |
| `capture` | Capture terminal screen content |
| `list` | List all sessions |
| `inspect` | Show session state and process info |
| `wait` | Wait for a condition before continuing |
| `kill` | Terminate a session |
| `detach` | Detach from a session |
| `signal` | Send an OS signal to the session process |
| `record` | Enable output recording |
| `mark` | Set a named marker in the recording |
| `interrupt` | Send Ctrl-C to a session |
| `escape` | Send Escape to a session |

**Argument help** — add terse descriptions to every undocumented arg:

- `--vt` → "Virtual terminal engine"
- `--cwd` → "Working directory for the session"
- `--cmd` → "Command to run (default: user's shell)"
- `--json` → "Output as JSON"
- `--record` → "Enable output recording"
- `--no-create` → "Fail if the session does not exist"
- `--no-enter` → "Do not append Enter after the text"
- `-l` → "Send keys as literal characters"
- `-H` → "Send keys as hex-encoded bytes"
- `-N` → "Repeat the key sequence N times"
- `--target` → "Signal target: foreground (default) or leader"
- `--timeout` → "Maximum seconds to wait (default: 30)"
- `--idle-time` → "Wait until output settles for this many seconds"
- `--text` → "Wait until this text appears on screen"
- `--since` → already documented
- `--since-marker` → already documented
- `--raw` → already documented

**send-keys `after_long_help`** — document supported key names:

```
Key names: Enter, Escape (Esc), Tab, BSpace, Space,
           Up, Down, Left, Right, Home, End,
           PgUp (PageUp), PgDn (PageDown),
           IC (Insert), DC (Delete),
           F1-F12, BTab (Shift-Tab)

Modifiers:  C-x (Ctrl), M-x (Meta/Alt), S-x (Shift)
            ^x  (Ctrl, alternative syntax)

Examples:   cleat send-keys myapp Enter
            cleat send-keys myapp C-c
            cleat send-keys myapp -l 'literal text'
            cleat send-keys myapp -H 1b5b41
```

### 2. Command alignment with tmux-cli

**Rename `create` to `launch`**

Rename the `Create` variant to `Launch` in the `Command` enum. Add `#[command(alias = "create")]` as a hidden alias for backwards compatibility. Same arguments and behavior.

Add help guidance in `after_long_help`:

```
Tip: launch a shell (e.g. zsh) and use `send` to run commands.
Sessions exit when the launched process exits.
```

**Add `send` subcommand**

```
cleat send <ID> <TEXT> [--no-enter]
```

- `<TEXT>` — single argument, always literal (no key name interpretation)
- Appends `\r` by default
- `--no-enter` suppresses the `\r`
- Implementation: encode text as literal bytes, conditionally append `\r`, call `service.send_keys()`

**Add `interrupt` subcommand**

```
cleat interrupt <ID>
```

Sends `0x03` (Ctrl-C). Equivalent to `send-keys <ID> C-c`.

**Add `escape` subcommand**

```
cleat escape <ID>
```

Sends `0x1b` (Escape). Equivalent to `send-keys <ID> Escape`.

### 3. `wait` command

```
cleat wait <ID> <--idle-time <SECS> | --text <TEXT>> [--timeout <SECS>] [--json]
```

At least one condition is required. There is no implicit default mode — process-state detection (`foreground_pgid == leader_pid`) only works for shell sessions and silently gives wrong answers for non-shell commands (e.g. `launch --cmd 'sleep 60'` would report "ready" immediately). Callers specify what they are waiting for.

**`--idle-time <SECS>` — output silence.** The daemon measures silence relative to the wait registration time, not historical output. Specifically: the condition is satisfied when `now - max(registration_time, last_output_time) >= idle_time`. Stale silence from before registration does not count; fresh output resets the clock.

**`--text <TEXT>` — text match.** The daemon checks the VT-rendered screen and responds when the text appears. Requires a VT engine that supports screen capture (same as `capture` without `--since`). Returns an error for passthrough sessions — including when combined with `--idle-time`. The daemon validates all conditions before registering the wait; if any condition is unsupported, the entire request fails. Silently dropping an unsupported condition would change OR semantics in ways the caller did not intend.

**Flags compose with OR semantics.** When both `--idle-time` and `--text` are specified, the daemon responds when *either* condition is met — "wait until this text appears or output settles." This matches the agent intent: multiple heuristics for "something finished."

**`--timeout <SECS>`** applies to all modes. Default: 30 seconds.

**Exit codes:**
- 0 — condition met
- 1 — timeout
- 2 — session gone or error

The current binary exits 0 or 1 for all commands (`main.rs` maps `Err` to exit code 1). This spec requires changing `cli::execute` to return a typed result that `main` maps to distinct exit codes. The `WaitTimeout` variant maps to exit 1; other errors map to exit 2. All existing commands continue to exit 0 on success and 1 on error (no behavior change for them).

**`--json`** outputs a JSON object with `status` and `elapsed_ms`. Status values: `"ready"`, `"timeout"`, `"session_gone"`. Examples: `{"status": "ready", "elapsed_ms": 342}`, `{"status": "timeout", "elapsed_ms": 30000}`, `{"status": "session_gone", "elapsed_ms": 1204}`. Without `--json`, silent on success, error message on timeout or session gone.

#### Protocol

New frames:

```
Frame::Wait { conditions: Vec<WaitCondition>, timeout_ms: u64 }
Frame::WaitResult { status: WaitStatus, elapsed_ms: u64 }
```

`WaitCondition` variants:
- `OutputIdle { quiet_ms: u64 }` — no PTY output for `quiet_ms` milliseconds
- `TextMatch { text: String }` — text found on VT-rendered screen

`WaitStatus` variants:
- `Ready`
- `Timeout`
- `SessionGone`

If the session process exits while a wait is pending, the daemon writes `WaitResult { status: SessionGone }`. If the socket drops unexpectedly (daemon crash), the client treats the read error as exit code 2.

The daemon registers the wait conditions on the accepted socket connection. Conditions are evaluated immediately at registration (so `--text` succeeds instantly if the text is already on screen). Note: `--idle-time` cannot succeed at registration because silence is measured from registration time — see the idle-time definition above. After registration, the event loop evaluates pending conditions on each loop tick — not only after PTY reads, since `--idle-time` requires timer-driven evaluation when no output arrives. When any condition is met, the timeout expires, or the session exits, the daemon writes `WaitResult` and closes the wait. Multiple concurrent waiters are supported — each wait request opens its own socket connection and the daemon tracks each independently.

### 4. Off-by-one in since-marker capture

**Symptom:** `capture --since-marker` starts with a duplicated first character (`eecho hello` instead of `echo hello`).

**Investigation required.** The marker offset is recorded after `rec.flush()` and before the marker event is written, in the single-threaded daemon event loop (`session.rs:583`). The offset should be writer-aligned. The root cause could be in the stored offset, the reader's seek behavior, or a race between PTY output arrival and the flush. Do not prescribe a fix direction until the fault is identified.

**Approach:** Test-first.

1. Write a test that creates a session, waits for the prompt to settle, sets a marker, sends known text, captures since the marker, and asserts the exact output.
2. Dump the raw cast file around the marker offset to see exact event boundaries and verify the offset lands at a line start.
3. Determine whether the stored offset or the reader semantics are at fault.
4. Fix the identified root cause.

## Scope boundary

**In scope:** Help text, command renames/additions, `wait` with protocol frames, exit code changes for `wait`, off-by-one fix.

**Out of scope:** VT stream transcoding for `capture --since-marker` rendered text (#29, depends on #22). Terminal screen introspection. The `capture --since-marker` command continues to return raw event data until #29 lands.
