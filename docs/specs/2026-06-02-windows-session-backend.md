# Windows Session Backend Design

Design sketch for adding a native Windows session backend to cleat after the
platform-boundary refactor in PR #64.

## Goal

Support cleat sessions on Windows while preserving the daemon-per-session model:
`cleat launch` starts a long-lived session daemon, clients communicate with that
daemon, and the session continues after the foreground CLI exits.

The first Windows milestone should make a narrow command-driven session work.
Interactive attach parity can follow once the child PTY and IPC backend are
proven.

## Non-Goals

- Do not implement full Ghostty app support. Cleat depends on `libghostty-vt`,
  which is a separate lower-level library.
- Do not redesign Unix sessions around Windows constraints.
- Do not make Windows signals pretend to be POSIX. Expose compatible commands
  where behavior is defensible, and document differences.
- Do not start with async unless a concrete Windows API forces it.

## Existing Shape

The shared runtime now has platform seams under `crates/cleat/src/platform/`:

- `ipc`: session stream/listener/connect helpers
- `terminal`: foreground terminal mode, size, stdin polling, attach signal exit
- `daemon`: daemon process spawn, pid file, liveness, termination
- `process`: executable naming, executable detection, process termination
- `signals`: CLI signal-name parsing to platform signal values
- `pty`: PTY child facade, currently backed by the Unix module

The shared session loop still owns:

- protocol frame dispatch
- VT engine feed/replay/capture
- recorder, markers, waits, expects
- foreground attachment state
- cleanup and session lifecycle

## Backend Choices

### IPC

Use Windows named pipes for session IPC.

Rationale:
- Named pipes match the local daemon/control-plane shape better than TCP.
- They avoid port allocation, firewall, and localhost binding ambiguity.
- They support multiple client connections and blocking stream IO.

Open questions:
- Pipe name format should be derived from the runtime root and session id but
  sanitized for Windows pipe namespace constraints.
- The session directory still needs a liveness indicator. The existing `socket`
  path can remain as a marker file, but the actual transport will be a named
  pipe.

Alternative: localhost TCP. This is simpler to prototype but creates port
management and security questions. Keep it as a fallback if named pipes become
too awkward.

### PTY

Use Windows ConPTY (`CreatePseudoConsole`) for the child terminal.

Rationale:
- It is the native Windows pseudo-terminal API.
- It provides byte-stream pipes that map naturally to cleat's VT feed and
  recording model.
- It supports resize through `ResizePseudoConsole`.

First spike should prove:
- spawn `powershell.exe` or `cmd.exe`
- read PTY output
- write input
- resize
- detect child exit
- close handles without leaking the pseudoconsole

Implementation choice:
- Prefer a small direct Windows API wrapper or a narrow existing crate if it
  exposes the exact needed handles and lifecycle.
- Avoid committing to a broad terminal crate until the cleat-specific contract
  is clear.

### Shell and Command Semantics

Unix currently runs `$SHELL` or `$SHELL -lc <cmd>`.

Windows should default to `pwsh.exe` if available, then `powershell.exe`, then
`cmd.exe`. For `--cmd`, use shell-specific command execution:

- PowerShell: `-NoLogo -NoProfile -Command <cmd>`
- cmd: `/C <cmd>`

Open question:
- Whether `--cmd` should keep the shell alive after command completion. Unix
  exits when the command exits today; preserve that behavior initially.

### Process Control

Map existing control commands conservatively:

- `kill`: terminate the daemon if it is still a cleat process; daemon cleanup
  should terminate the child process/pseudoconsole.
- `signal INT`: send Ctrl-C to the console process group if possible.
- `signal TERM` / `KILL`: terminate the child process or process tree.
- `signal HUP`, `QUIT`, `USR1`, `USR2`, `STOP`, `TSTP`, `CONT`: unsupported on
  Windows unless a specific mapping is later justified.

Foreground-process-group semantics do not directly exist on Windows. `inspect`
should not fabricate `foreground_pgid`. Add platform-specific fields later if
needed, or report `None` with clear state.

Open questions:
- Whether ConPTY plus process creation can reliably send Ctrl-C only to the
  session child and not the cleat daemon.
- Whether process tree termination should use Job Objects from the start.

### Foreground Attach

Defer full interactive attach until the ConPTY backend works for command-driven
sessions.

Initial attach support can remain unsupported on Windows, or it can use the same
byte relay once these are solved:

- Windows console raw-ish mode for stdin
- terminal size detection from Windows console APIs
- Ctrl-C handling without killing the cleat client itself
- resize event propagation

OpenSSH-on-Windows is a real target: attach behavior should be tested from an
SSH session as well as Windows Terminal.

### VT Engine

Treat `libghostty-vt` as potentially portable to Windows, independent of the
full Ghostty app.

Follow-up work:
- add a Windows prepare script or cross-platform tool for pinned Ghostty VT
- support `.dll`/import `.lib` or static `ghostty-vt-static.lib`
- adjust runtime library lookup (`PATH`) if dynamically linked

This is separate from the session backend. A passthrough/nonfunctional default
Windows build should remain possible while the Ghostty VT path is explored.

## Proposed Milestones

1. Keep Windows default build green in CI.
2. ConPTY spike outside the shared daemon loop.
3. Named-pipe IPC spike with `Frame` read/write round trips.
4. Minimal backend integration:
   - `launch --cmd`
   - `send` / `send-keys`
   - `record`, `transcript`, `expect --since`
   - `kill`
   - no interactive `attach` yet
5. Add `capture` once a functional VT engine is available on Windows.
6. Add interactive `attach` after terminal-mode and Ctrl-C behavior are proven.

## Risks

- Ctrl-C and process-tree behavior may not map cleanly to Unix signal targets.
- Named pipe path/lifetime behavior may need a marker file distinct from the
  actual transport endpoint.
- ConPTY EOF and process-exit ordering may differ from Unix PTY behavior.
- Dynamic `libghostty-vt` loading on Windows may complicate distribution.

## Validation

Baseline Windows checks:

```powershell
cargo check --locked
cargo test --workspace --locked
```

Unix parity checks remain:

```bash
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```
