# CLI Agent Readiness

Improve cleat's CLI for agent consumers: add help text, align command names with tmux-cli conventions, add high-level agent verbs (`send`, `wait`, `interrupt`, `escape`), and fix the since-marker capture off-by-one.

## Context

An audit of cleat's CLI from an agent's perspective revealed five gaps:

1. **No help text.** Every subcommand shows as a bare name. Arguments lack descriptions. An agent reading `--help` cannot discover what cleat does.

2. **`send-keys` is awkward for agents.** Agents almost always send literal text followed by Enter. With `send-keys`, this requires two calls (`send-keys -l 'echo hello'` then `send-keys Enter`) because `-l` makes `Enter` literal.

3. **No `wait` primitive.** Agents sleep and hope the command finished. No way to block until the shell is idle.

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
| `wait` | Wait for a session to become idle |
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

Rename the `Create` variant to `Launch` in the `Command` enum. Add `#[command(alias = "create")]` for backwards compatibility. Same arguments and behavior.

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
cleat wait <ID> [--timeout <SECS>] [--idle-time <SECS>] [--text <TEXT>] [--json]
```

**Default mode — process state.** The daemon checks `tcgetpgrp` on each event loop cycle and responds when `foreground_pgid == leader_pid` (the shell is idle — the foreground command returned).

**`--idle-time <SECS>` — output silence.** The daemon tracks when PTY output last arrived and responds when no output has arrived for the specified duration.

**`--text <TEXT>` — text match.** The daemon checks the VT-rendered screen after each output event and responds when the text appears.

`--idle-time` and `--text` are mutually exclusive. Each wait request evaluates one condition.

**`--timeout <SECS>`** applies to all modes. Default: 30 seconds.

**Exit codes:**
- 0 — condition met
- 1 — timeout
- 2 — session gone or error

**`--json`** outputs `{"status": "ready", "elapsed_ms": 342}` or `{"status": "timeout", "elapsed_ms": 30000}`. Without `--json`, silent on success, error message on timeout or failure.

#### Protocol

New frames:

```
Frame::Wait { mode: WaitMode, timeout_ms: u64 }
Frame::WaitResult { status: WaitStatus, elapsed_ms: u64 }
```

`WaitMode` variants:
- `ProcessIdle` — foreground pgid == leader pid
- `OutputIdle { quiet_ms: u64 }` — no PTY output for `quiet_ms` milliseconds
- `TextMatch { text: String }` — text found on VT-rendered screen

`WaitStatus` variants:
- `Ready`
- `Timeout`

The daemon registers the wait condition on the accepted socket connection. The event loop evaluates pending wait conditions after each PTY read or SIGCHLD. When the condition is met or the timeout expires, the daemon writes `WaitResult` and closes the wait.

### 4. Off-by-one in since-marker capture

**Symptom:** `capture --since-marker` starts with a duplicated first character (`eecho hello` instead of `echo hello`).

**Area:** The boundary between `mark` (flushes coalescing buffer, records `bytes_written()`) and `read_output_since` (seeks to that offset). The coalescing buffer may split terminal echo into separate events based on arrival timing.

**Approach:** Test-first.

1. Write a test that creates a session, waits for the prompt to settle, sets a marker, sends known text, captures since the marker, and asserts the exact output.
2. Dump the raw cast file around the marker offset to see event boundaries.
3. Check whether the coalescing flush in the mark handler races with concurrent PTY output on the event loop.
4. Fix the root cause — either tighten the flush/offset boundary or adjust `read_output_since` to handle a partial leading event.

## Scope boundary

**In scope:** Help text, command renames/additions, `wait` with protocol frames, off-by-one fix.

**Out of scope:** VT stream transcoding for `capture --since-marker` rendered text (#29, depends on #22). Terminal screen introspection. The `capture --since-marker` command continues to return raw event data until #29 lands.
