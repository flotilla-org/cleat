# meta.json Removal Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove `meta.json` as a configuration and signalling mechanism. Session config passes as `cleat serve` CLI args. Session enumeration uses live `Inspect` frames.

**Architecture:** The `Serve` command gains `--vt`, `--cmd`, `--cwd`, `--record` flags. `spawn_daemon_process` builds argv from `SessionMetadata`. `run_session_daemon` takes config directly instead of reading from disk. `list` connects to each session's daemon socket and sends `Inspect`. The `name` field is collapsed into `id` everywhere.

**Tech Stack:** Rust, clap (CLI), serde_json (inspect serialization)

**Spec:** `docs/specs/2026-03-21-meta-json-removal.md`
**GitHub issue:** flotilla-org/cleat#13

---

## File Structure

### Modified files

| File | Changes |
|------|---------|
| `crates/cleat/src/protocol.rs` | Remove `name` from `SessionInfo`, `SessionInspect` |
| `crates/cleat/src/runtime.rs` | Remove `name` from `SessionMetadata`, remove `SessionRecord`, `write_metadata`, `list_sessions`; simplify `create_session` |
| `crates/cleat/src/cli.rs` | Add flags to `Serve` command, simplify `create --record`, update `execute` for new `Serve` signature |
| `crates/cleat/src/server.rs` | Replace `meta.json` existence checks with directory checks, rewrite `list()` using `Inspect` frames, simplify `record()`, update `attach` no-create path, update `serve()` signature |
| `crates/cleat/src/session.rs` | Update `run_session_daemon` to take `SessionMetadata`, update `spawn_daemon_process` to pass args, update `ensure_session_started` to use directory existence, remove `load_session`, update `build_inspect_result` |
| `crates/cleat/tests/cli.rs` | Update tests for removed `name`, new `Serve` flags |
| `crates/cleat/tests/lifecycle.rs` | Update tests for removed `name`, simplified `create --record` |

---

### Task 1: Remove `name` field everywhere

**Files:**
- Modify: `crates/cleat/src/protocol.rs`
- Modify: `crates/cleat/src/runtime.rs`
- Modify: `crates/cleat/src/server.rs`
- Modify: `crates/cleat/src/session.rs`
- Modify: `crates/cleat/src/cli.rs`
- Modify: `crates/cleat/tests/cli.rs`
- Modify: `crates/cleat/tests/lifecycle.rs`

- [ ] **Step 1: Remove `name` from `SessionMetadata`**

In `crates/cleat/src/runtime.rs`, remove `pub name: Option<String>` from `SessionMetadata`. Update `create_session` — the `name` parameter becomes the `id` directly (with UUID fallback):

```rust
pub fn create_session(
    &self,
    id: Option<String>,
    vt_engine: VtEngineKind,
    cwd: Option<PathBuf>,
    cmd: Option<String>,
) -> Result<SessionRecord, String> {
    self.ensure_root()?;

    let id = id.unwrap_or_else(|| format!("session-{}", Uuid::new_v4()));
    let dir = self.root.join(&id);
    fs::create_dir_all(&dir).map_err(|err| format!("create session dir {}: {err}", dir.display()))?;
    let metadata = SessionMetadata { id, vt_engine, cwd, cmd, record: false };
    self.write_metadata(&metadata)?;
    Ok(SessionRecord { dir, metadata })
}
```

- [ ] **Step 2: Remove `name` from `SessionInfo`**

In `crates/cleat/src/protocol.rs`, remove `pub name: Option<String>` from `SessionInfo`.

- [ ] **Step 3: Remove `name` from `SessionInspect`**

In `crates/cleat/src/protocol.rs`, remove `pub name: Option<String>` from `SessionInspect`.

- [ ] **Step 4: Update all code that populates `name`**

In `crates/cleat/src/server.rs`:
- `create()` — remove `name: session.name,` from `SessionInfo` construction (line 38)
- `attach()` — remove `name: session.name,` from `SessionInfo` construction (line 130)
- `session_info_from_record()` — remove `name: record.metadata.name,` (line 205)

In `crates/cleat/src/session.rs`:
- `build_inspect_result()` — remove `name: session.name.clone(),` from `SessionInspect` (line 692)

In `crates/cleat/src/cli.rs`:
- `format_session_human()` — no change needed (doesn't use `name`)

- [ ] **Step 5: Update `ensure_session_started` call**

In `crates/cleat/src/session.rs`, `ensure_session_started` passes `name` to `layout.create_session()`. Rename the parameter from `name` to `id`:

```rust
pub fn ensure_session_started(
    layout: &RuntimeLayout,
    id: Option<String>,
    vt_engine: Option<VtEngineKind>,
    cwd: Option<PathBuf>,
    cmd: Option<String>,
) -> Result<SessionMetadata, String> {
    let session = if let Some(existing) = id.as_deref().and_then(|value| load_session(layout.root(), value).ok().flatten()) {
        existing
    } else {
        let vt_engine = vt_engine.unwrap_or_else(vt::default_vt_engine_kind);
        vt_engine.ensure_available()?;
        layout.create_session(id, vt_engine, cwd, cmd)?.metadata
    };

    let socket_path = session_socket_path(layout.root(), &session.id);
    if !socket_path.exists() {
        spawn_daemon_process(layout.root(), &session)?;
        wait_for_socket(&socket_path)?;
    }

    Ok(session)
}
```

In `crates/cleat/src/server.rs`, `create()` and `attach()` pass `name` to `ensure_session_started`. These stay the same (the parameter is just renamed in the callee).

- [ ] **Step 6: Update CLI tests**

In `crates/cleat/tests/cli.rs`, remove `name: ...` from all `Command::Create` and `Command::Attach` struct literal assertions. These tests will fail to compile until the field is removed. There are no `name`-specific assertions — the field was always `None` or identical to `id`.

In `crates/cleat/tests/lifecycle.rs`, update `create_json_returns_structured_metadata` — it deserializes `SessionInfo` from JSON, which no longer has `name`.

- [ ] **Step 7: Run tests**

Run: `cargo test -p cleat --locked`
Expected: all compile and pass.

- [ ] **Step 8: Run clippy and fmt**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings && cargo +nightly-2026-03-12 fmt --check`

- [ ] **Step 9: Commit**

```bash
git add crates/cleat/
git commit -m "refactor(cleat): remove name field, collapse into id"
```

---

### Task 2: Add config flags to `Serve` command and wire through to daemon

**Files:**
- Modify: `crates/cleat/src/cli.rs`
- Modify: `crates/cleat/src/server.rs`
- Modify: `crates/cleat/src/session.rs`
- Modify: `crates/cleat/tests/cli.rs`

- [ ] **Step 1: Add flags to `Serve` command**

In `crates/cleat/src/cli.rs`, update the `Serve` variant:

```rust
#[command(hide = true)]
Serve {
    #[arg(long)]
    id: String,
    #[arg(long, value_enum, default_value_t = crate::vt::default_vt_engine_kind())]
    vt: VtEngineKind,
    #[arg(long)]
    cmd: Option<String>,
    #[arg(long)]
    cwd: Option<PathBuf>,
    #[arg(long)]
    record: bool,
},
```

- [ ] **Step 2: Update `execute` for `Command::Serve`**

```rust
Command::Serve { id, vt, cmd, cwd, record } => {
    let session = crate::runtime::SessionMetadata { id, vt_engine: vt, cwd, cmd, record };
    service.serve(&session)?;
    Ok(None)
}
```

- [ ] **Step 3: Update `SessionService::serve()`**

In `crates/cleat/src/server.rs`, change signature:

```rust
pub fn serve(&self, session: &crate::runtime::SessionMetadata) -> Result<(), String> {
    run_session_daemon(self.layout.root(), session)
}
```

- [ ] **Step 4: Update `run_session_daemon` signature**

In `crates/cleat/src/session.rs`, change from `(root: &Path, id: &str)` to `(root: &Path, session: &SessionMetadata)`:

```rust
pub fn run_session_daemon(root: &Path, session: &SessionMetadata) -> Result<(), String> {
    let id = &session.id;
    let socket_path = session_socket_path(root, id);
    if socket_path.exists() {
        let _ = fs::remove_file(&socket_path);
    }

    let listener =
        std::os::unix::net::UnixListener::bind(&socket_path).map_err(|err| format!("bind socket {}: {err}", socket_path.display()))?;
    listener.set_nonblocking(true).map_err(|err| format!("set listener nonblocking: {err}"))?;
    fs::write(daemon_pid_path(root, id), std::process::id().to_string()).map_err(|err| format!("write daemon pid: {err}"))?;

    let pty_child = spawn_pty_child(session)?;
    let pty_fd = pty_child.master_fd;
    set_nonblocking(pty_fd)?;
    let mut vt_engine = default_vt_engine(session.vt_engine)?;
    let mut detached_da = DeviceAttributeTracker::new();

    let mut active_client: Option<ActiveClient> = None;
    let mut recorder: Option<crate::recording::OutputRecorder> = None;
    if session.record || std::env::var("CLEAT_RECORD").map(|v| v == "1").unwrap_or(false) {
        match crate::recording::OutputRecorder::new(&root.join(id)) {
            Ok(r) => recorder = Some(r),
            Err(err) => eprintln!("failed to start recording: {err}"),
        }
    }
    // ... rest of the function body is unchanged from here
```

Note: the existing function does NOT have a `#[cfg(unix)]` attribute — only the non-unix stub does. Keep it that way.

Remove the `load_session` call that was at the top. Use `session` directly for `session.vt_engine`, `session.record`, etc.

Also update the `#[cfg(not(unix))]` stub:

```rust
#[cfg(not(unix))]
pub fn run_session_daemon(_root: &Path, _session: &SessionMetadata) -> Result<(), String> {
    Err("session daemon is only supported on unix".into())
}
```

- [ ] **Step 5: Update `spawn_daemon_process` to pass args**

In `crates/cleat/src/session.rs`:

```rust
fn spawn_daemon_process(root: &Path, session: &SessionMetadata) -> Result<(), String> {
    let exe = resolve_cleat_executable()?;
    let mut command = Command::new(exe);
    command
        .arg("--runtime-root")
        .arg(root)
        .arg("serve")
        .arg("--id")
        .arg(&session.id)
        .arg("--vt")
        .arg(session.vt_engine.as_str());
    if let Some(cmd) = &session.cmd {
        command.arg("--cmd").arg(cmd);
    }
    if let Some(cwd) = &session.cwd {
        command.arg("--cwd").arg(cwd);
    }
    if session.record {
        command.arg("--record");
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = command.spawn().map_err(|err| format!("spawn session daemon for {}: {err}", session.id))?;
    fs::write(daemon_pid_path(root, &session.id), child.id().to_string()).map_err(|err| format!("write daemon pid: {err}"))?;
    Ok(())
}
```

- [ ] **Step 6: Update `#[cfg(not(unix))]` stub**

```rust
#[cfg(not(unix))]
pub fn run_session_daemon(_root: &Path, _session: &SessionMetadata) -> Result<(), String> {
    Err("session daemon is only supported on unix".into())
}
```

- [ ] **Step 7: Update CLI parsing tests for `Serve`**

The existing test `send_keys_execute_reports_missing_session` uses `Command::SendKeys` and is unaffected. But update the `help_lists_expected_subcommands` test if the Serve position shifted (it's hidden, so it won't appear in the list — no change needed).

Add a test for the new Serve flags if they aren't covered. Since `Serve` is hidden, we just need to verify it parses:

```rust
#[test]
fn serve_parses_all_flags() {
    let cli = Cli::try_parse_from(["cleat", "serve", "--id", "alpha", "--vt", "passthrough", "--cmd", "bash", "--cwd", "/tmp", "--record"]).expect("parse serve");
    assert!(matches!(cli.command, Command::Serve { ref id, record: true, .. } if id == "alpha"));
}
```

- [ ] **Step 8: Run tests**

Run: `cargo test -p cleat --locked`

- [ ] **Step 9: Run clippy and fmt**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings && cargo +nightly-2026-03-12 fmt --check`

- [ ] **Step 10: Commit**

```bash
git add crates/cleat/
git commit -m "refactor(cleat): pass session config as serve CLI args instead of reading meta.json"
```

---

### Task 3: Replace meta.json existence checks with directory checks

**Files:**
- Modify: `crates/cleat/src/server.rs`

- [ ] **Step 1: Replace all `meta.json` existence guards**

In `crates/cleat/src/server.rs`, replace every instance of:
```rust
if !self.layout.root().join(id).join("meta.json").exists() {
    return Err(format!("missing session {id}"));
}
```

with:
```rust
if !self.layout.root().join(id).exists() {
    return Err(format!("missing session {id}"));
}
```

This applies to: `kill`, `detach`, `capture`, `send_keys`, `inspect`, `signal`, `record`.

For `record()`, also remove the meta.json read-modify-write block (lines 176-182) — recording is now runtime-only:

```rust
pub fn record(&self, id: &str, enable: bool) -> Result<(), String> {
    if !self.layout.root().join(id).exists() {
        return Err(format!("missing session {id}"));
    }
    let socket_path = session_socket_path(self.layout.root(), id);
    let mut stream =
        std::os::unix::net::UnixStream::connect(&socket_path).map_err(|err| format!("connect {}: {err}", socket_path.display()))?;
    Frame::RecordControl { enable }.write(&mut stream).map_err(|err| format!("write record control: {err}"))?;
    match Frame::read(&mut stream).map_err(|err| format!("read record response: {err}"))? {
        Frame::Ack => Ok(()),
        Frame::Error(message) => Err(message),
        other => Err(format!("unexpected record response: {other:?}")),
    }
}
```

- [ ] **Step 2: Update `ensure_session_started` to use directory existence**

In `crates/cleat/src/session.rs`, replace the `load_session` call with a directory check:

```rust
pub fn ensure_session_started(
    layout: &RuntimeLayout,
    id: Option<String>,
    vt_engine: Option<VtEngineKind>,
    cwd: Option<PathBuf>,
    cmd: Option<String>,
) -> Result<SessionMetadata, String> {
    let session = if let Some(ref id_str) = id {
        if layout.root().join(id_str).exists() {
            // Session directory exists — reuse it. Build minimal metadata for daemon spawn.
            let vt_engine = vt_engine.unwrap_or_else(vt::default_vt_engine_kind);
            SessionMetadata { id: id_str.clone(), vt_engine, cwd, cmd, record: false }
        } else {
            let vt_engine = vt_engine.unwrap_or_else(vt::default_vt_engine_kind);
            vt_engine.ensure_available()?;
            layout.create_session(id, vt_engine, cwd, cmd)?.metadata
        }
    } else {
        let vt_engine = vt_engine.unwrap_or_else(vt::default_vt_engine_kind);
        vt_engine.ensure_available()?;
        layout.create_session(id, vt_engine, cwd, cmd)?.metadata
    };

    let socket_path = session_socket_path(layout.root(), &session.id);
    if !socket_path.exists() {
        spawn_daemon_process(layout.root(), &session)?;
        wait_for_socket(&socket_path)?;
    }

    Ok(session)
}
```

- [ ] **Step 3: Update `attach` no-create path**

In `crates/cleat/src/server.rs`, the `no_create` path currently calls `list_sessions()`. Replace with directory existence:

```rust
let session = if no_create {
    let id = name.ok_or_else(|| "attach --no-create requires a session id".to_string())?;
    if !self.layout.root().join(&id).exists() {
        return Err(format!("missing session {id}"));
    }
    let vt_engine = vt_engine.unwrap_or_else(crate::vt::default_vt_engine_kind);
    crate::runtime::SessionMetadata { id, vt_engine, cwd, cmd, record: false }
} else {
    ensure_session_started(&self.layout, name, vt_engine, cwd, cmd)?
};
```

- [ ] **Step 4: Update unit tests in `server.rs`**

Update `kill_does_not_signal_unrelated_process_from_stale_pid_file` and `send_keys_writes_frame_to_session_socket` — remove the `meta.json` write, just create the directory:

```rust
let session_dir = temp.path().join("alpha");
fs::create_dir_all(&session_dir).expect("create session dir");
// No meta.json write needed — directory existence is sufficient
```

- [ ] **Step 5: Delete `load_session`**

In `crates/cleat/src/session.rs`, delete the `load_session` function (lines 907-914). No callers remain.

- [ ] **Step 6: Run tests**

Run: `cargo test -p cleat --locked`

- [ ] **Step 7: Run clippy and fmt**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings && cargo +nightly-2026-03-12 fmt --check`

- [ ] **Step 8: Commit**

```bash
git add crates/cleat/
git commit -m "refactor(cleat): replace meta.json existence checks with directory checks, delete load_session"
```

---

### Task 4: Rewrite `list` to use `Inspect` frames, remove `write_metadata`/`list_sessions`/`SessionRecord`

**Files:**
- Modify: `crates/cleat/src/server.rs`
- Modify: `crates/cleat/src/runtime.rs`
- Modify: `crates/cleat/tests/lifecycle.rs`

- [ ] **Step 1: Rewrite `SessionService::list()`**

Replace the current implementation that calls `layout.list_sessions()` with socket-based inspection:

```rust
pub fn list(&self) -> Result<Vec<SessionInfo>, String> {
    if !self.layout.root().exists() {
        return Ok(vec![]);
    }

    let mut sessions = Vec::new();
    let entries = std::fs::read_dir(self.layout.root())
        .map_err(|err| format!("read runtime root {}: {err}", self.layout.root().display()))?;

    for entry in entries {
        let entry = entry.map_err(|err| format!("read runtime entry: {err}"))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let id = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name.to_string(),
            None => continue,
        };
        let socket_path = session_socket_path(self.layout.root(), &id);
        if !socket_path.exists() {
            continue;
        }
        if let Ok(result) = self.inspect(&id) {
            let status = if result.attachments.is_empty() {
                SessionStatus::Detached
            } else {
                SessionStatus::Attached
            };
            sessions.push(SessionInfo {
                id: result.session.id,
                vt_engine: parse_vt_engine_kind(&result.session.vt_engine),
                cwd: result.session.cwd,
                cmd: result.session.cmd,
                status,
            });
        }
    }
    sessions.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(sessions)
}
```

Note: `VtEngineKind` is stored as a string in `SessionInspect.vt_engine` (e.g., `"passthrough"`, `"ghostty"`). It has `Deserialize` with `#[serde(rename_all = "lowercase")]` but no `FromStr`. Use a match on the string values:

```rust
fn parse_vt_engine_kind(s: &str) -> VtEngineKind {
    match s {
        "ghostty" => crate::vt::VtEngineKind::Ghostty,
        _ => crate::vt::VtEngineKind::Passthrough,
    }
}
```

Add this helper in `server.rs` and use it in `list()` instead of `.parse()`.

- [ ] **Step 2: Remove `session_info_from_record`**

Delete the `session_info_from_record` function from `server.rs` — no longer needed.

- [ ] **Step 3: Remove `SessionRecord`, `write_metadata`, `list_sessions` from `runtime.rs`**

In `crates/cleat/src/runtime.rs`:
- Delete `SessionRecord` struct
- Delete `write_metadata` method
- Delete `list_sessions` method
- Remove `serde::{Deserialize, Serialize}` import if `SessionMetadata` no longer derives them (but it still needs `Serialize`/`Deserialize` — keep the import, but remove `Serialize`/`Deserialize` derives from `SessionMetadata` since it's no longer written to disk. Actually, `SessionMetadata` might still be useful as a plain struct. Remove the serde derives if nothing serializes it anymore.)

Update `create_session` to stop calling `write_metadata`:

```rust
pub fn create_session(
    &self,
    id: Option<String>,
    vt_engine: VtEngineKind,
    cwd: Option<PathBuf>,
    cmd: Option<String>,
) -> Result<SessionMetadata, String> {
    self.ensure_root()?;
    let id = id.unwrap_or_else(|| format!("session-{}", Uuid::new_v4()));
    let dir = self.root.join(&id);
    fs::create_dir_all(&dir).map_err(|err| format!("create session dir {}: {err}", dir.display()))?;
    Ok(SessionMetadata { id, vt_engine, cwd, cmd, record: false })
}
```

Note: `create_session` now returns `SessionMetadata` directly instead of `SessionRecord`.

- [ ] **Step 4: Update callers of `create_session`**

In `crates/cleat/src/session.rs`, `ensure_session_started` calls `layout.create_session(...)?.metadata` — change to just `layout.create_session(...)?` since it now returns `SessionMetadata` directly.

- [ ] **Step 5: Remove `SessionRecord` import from `server.rs`**

Remove `SessionRecord` from the import in `server.rs` line 5.

- [ ] **Step 6: Simplify `create --record` in `cli.rs`**

The `create --record` path currently has a retry loop to send `RecordControl` to the daemon. Since `spawn_daemon_process` now passes `--record` as an arg, the daemon enables recording on startup. Remove the retry loop:

```rust
Command::Create { id, json, vt, cwd, cmd, record } => {
    let created = service.create(id, vt, cwd, cmd, record)?;
    if json {
        serde_json::to_string(&created).map(Some).map_err(|err| format!("serialize create result: {err}"))
    } else {
        Ok(Some(created.id))
    }
}
```

This requires threading `record` through `service.create()` → `ensure_session_started()` → `SessionMetadata`. Update `service.create()` to accept `record: bool`:

```rust
pub fn create(
    &self,
    id: Option<String>,
    vt_engine: Option<VtEngineKind>,
    cwd: Option<std::path::PathBuf>,
    cmd: Option<String>,
    record: bool,
) -> Result<SessionInfo, String> {
```

And `ensure_session_started` to accept and pass `record`:

```rust
pub fn ensure_session_started(
    layout: &RuntimeLayout,
    id: Option<String>,
    vt_engine: Option<VtEngineKind>,
    cwd: Option<PathBuf>,
    cmd: Option<String>,
    record: bool,
) -> Result<SessionMetadata, String> {
```

**Important:** `server.rs::attach()` also calls `ensure_session_started` in its `else` branch. Update that call to pass `false` for `record` (attach handles recording separately via `service.record()` in `cli.rs`):

```rust
ensure_session_started(&self.layout, name, vt_engine, cwd, cmd, false)?
```

Set `record` on the `SessionMetadata` before passing to `spawn_daemon_process`.

- [ ] **Step 7: Update lifecycle tests**

Tests that previously used `service.create(Some("alpha".into()), None, None, None)` now need a `record` parameter: `service.create(Some("alpha".into()), None, None, None, false)`.

The `create_with_record_flag_activates_recording` test simplifies — remove the retry loop expectation. Recording should be active immediately because the daemon started with `--record`.

The `attach_no_create_rejects_missing_session` test is unaffected (it already tests the error path). The `attach --no-create` success-path tests (e.g., `attach_creates_session_lazily_and_reuses_it_on_later_attach`) don't use `--no-create` so they're also unaffected.

- [ ] **Step 8: Run tests**

Run: `cargo test -p cleat --locked`

- [ ] **Step 9: Run clippy and fmt**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings && cargo +nightly-2026-03-12 fmt --check`

- [ ] **Step 10: Commit**

```bash
git add crates/cleat/
git commit -m "refactor(cleat): rewrite list to use Inspect frames, remove meta.json infrastructure"
```

---

### Task 5: Final cleanup and verification

**Files:**
- All modified files

- [ ] **Step 1: Verify no meta.json references remain**

Run: `grep -r "meta.json" crates/cleat/src/ crates/cleat/tests/`
Expected: no matches.

- [ ] **Step 2: Verify no `load_session` references remain**

Run: `grep -r "load_session" crates/cleat/src/`
Expected: no matches.

- [ ] **Step 3: Verify no `SessionRecord` references remain**

Run: `grep -r "SessionRecord" crates/cleat/`
Expected: no matches.

- [ ] **Step 4: Verify no `write_metadata` references remain**

Run: `grep -r "write_metadata" crates/cleat/`
Expected: no matches.

- [ ] **Step 5: Run full CI gate**

```bash
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

- [ ] **Step 6: Fix any issues**

- [ ] **Step 7: Commit any cleanup**

```bash
git add -A
git commit -m "chore(cleat): final cleanup for meta.json removal"
```
