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
LD_LIBRARY_PATH="$PWD/.tools/ghostty-install/lib" cargo build -p cleat --locked --features ghostty-vt
LD_LIBRARY_PATH="$PWD/.tools/ghostty-install/lib" cargo test -p cleat --locked --features ghostty-vt
```

On **macOS**:
```bash
DYLD_LIBRARY_PATH="$PWD/.tools/ghostty-install/lib" cargo build -p cleat --locked --features ghostty-vt
DYLD_LIBRARY_PATH="$PWD/.tools/ghostty-install/lib" cargo test -p cleat --locked --features ghostty-vt
```

The helper reads pinned inputs from [`tools/ghostty-toolchain.toml`](tools/ghostty-toolchain.toml), verifies Zig `0.15.2`, clones or refreshes Ghostty into `.tools/ghostty-src`, and installs the Ghostty VT headers and shared library into `.tools/ghostty-install`.

The `ghostty-vt` build path defaults to the repo-local prefix at `.tools/ghostty-install`. You can still override it with `CLEAT_GHOSTTY_PREFIX`, but feature-on runs and tests must set the library path (`LD_LIBRARY_PATH` on Linux, `DYLD_LIBRARY_PATH` on macOS) so the loader can find the shared library.

```bash
find .tools/ghostty-install -maxdepth 3 | sort
```

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

## Behavioral Model

Three layers cooperate during a session. Knowing which layer is authoritative for which behavior is the main thing to internalize before debugging with cleat — it's the most common source of confusion.

### Layers

- **Host terminal** — your real terminal emulator (kitty, ghostty, iTerm, Terminal.app, etc). In play *only while a client is attached*. Renders output to you, supplies keyboard input, and answers the child's capability queries (DA, DSR, kitty/sixel protocol queries) with whatever the host terminal actually supports.
- **VT engine** — cleat's internal terminal emulator (libghostty with `--features ghostty-vt`; the `passthrough` engine is a placeholder for testing). Always active. Parses child PTY output into a structured screen grid, tracks modes/cursor/styles, and — when *detached* — synthesizes replies to capability queries so the child's detection logic doesn't stall.
- **Recording** — optional raw PTY output tee, stored as asciicast v3 in `session.cast`. Authoritative source for `transcript` and `expect`. Enabled per-session with `--record` or globally via `CLEAT_RECORD=1`.

### Command → layer map

| Command | Exercises | Notes |
|---|---|---|
| `launch` | daemon + VT engine | Creates session, spawns daemon, initializes VT engine |
| `attach` / `detach` | host terminal + daemon | While attached, host terminal is authoritative for query replies |
| `list`, `inspect`, `kill`, `signal` | daemon state | No VT / recording involvement |
| `capture` | VT engine | Renders the current screen grid to text; errors on the `passthrough` engine |
| `transcript`, `expect` | recording | Reads raw bytes from asciicast; no re-rendering |
| `send`, `send-keys`, `interrupt`, `escape` | daemon → PTY | Writes to child stdin via the PTY master |
| `record`, `mark` | recording | Mutates recording state |
| `wait --idle-time` | daemon | PTY-output idle timer |
| `wait --text` | VT engine | Consults the rendered screen grid |

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
