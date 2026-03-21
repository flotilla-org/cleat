# Remove meta.json: Pass Session Config as Daemon Args

**Goal:** Eliminate `meta.json` as a configuration and signalling mechanism. Session config moves to `cleat serve` CLI args. Session enumeration moves to live `Inspect` frames over daemon sockets.

**GitHub issue:** flotilla-org/cleat#13

---

## Background

`meta.json` currently serves three roles that conflict:

1. **Session existence check** — every service method guards with `meta.json.exists()`, but directory existence suffices.
2. **Daemon startup config** — the daemon reads `meta.json` once to learn its VT engine, command, cwd, and record flag. But the CLI process that wrote the file also spawns the daemon, so it can pass these as args directly.
3. **Mutable state persistence** — `service.record()` read-modify-writes `meta.json` to persist the record flag across daemon restarts. But daemon restart kills the PTY child, so the session is effectively dead anyway. Recording state doesn't need persistence.

Removing `meta.json` simplifies the codebase, eliminates the race condition in `create --record` (where the daemon reads the file before the CLI can patch it), and removes the only read-modify-write pattern on a shared file.

## Changes

### 1. Pass config as `serve` args, update the full call chain

The `Serve` CLI command gains flags matching session config:

```
cleat serve --id <id> --vt <engine> [--cmd <cmd>] [--cwd <path>] [--record]
```

The call chain updates end-to-end:

- **`Command::Serve`** — gains `vt: VtEngineKind`, `cmd: Option<String>`, `cwd: Option<PathBuf>`, `record: bool` fields.
- **`SessionService::serve()`** — signature changes from `serve(&self, id: &str)` to accept the full config (or a `SessionMetadata`). Passes it through to `run_session_daemon`.
- **`run_session_daemon()`** — signature changes from `(root: &Path, id: &str)` to `(root: &Path, session: &SessionMetadata)` (or equivalent). Stops calling `load_session()`. Uses the passed-in config directly.
- **`spawn_daemon_process()`** — builds argv from `SessionMetadata` fields: `--id`, `--vt`, `--cmd`, `--cwd`, `--record`.
- **`cli::execute()` for `Command::Serve`** — constructs `SessionMetadata` from the parsed CLI args and passes it to `service.serve()`.

### 2. Collapse `name` into `id`

`SessionMetadata.name` is always either identical to `id` or `None`. Drop it. One identifier everywhere:

- `SessionMetadata` — remove `name` field
- `SessionInfo` — remove `name` field
- `SessionInspect` — remove `name` field
- `build_inspect_result()` — stop populating `name`

The `create` and `attach` CLI commands currently accept an optional positional `ID`. This stays the same — the value becomes `SessionMetadata.id` directly, with a UUID fallback when omitted.

### 3. Session existence = directory existence

Replace all `root.join(id).join("meta.json").exists()` guards with `root.join(id).exists()`. This applies to every service method: `kill`, `detach`, `capture`, `send_keys`, `inspect`, `signal`, `record`.

`create_session` stops writing `meta.json`. It creates the directory and returns `SessionMetadata` (built from args, not from a file).

### 4. `ensure_session_started` detects existing sessions by directory

`ensure_session_started()` currently calls `load_session()` to detect a pre-existing session. After `load_session` is deleted, it checks `root.join(id).exists()` instead. If the directory exists, it assumes the session was previously created and skips to checking/spawning the daemon. The `SessionMetadata` for a reused session is not needed from disk — the daemon is already running (or will be respawned with fresh args from the caller).

For the `attach --no-create` path: `SessionService::attach()` currently calls `list_sessions()` to find the session by ID. Replace this with a directory existence check (`root.join(id).exists()`). If the directory exists and has a socket, the session is live. If not, return the "missing session" error. The `SessionMetadata` fields (vt_engine, cmd, cwd) are not needed for attach — the daemon already has them.

### 5. `list` uses live `Inspect` frames

`RuntimeLayout::list_sessions()` is replaced. The new flow in `SessionService::list()`:

1. Scan subdirectories of the runtime root.
2. For each directory that has a `socket` file, connect and send `Frame::Inspect`.
3. Build `SessionInfo` from the `InspectResult` response. Map `attachments` to `SessionStatus`: non-empty attachments = `Attached`, empty = `Detached`.
4. Directories without a socket (or where the connect/inspect fails) are either not yet started or stale — skip them silently, or report them as dead if the directory has a `daemon.pid` file with a non-running PID.

This eliminates `RuntimeLayout::list_sessions()` and `SessionRecord`. `SessionService::list()` does the socket scan directly.

### 6. `create --record` simplifies

Currently: create session, retry-loop `service.record()` to poke the daemon.

After: `spawn_daemon_process` passes `--record` to `cleat serve`. The daemon sees `--record` in its own argv and enables recording on startup. No retry loop, no post-hoc patching.

### 7. `service.record()` stops persisting

`service.record()` sends a `RecordControl` frame to the live daemon. It no longer reads/writes `meta.json`. The existence guard changes from `meta.json.exists()` to `root.join(id).exists()` (per change 3). Recording is a runtime-only setting.

### 8. Remove `load_session` and `write_metadata`

- `load_session()` in `session.rs` — deleted. The daemon gets config from CLI args.
- `RuntimeLayout::write_metadata()` — deleted. Nothing writes `meta.json`.
- `RuntimeLayout::create_session()` — simplified. Creates the directory, returns `SessionMetadata` built from args. No file I/O beyond `create_dir_all`.
- `RuntimeLayout::list_sessions()` — deleted. Replaced by socket-based inspection in `SessionService::list()`.
- `SessionRecord` struct — deleted. Was a wrapper around directory path + metadata from file.

## What doesn't change

- Protocol frames (Inspect, InspectResult, Signal, RecordControl, etc.)
- Recording module (`OutputRecorder`, snapshots)
- Signal dispatch
- VT engine selection and lifecycle
- Attach/detach flow (the socket handshake, relay_stdio, etc.)
- The `daemon.pid` file (still used by `kill` to find the daemon process; the existing double-write from both parent and daemon is unchanged)
- The `socket` file (still the daemon's Unix socket)
- The `foreground` marker file (still used for attach exclusion)

## Test impact

- Tests that manually write `meta.json` to simulate sessions (e.g., `kill_does_not_signal_unrelated_process`, `send_keys_writes_frame_to_session_socket`) need updating — create a directory instead (no meta.json needed for existence checks).
- `list` tests change from checking file-based metadata to checking inspect-based responses. These tests require a running daemon. Use the existing `service.create()` pattern (which spawns a daemon) rather than manual directory setup. For unit tests of `list` without real daemons, spin up a mock `UnixListener` that responds to `Inspect` frames.
- CLI parsing tests for `Serve` need updating to cover new flags (`--vt`, `--cmd`, `--cwd`, `--record`).
- `create --record` test simplifies — recording is activated by daemon startup arg, no retry loop needed.
- `attach --no-create` tests need updating — no longer calls `list_sessions()`, uses directory existence instead.
