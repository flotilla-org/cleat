# cleat

Session daemon with a structured control plane for agents and terminal persistence.

## Status

**Ghostty is currently the only functional VT engine.**

Builds without `ghostty-vt` are non-functional placeholder builds for real usage. The current `passthrough` engine is a placeholder/test-only seam, not a real VT engine.

This repository is being split out from the Flotilla monorepo. The first standalone import keeps the existing `cleat` crate, tests, and the optional `ghostty-vt` integration path, but only the Ghostty-backed build is intended for actual terminal use.

A future Rust VT engine may be added later. Until then, treat Ghostty as the only supported functional engine.

## Development

Default development builds still compile without Ghostty so contributors can work in the repo, but those binaries are intentionally incomplete for real use.

```bash
cargo build --locked
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

## Functional Ghostty Build

Use the repo-local helper to fetch the pinned Ghostty ref and build a local install prefix under `.tools/`, then build `cleat` with `ghostty-vt` enabled.

```bash
./tools/prepare-ghostty-vt.sh
```

On **Linux**:
```bash
cargo build -p cleat --locked --features ghostty-vt
cargo test -p cleat --locked --features ghostty-vt
```

On **macOS**:
```bash
cargo build -p cleat --locked --features ghostty-vt
cargo test -p cleat --locked --features ghostty-vt
```

On **Windows**:
```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File tools\prepare-ghostty-vt.ps1
$env:CLEAT_GHOSTTY_PREFIX = (Resolve-Path .tools\ghostty-install).Path
cargo build -p cleat --locked --features ghostty-vt
cargo test -p cleat --locked --features ghostty-vt
```

The helpers read pinned inputs from [`tools/ghostty-toolchain.toml`](tools/ghostty-toolchain.toml), verify or install Zig `0.15.2`, clone or refresh Ghostty into `.tools/ghostty-src`, and install the Ghostty VT headers and libraries into `.tools/ghostty-install`.

The `ghostty-vt` build path defaults to the repo-local prefix at `.tools/ghostty-install`. You can still override it with `CLEAT_GHOSTTY_PREFIX`. Cleat prefers the static Ghostty VT library on Unix when present (`libghostty-vt.a`) and falls back to the shared library otherwise. On Windows, cleat links against `ghostty-vt.lib` and copies `ghostty-vt.dll` next to the built executable.

```bash
find .tools/ghostty-install -maxdepth 3 | sort
```

## Session Model

**Named daemons host sets of sessions.** A daemon is the process boundary for one named session set. The default daemon is named `default`; pass `--server NAME` to address a different daemon. A session address is therefore `(daemon, id)`, with an unqualified ID meaning "this ID in the selected daemon."

**Session IDs.** You choose the ID (`cleat launch my-session`) or let cleat generate one (`session-<uuid>`). IDs are directory names under their daemon's `sessions/` directory, so use filesystem-safe characters. Launching with an ID that already has a live session in the selected daemon reuses that session; it does not create a duplicate.

**Tags.** Sessions may carry flat, opaque tags. Add them at launch with repeated `--tag TAG`, mutate them with `cleat tag <id> +TAG -TAG`, and filter directory reads with repeated `--selector TAG`. Selectors are exact whole-tag matches and are ANDed when repeated. `key=value` is only a client convention; cleat does not interpret tag keys, values, hierarchy, or globs.

**Runtime directory.** Discovered in priority order:

1. `$CLEAT_RUNTIME_DIR` (if set)
2. `$XDG_RUNTIME_DIR/cleat` (if `XDG_RUNTIME_DIR` is set)
3. `$TMPDIR/cleat-<uid>`
4. `/tmp/cleat-<uid>`

Runtime layout v2 is daemon-scoped:

```text
<runtime-root>/
  <daemon-name>/
    socket
    daemon.pid
    sessions/
      <session-id>/
        session.cast
        foreground
```

`socket` and `daemon.pid` belong to the daemon, not to an individual session. Session directories live under `sessions/`. `session.cast` is the asciicast v3 recording when recording is active. `foreground` is a transient attachment marker.

**Liveness and discovery.** Session liveness is daemon state, not a per-session socket stat. `cleat list` queries the selected daemon. `cleat list --all` enumerates every daemon directory under the runtime root and queries or sweeps each daemon independently. If a daemon is definitively stale, cleat performs a daemon-scoped sweep: sessions with a non-empty recording are preserved as recreatable, and sessions without a recording are removed.

**Linger and cleanup.** A daemon starts on first use of its name. When it has no live sessions, it lingers for 120 seconds before exiting so a burst of commands does not repeatedly bounce the daemon. When a child process exits, its session is removed unless it has a recording that makes it recreatable.

**Recording and recreation.** CLI-created sessions record by default. Use `--no-record` to opt out, and `cleat record <id>` to enable recording on a running session. Recording is the persistence floor: a daemon crash or host reboot loses the PTY and process state, but a preserved recording can seed scrollback when the session is recreated.

## Behavioral Model

Four surfaces cooperate during a session. Knowing which surface is authoritative for which behavior is the main thing to internalize before debugging with cleat.

### Surfaces

- **Host terminal** — your real terminal emulator (kitty, ghostty, iTerm, Terminal.app, etc). In play *only while a client is attached*. Renders output to you, supplies keyboard input, and answers the child's capability queries (DA, DSR, kitty/sixel protocol queries) with whatever the host terminal actually supports.
- **VT engine** — cleat's internal terminal emulator (libghostty with `--features ghostty-vt`; the `passthrough` engine is a placeholder for testing). Always active. Parses child PTY output into a structured screen grid, tracks modes/cursor/styles, and — when *detached* — synthesizes replies to capability queries so the child's detection logic doesn't stall.
- **Recording** — default-on raw PTY output tee, stored as asciicast v3 in `session.cast`. Authoritative source for `transcript` and `expect`.
- **Packet surface** — structured multiplexed control/render/directory protocol. `cleat packets` exposes the raw probe surface, and `cleat list --watch` uses the directory subscription to print a snapshot followed by lifecycle deltas.

### Command Map

| Command | Exercises | Notes |
|---|---|---|
| `--server NAME` | daemon selection | Selects the named daemon for the command; default is `default` |
| `launch [--tag TAG]... [--record|--no-record]` | daemon + VT engine + recording | Creates or reuses a session in the selected daemon |
| `tag <id> +TAG -TAG` | daemon directory state | Mutates opaque tags; tags are not interpreted by cleat |
| `attach` / `detach` | host terminal + daemon | While attached, host terminal is authoritative for query replies |
| `watch` | host terminal + daemon | Read-only live view; does not take foreground control |
| `list [--selector TAG]...` | daemon directory state | One-shot read of the selected daemon's directory |
| `list --all` | daemon directory state | Enumerates every daemon directory under the runtime root |
| `list --watch [--selector TAG]...` | packet directory subscription | Prints a snapshot, then one line per directory delta |
| `packets` | packet protocol | Opens the structured multiplexed probe surface |
| `inspect`, `kill`, `signal` | daemon state | No VT / recording involvement |
| `capture` | VT engine | Renders the current screen grid to text; errors on the `passthrough` engine |
| `transcript`, `expect` | recording | Reads raw bytes from asciicast; no re-rendering |
| `send`, `send --submit`, `send-keys`, `interrupt`, `escape` | daemon → PTY | Writes to child stdin via the PTY master; `--submit` sends paste/text then Enter |
| `record`, `mark` | recording | Enables recording or writes a marker |
| `wait --idle-time` | daemon | PTY-output idle timer |
| `wait --text` | VT engine | Consults the rendered screen grid |
| `wait --screen-stable` | VT engine | Waits for the rendered screen grid to stop changing |

### Queries and capabilities

When the child emits a capability query, the reply source depends on attach state:

- **Attached** — the host terminal replies. Whatever your real terminal actually supports is what the child sees. Behavior matches running the child outside cleat.
- **Detached** — the VT engine (libghostty) synthesizes replies.

Currently answered by the VT engine in detached mode:

| Query | Reply |
|---|---|
| DA1 (`CSI c`) | `\x1b[?62;22c` (conformance level 62 = VT220, feature 22 = ANSI color) |
| DA2 (`CSI > c`) | `\x1b[>1;10;0c` (device type 1 = VT220, firmware 10, cartridge 0) |
| DA3 (`CSI = c`) | DECRPTUI response with unit ID 0 |
| DSR, including Cursor Position Report (`CSI 6 n`) | computed from VT state (e.g. `\x1b[row;colR`) |
| DECRQM (mode reports) | computed from VT mode state |

Currently dropped (no reply sent, even in detached mode):

- ENQ (`0x05`)
- XTVERSION (`CSI > q`)
- XTWINOPS size queries (`CSI 14/16/18 t`)
- Color-scheme query (`CSI ? 996 n`)
- Kitty keyboard protocol queries (`CSI ? u`)
- Kitty graphics protocol queries (`APC G ... q=... ST`)
- XTGETTCAP (`DCS + q ... ST`)

The first four have structurally identical fixes to the DA/DSR wiring and will likely land as a follow-up. The kitty-protocol and XTGETTCAP entries need upstream libghostty work or a cleat-side sniffer — tracked in the issue list.

### Common surprises

- **`capture` shows what the VT engine parsed** from the output stream — not necessarily what your real terminal would display. Usually identical, but diverges for kitty graphics: the VT engine doesn't surface image content today, while an attached host terminal would render the images.
- **Attached and detached sessions may behave differently for the same child program** if the child branches on capability-query responses. A TUI that probes for kitty graphics via `APC G ... q=... ST` sees support when attached to kitty and no support when detached (the query is currently dropped). Reproducible behavior for protocol-sensitive stages requires picking the right mode. This asymmetry is a known design question, not a target — see [#58](https://github.com/flotilla-org/cleat/issues/58) for the direction (VT engine always authoritative, host terminal as a derived view).
- **Recording is raw PTY output** with escape sequences intact. `transcript` emits them verbatim; use `capture` to get human-readable text from the current screen state.
- **Non-Ghostty builds return errors** for `capture` and other VT-dependent operations. The `passthrough` engine is a test seam, not a real VT. A functional binary requires `--features ghostty-vt`.
