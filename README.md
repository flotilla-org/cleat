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
