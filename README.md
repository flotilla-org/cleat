# cleat

Session daemon with a structured control plane for agents and terminal persistence.

See [ROADMAP.md](ROADMAP.md) for the current priorities, decisions and issue links.

## Status

**Ghostty is currently the only functional VT engine.**

Builds without `ghostty-vt` are non-functional placeholder builds for real usage. The current `passthrough` engine is a placeholder/test-only seam, not a real VT engine.

This repository is being split out from the Flotilla monorepo. The first standalone import keeps the existing `cleat` crate, tests, and the optional `ghostty-vt` integration path, but only the Ghostty-backed build is intended for actual terminal use.

A future Rust VT engine may be added later. Until then, treat Ghostty as the only supported functional engine.

## Attachment cycle safety

Daemons reject direct and indirect output cycles across local daemons, including
read-only watchers and packet channels. Acyclic nesting remains available with a
visible containing-session indicator. This requires upgraded clients **and**
daemons; restart old clients, daemons, and containing sessions before claiming
protection. Older clients receive an upgrade-required error. SSH/remote output
subscriptions are admitted untracked, with an acknowledgement warning that cycle
protection does not cover remote relationships. No graph edge is recorded for
them. See [output admission and rollout](docs/output-cycle-admission.md).

## Development

Development builds use Ghostty by default. The explicit `--no-default-features` build is available for work on the Rust-only placeholder path.

```bash
./tools/prepare-ghostty-vt.sh   # once per checkout (fetches + builds the pinned Ghostty VT)
cargo build --locked
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

The `ghostty-vt` feature is a **default feature**: a plain `cargo build` produces a functional binary, and fails with an actionable message if the prepared Ghostty install is missing. Build the VT-less variant (placeholder engine, testing only) with `--no-default-features`.

## Functional Ghostty Build

Use the repo-local helper to fetch the pinned Ghostty ref and build a local install prefix under `.tools/`; `ghostty-vt` is enabled by default, so a normal build picks it up.

```bash
./tools/prepare-ghostty-vt.sh
```

On **Linux** and **macOS**:
```bash
cargo build -p cleat --locked
cargo test -p cleat --locked
```

On **Windows**:
```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File tools\prepare-ghostty-vt.ps1
$env:CLEAT_GHOSTTY_PREFIX = (Resolve-Path .tools\ghostty-install).Path
cargo build -p cleat --locked
cargo test -p cleat --locked
```

The helpers read pinned inputs from [`tools/ghostty-toolchain.toml`](tools/ghostty-toolchain.toml), verify or install Zig `0.16.0`, clone or refresh Ghostty into `.tools/ghostty-src`, and install the Ghostty VT headers and libraries into `.tools/ghostty-install`.

The `ghostty-vt` build path defaults to the repo-local prefix at `.tools/ghostty-install`. You can still override it with `CLEAT_GHOSTTY_PREFIX`. Cleat prefers the static Ghostty VT library on Unix when present (`libghostty-vt.a`) and falls back to the shared library otherwise. On Windows, cleat links against `ghostty-vt.lib` and copies `ghostty-vt.dll` next to the built executable.

On Windows, sessions run under the bundled ConPTY ([ADR 0006](docs/adr/0006-bundled-conpty-on-windows.md)) so Kitty graphics and sixel reach the VT engine; the inbox ConPTY drops them. `prepare-ghostty-vt.ps1` also runs `tools\prepare-conpty.ps1`, which fetches the package pinned in [`tools/conpty.toml`](tools/conpty.toml) from nuget.org and verifies its SHA-256. The build copies `conpty.dll`, `OpenConsole.exe` and `conpty-LICENSE.txt` next to the built executables. Without them, sessions fall back to the inbox ConPTY and `cleat list`/`inspect` report the degradation. Set `CLEAT_CONPTY=inbox` in the daemon's environment to force the fallback.

```bash
find .tools/ghostty-install -maxdepth 3 | sort
```

## Session Model

**Named daemons host sets of sessions.** Each physical daemon has a positive, monotonically increasing *generation*, stored at `<root>/<name>@<generation>/`. `<root>/<name>` aliases the current generation (a symlink on Unix, a text file on Windows). New sessions target the current generation; session-ID commands search the selected name’s generations and refuse ambiguous IDs. Use `--server name@N` to select one generation. Auto-start creates generation 1 initially, or advances after the current daemon dies; it never creates a successor to a live daemon. A live legacy directory stays in place; a dead legacy directory is adopted as generation 1. Retained sessions in dead generations remain recreatable: `attach` through the logical name adopts their recording into the current generation. Explicit generation addresses stay pinned and report a dead host instead of auto-starting it. Each session has a separate *hosting epoch*, initially 1.

After installing a new fleet generation, run `cleat server drain --server NAME` once for each logical daemon on the host (`default` when omitted). It compares the running daemon's git SHA and packet protocol to the installed client. A matching build reports `nothing to drain`; otherwise it starts the installed executable in the next generation, waits up to a bounded health deadline (10 seconds plus an in-flight request), atomically publishes the alias, and asks the old generation to drain. `--json` reports `changed`, `installed`, `old`, `current`, and an optional `warning`, including build identities and the old generation's remaining session count. A startup or health failure leaves the alias unchanged. Failed attempts retain their reserved generation directories, and an unpublished daemon that started follows the normal idle-exit policy; automatic reclamation and retrying an unfinished retirement are tracked in [#265](https://github.com/flotilla-org/cleat/issues/265). If publication succeeds but the old daemon's drain request fails unexpectedly, drain reports that the alias moved; a retry sees the new current build and does not retry the old request. The installed client must have a known git SHA so an unknown revision cannot silently count as a matching build. Explicit generation targets are rejected; drain always operates on a logical name.

A draining daemon refuses new session IDs, keeps existing sessions and attachments running, and exits as soon as its last session ends. Recordings remain available for recreation; empty drained generation directories are removed. Session-ID commands through the logical name still find the old host, while `cleat launch` uses the new host. Across a packet protocol change, live old sessions require the matching old client until they exit; new sessions work with the installed client. Keep the previous binary available during the roll. No PTYs are moved.

For a live legacy host, its directory remains at `<root>/<name>/` and the alias is published at `<root>/.<name>.current` instead; `--server name@legacy` addresses that host directly. Older binaries that do not understand drain are left serving with a warning and a successful exit: new clients still launch sessions on the successor. `cleat version --daemon` compares the installed and selected daemon builds; `cleat daemons` shows every discovered generation's build identity and `serving`/`draining` state. Drain fails if the selected current daemon cannot be reached; ordinary auto-start remains responsible for starting an absent or dead daemon.

A daemon is the process boundary for one named session set. The default daemon is named `default`; pass `--server NAME` to address a different daemon. Without `--server`, commands target the ambient daemon named by `$CLEAT_DAEMON` when running inside a session, and `default` otherwise. An explicit `--server` always wins. A session address is therefore `(daemon, id)`, with an unqualified ID meaning "this ID in the selected daemon."

**Build identity.** `cleat --version` includes the Git revision, tracked-file dirty status, build profile, optimization level, target, packet protocol version, and VT engine. `cleat version --daemon` also reports the selected running daemon's build without starting or restarting it; add `--json` for structured metadata. Older daemons report an unknown build. Builds made without Git metadata report an unknown revision and dirty status. The daemon exposes the same metadata in the `build` field of `GET /` and `GET /healthz` on its control socket.

**Session IDs.** You choose the ID (`cleat launch my-session`) or let cleat generate one (`session-<uuid>`). IDs are directory names under their daemon's `sessions/` directory, so use filesystem-safe characters. Launching with an ID that already has a live session in the selected daemon reuses that session; it does not create a duplicate.

**Sibling sessions.** `cleat launch [ID] --from SOURCE` resolves the daemon that owns `SOURCE` and applies the normal launch behavior there. The new session shares that daemon's initial execution context, not the source session's current cwd or exported variables. `--from` and `--server` are mutually exclusive; use `--server` directly when the same source ID exists in more than one daemon.

**Tags.** Sessions may carry flat, opaque tags. Add them at launch with repeated `--tag TAG`, mutate them with `cleat tag <id> +TAG -TAG`, and filter directory reads with repeated `--selector TAG`. Selectors are exact whole-tag matches and are ANDed when repeated. `key=value` is only a client convention; cleat does not interpret tag keys, values, hierarchy, or globs.

**State directory.** The daemon registration, control socket, session state, and recordings share one root, discovered in priority order:

1. `$CLEAT_RUNTIME_DIR` (if set)
2. `$XDG_STATE_HOME/cleat` (if `XDG_STATE_HOME` is an absolute path)
3. `$HOME/.local/state/cleat` on Unix, or `%LOCALAPPDATA%\cleat` on Windows

Discovery fails with an actionable error when no persistent state directory is available; cleat never implicitly stores daemon state or recordings in a temporary directory. `CLEAT_RUNTIME_DIR` retains its historical name as the explicit whole-layout override. Use a short persistent path for this override if the discovered Unix socket path exceeds the platform limit; cleat rejects an unusable socket path before starting a daemon.

Every daemon exports its coordinates into each session child, following the same pattern as tmux's `$TMUX` variable:

- `$CLEAT_RUNTIME_DIR` — the daemon's state root, including private roots
- `$CLEAT_DAEMON` — the daemon name used for ambient command targeting
- `$CLEAT_SESSION` — the current session ID
- `$CLEAT_OUTPUT_DAEMON` — the physical daemon generation used for output-cycle admission

This makes bare commands inside a session use that session's daemon and state root. `cleat daemons` discovers daemon directories at the ambient root and the well-known XDG/platform roots. Discovery is intentionally best-effort, not exhaustive: private roots that are not ambient can remain undiscoverable, and each daemon's own Directory remains authoritative for its sessions. Use `cleat daemons --json` for structured `{name, runtime_root, generation, alive, drain_state, build}` entries. `generation` is null for legacy hosts, and `drain_state` is `serving` or `draining`. Build identity is retained for dead generations.

Runtime layout v2 is daemon-scoped:

```text
<state-root>/
  <daemon-name>/
    socket
    daemon.pid
    sessions/
      <session-id>/
        session.cast
        foreground
```

`socket` and `daemon.pid` belong to the daemon, not to an individual session. Session directories live under `sessions/`. `session.cast` is the asciicast v3 recording when recording is active. `foreground` is a transient attachment marker.

**Liveness and discovery.** Session liveness is daemon state, not a per-session socket stat. `cleat list` queries the selected daemon. `cleat list --all` enumerates every daemon directory under the state root and queries or sweeps each daemon independently. If a daemon is definitively stale, cleat performs a daemon-scoped sweep: sessions with a non-empty recording are preserved as recreatable, and sessions without a recording are removed.

**Linger and cleanup.** A daemon starts on first use of its name. When it has no live sessions, it lingers for 120 seconds before exiting so a burst of commands does not repeatedly bounce the daemon. When a child process exits, its session is removed unless it has a recording that makes it recreatable.

**Unix termination.** `cleat kill <id>` (HTTP `DELETE /sessions/{id}`) sends TERM to the session tree, then KILL to surviving processes after a two-second grace period. DELETE acknowledges the request with 204 before the grace period ends; other sessions remain serviceable. Tree signals include the leader group, the foreground job-control group, and discoverable descendants, including children that called `setsid`. Cleanup retains process identities across leader exit and includes descendants born during grace. Descendant walking is best-effort: children already reparented before the initial snapshot cannot be recovered by ancestry. Explicit `signal --target tree` sends only the requested signal and does not schedule escalation.

**Recording and recreation.** CLI-created sessions record by default. Use `--no-record` to opt out, and `cleat record <id>` to enable recording on a running session. Recording is the persistence floor: a daemon crash or host reboot loses the PTY and process state, but a preserved recording can seed scrollback when the session is recreated.

## Behavioral Model

Four surfaces cooperate during a session. Knowing which surface is authoritative for which behavior is the main thing to internalize before debugging with cleat.

### Surfaces

- **Host terminal** — your real terminal emulator (kitty, ghostty, iTerm, Terminal.app, etc). In play *only while a client is attached*. Renders output to you, supplies keyboard input, and answers the child's capability queries (DA, DSR, kitty/sixel protocol queries) with whatever the host terminal actually supports.
- **VT engine** — cleat's internal terminal emulator (libghostty with `--features ghostty-vt`; the `passthrough` engine is a placeholder for testing). Always active. Parses child PTY output into a structured screen grid, tracks modes/cursor/styles, and — when *detached* — synthesizes replies to capability queries so the child's detection logic doesn't stall.
- **Recording** — default-on raw PTY output tee, stored as asciicast v3 in `session.cast`. Authoritative source for `transcript` and `expect`.
- **Packet surface** — structured multiplexed control/render/directory protocol. `cleat packets` exposes the raw probe surface, and `cleat list --watch` uses the directory subscription to print a snapshot followed by lifecycle deltas. Rust clients can use `SessionService::connect_activity` to subscribe once for every session matching a tag selector: the stream starts with an activity snapshot, then emits threshold-based `active`/`stable` transitions and membership changes with session IDs, tags, and Unix-millisecond timestamps.

Screen activity has one meaning across JSON polling and packet subscriptions: a session is `active` only after PTY output changes its rendered screen, so terminal queries do not count. A never-rendered session starts `stable`, and an engine that cannot observe render damage remains `stable`. JSON uses a fixed one-second stability threshold; each packet subscriber applies its requested threshold to the same render-change timeline. `stable_since` on JSON is when stability was reached, while packet `stable_since_unix_ms` is the beginning of the quiet window that will reach the subscriber's threshold.

This subscription contract starts with packet protocol v4. Version 3 used raw PTY-output timing for subscriptions, so v3 clients and daemons are rejected instead of silently disagreeing about query-only activity. The activity message shapes are otherwise unchanged.

### Command Map

| Command | Exercises | Notes |
|---|---|---|
| `--server NAME` | daemon selection | Explicit daemon selection; otherwise `$CLEAT_DAEMON`, then `default` |
| `daemons [--json]` | daemon discovery | Best-effort discovery across ambient and well-known state roots |
| `launch [--from SOURCE] [--tag TAG]... [--record|--no-record]` | daemon + VT engine + recording | Creates or reuses a session in the selected daemon, or in `SOURCE`'s daemon with `--from` |
| `tag <id> +TAG -TAG` | daemon directory state | Mutates opaque tags; tags are not interpreted by cleat |
| `attach` / `detach` | host terminal + daemon | While attached, host terminal is authoritative for query replies |
| `watch` | host terminal + daemon | Read-only live view; does not take foreground control |
| `list [--selector TAG]...` | daemon directory state | One-shot read of the selected daemon's directory |
| `list --all` | daemon directory state | Enumerates every daemon directory under the state root |
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

For Ghostty packet sessions, the session's VT engine answers application queries
whether or not a viewer is attached. CLI and native viewers render that state;
the CLI negotiates keyboard, mouse and image delivery with its outer terminal
separately. See [structured input](docs/structured-keyboard.md).

Child terminal identity also belongs to the engine. Ghostty sessions prefer
`TERM=xterm-ghostty` when the session host can resolve its terminfo entry,
otherwise `xterm-256color`. They set `TERM_PROGRAM=ghostty` and
`COLORTERM=truecolor`; explicit session environment overrides win. The
passthrough test engine uses `TERM=dumb`. See [terminal identity](docs/terminal-identity.md)
for lookup, platform and compatibility details.

### Common surprises

- **Text `capture` reports the VT engine's screen text.** Graphics resources and placements are delivered separately to packet/native viewers; text capture is not an image export.
- **Recording is raw PTY output** with escape sequences intact. `transcript` emits them verbatim; use `capture` to get human-readable text from the current screen state.
- **Non-Ghostty builds return errors** for `capture` and other VT-dependent operations. The `passthrough` engine is a test seam, not a real VT. A functional binary requires `--features ghostty-vt`.
