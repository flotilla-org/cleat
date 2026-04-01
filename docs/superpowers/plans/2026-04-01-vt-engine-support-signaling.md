# VT Engine Support Signaling Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make cleat loudly identify Ghostty as the only functional VT engine today, mark passthrough as placeholder/test-only, and brand no-`ghostty-vt` binaries as non-functional for real usage.

**Architecture:** Add a single source of truth for VT support status that is available at build time and runtime. Use that status to drive Cargo warnings, CLI help/error text, and structured session metadata so both humans and agents can detect whether a build is functional. Keep the internal passthrough seam, but demote it everywhere user-facing.

**Tech Stack:** Rust, Cargo build script, clap, serde JSON, existing `cleat` protocol/session/server layers.

---

## File Structure

- Modify: `crates/cleat/build.rs`
  - Emit loud Cargo warnings for no-`ghostty-vt` builds and export compile-time status env vars.
- Create: `crates/cleat/src/vt/support.rs`
  - Centralize user-facing VT support policy strings and compile-time status helpers.
- Modify: `crates/cleat/src/vt/mod.rs`
  - Re-export support helpers and document functional vs placeholder engine policy next to `VtEngineKind`.
- Modify: `crates/cleat/src/cli.rs`
  - Update top-level/about/help text, `--vt` help text, human-readable list/inspect output, and command error wording.
- Modify: `crates/cleat/src/protocol.rs`
  - Extend inspect/list-facing structured types with explicit VT support status fields for agents.
- Modify: `crates/cleat/src/server.rs`
  - Populate new structured support-status fields from daemon/session state.
- Modify: `README.md`
  - Move the “Ghostty is the only functional VT engine” message to the top-level status/development docs.
- Modify: `crates/cleat/tests/cli.rs`
  - Lock new help text and command-surface wording.
- Modify: `crates/cleat/tests/lifecycle.rs`
  - Lock non-functional-build behavior, passthrough wording, and structured session output expectations.
- Optionally modify: `crates/cleat/tests/runtime.rs`, `crates/cleat/tests/vt.rs`, `crates/cleat/tests/vt_contracts.rs`
  - Rename assertions/messages that currently normalize passthrough as a first-class real-use engine.

## Chunk 1: Build-time status and shared policy helpers

### Task 1: Add failing tests for runtime-visible support policy helpers

**Files:**
- Create or Modify: `crates/cleat/tests/cli.rs`
- Create or Modify: `crates/cleat/tests/lifecycle.rs`

- [ ] **Step 1: Add a small CLI-facing test for support wording**

Add assertions that command/help text or formatting helpers include phrases equivalent to:

```rust
assert!(help.contains("Ghostty is currently the only functional VT engine"));
assert!(help.contains("builds without ghostty-vt are non-functional for real use"));
```

- [ ] **Step 2: Run the targeted test to verify it fails**

Run: `cargo test -p cleat --locked cli:: -- --nocapture`

Expected: FAIL because the current CLI/help text is still neutral.

- [ ] **Step 3: Add a lifecycle/structured-output test for explicit support status**

Add an assertion pattern like:

```rust
assert_eq!(inspect.session.vt_engine_status, "functional");
assert!(inspect.session.functional_vt_available);
```

or, for a no-`ghostty-vt` build:

```rust
assert_eq!(inspect.session.vt_engine_status, "placeholder");
assert!(!inspect.session.functional_vt_available);
```

- [ ] **Step 4: Run the targeted lifecycle test to verify it fails**

Run: `cargo test -p cleat --locked lifecycle:: -- --nocapture`

Expected: FAIL because the structured fields do not exist yet.

- [ ] **Step 5: Commit**

```bash
git add crates/cleat/tests/cli.rs crates/cleat/tests/lifecycle.rs
git commit -m "test: lock vt support messaging expectations"
```

### Task 2: Export build-time VT support status and warnings

**Files:**
- Modify: `crates/cleat/build.rs`
- Create: `crates/cleat/src/vt/support.rs`
- Modify: `crates/cleat/src/vt/mod.rs`

- [ ] **Step 1: Write the failing unit test or compile-time consumer for a support module**

Add a small runtime consumer test such as:

```rust
assert!(!cleat::vt::build_support_message().is_empty());
```

and a status predicate:

```rust
#[cfg(feature = "ghostty-vt")]
assert!(cleat::vt::functional_vt_available());
#[cfg(not(feature = "ghostty-vt"))]
assert!(!cleat::vt::functional_vt_available());
```

- [ ] **Step 2: Run the targeted test to verify it fails**

Run: `cargo test -p cleat --locked vt:: -- --nocapture`

Expected: FAIL because the helper API does not exist yet.

- [ ] **Step 3: Implement `crates/cleat/src/vt/support.rs` with the shared policy surface**

Add a focused helper module with functions/constants like:

```rust
pub const FUNCTIONAL_ENGINE_NAME: &str = "ghostty";
pub const PLACEHOLDER_ENGINE_STATUS: &str = "placeholder";
pub const FUNCTIONAL_ENGINE_STATUS: &str = "functional";

pub const fn functional_vt_available() -> bool {
    option_env!("CLEAT_FUNCTIONAL_VT_AVAILABLE") == Some("1")
}

pub fn build_support_message() -> &'static str {
    if functional_vt_available() {
        "Ghostty is currently the only functional VT engine."
    } else {
        "This cleat binary was built without ghostty-vt and is non-functional for real use. Ghostty is currently the only functional VT engine; passthrough is placeholder/test-only."
    }
}

pub fn vt_engine_status(engine: super::VtEngineKind) -> &'static str {
    match engine {
        super::VtEngineKind::Ghostty => FUNCTIONAL_ENGINE_STATUS,
        super::VtEngineKind::Passthrough => PLACEHOLDER_ENGINE_STATUS,
    }
}
```

Re-export the helpers from `crates/cleat/src/vt/mod.rs`.

- [ ] **Step 4: Update `crates/cleat/build.rs` to export env vars and warnings**

Implement these behaviors:

```rust
fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    if env::var_os("CARGO_FEATURE_GHOSTTY_VT").is_none() {
        println!("cargo:rustc-env=CLEAT_FUNCTIONAL_VT_AVAILABLE=0");
        println!("cargo:warning=building cleat without ghostty-vt; this binary is non-functional for real terminal usage");
        println!("cargo:warning=Ghostty is currently the only functional VT engine");
        println!("cargo:warning=passthrough is a placeholder/testing engine only");
        println!("cargo:warning=run ./tools/prepare-ghostty-vt.sh and rebuild with --features ghostty-vt for a functional binary");
        return;
    }

    println!("cargo:rustc-env=CLEAT_FUNCTIONAL_VT_AVAILABLE=1");
    // existing ghostty install validation/linking stays in place
}
```

Keep all existing Ghostty prefix validation/linker behavior intact for feature-on builds.

- [ ] **Step 5: Run targeted tests to verify they pass**

Run: `cargo test -p cleat --locked vt:: -- --nocapture`

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/cleat/build.rs crates/cleat/src/vt/mod.rs crates/cleat/src/vt/support.rs
git commit -m "build: export vt support status and warnings"
```

## Chunk 2: CLI/help/error text and structured agent-facing metadata

### Task 3: Update CLI help and user-visible wording

**Files:**
- Modify: `crates/cleat/src/cli.rs`
- Modify: `crates/cleat/tests/cli.rs`

- [ ] **Step 1: Write the failing CLI help tests**

Add targeted tests that render clap help and assert the new policy wording exists, for example:

```rust
let mut command = Cli::command();
let mut buffer = Vec::new();
command.write_long_help(&mut buffer).expect("write help");
let help = String::from_utf8(buffer).expect("utf8");
assert!(help.contains("Ghostty is currently the only functional VT engine"));
assert!(help.contains("non-functional for real use"));
```

Also add assertions that the `--vt` help text contains wording like:

```rust
assert!(help.contains("placeholder engines are for testing/development only"));
```

- [ ] **Step 2: Run the targeted test to verify it fails**

Run: `cargo test -p cleat --locked cli::help -- --nocapture`

Expected: FAIL because current help text is neutral.

- [ ] **Step 3: Update clap metadata and formatting strings in `crates/cleat/src/cli.rs`**

Make these changes:

- top-level `about`/`long_about` includes the build support message from `crate::vt::build_support_message()` where clap allows a static string; if clap requires string literals, add `after_help` or a runtime `command()` post-processing hook to append the message
- change `--vt` help from `Virtual terminal engine` to explicit policy wording
- append support status to human-readable list/inspect formatting, e.g. `passthrough (placeholder)`
- add a helper for no-`ghostty-vt` error prefix text such as:

```rust
fn nonfunctional_build_error() -> String {
    format!(
        "this cleat binary was built without ghostty-vt and is non-functional for real terminal usage; {}",
        crate::vt::build_support_message()
    )
}
```

Use that helper in `Launch`, `Attach`, and `Capture` error paths where appropriate.

- [ ] **Step 4: Run CLI tests to verify they pass**

Run: `cargo test -p cleat --locked cli:: -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/cleat/src/cli.rs crates/cleat/tests/cli.rs
git commit -m "cli: surface vt support policy in help and text output"
```

### Task 4: Add structured VT support fields for agents

**Files:**
- Modify: `crates/cleat/src/protocol.rs`
- Modify: `crates/cleat/src/server.rs`
- Modify: `crates/cleat/tests/lifecycle.rs`

- [ ] **Step 1: Add failing tests for inspect/list JSON support fields**

Add assertions such as:

```rust
assert_eq!(inspect.session.vt_engine, "passthrough");
assert_eq!(inspect.session.vt_engine_status, "placeholder");
assert!(!inspect.session.functional_vt_available);
```

and for normal ghostty sessions:

```rust
assert_eq!(inspect.session.vt_engine_status, "functional");
assert!(inspect.session.functional_vt_available);
```

- [ ] **Step 2: Run the targeted test to verify it fails**

Run: `cargo test -p cleat --locked lifecycle::inspect -- --nocapture`

Expected: FAIL because the JSON schema has no such fields.

- [ ] **Step 3: Extend the protocol structs**

In `crates/cleat/src/protocol.rs`, add fields to `SessionInspect` and `SessionInfo` if needed:

```rust
pub struct SessionInspect {
    pub id: String,
    pub state: String,
    pub vt_engine: String,
    pub vt_engine_status: String,
    pub functional_vt_available: bool,
    pub cwd: Option<PathBuf>,
    pub cmd: Option<String>,
}
```

If `SessionInfo` also needs agent-readable support status for `list --json`, add analogous fields there rather than forcing agents to inspect every session.

- [ ] **Step 4: Populate the new fields in `crates/cleat/src/server.rs`**

When building `SessionInfo` / `InspectResult`, compute support status through the shared helper module:

```rust
vt_engine_status: crate::vt::vt_engine_status(session.vt_engine).to_string(),
functional_vt_available: crate::vt::functional_vt_available(),
```

Ensure list/inspect conversion helpers preserve those values when converting from daemon inspect results.

- [ ] **Step 5: Run lifecycle tests to verify they pass**

Run: `cargo test -p cleat --locked lifecycle:: -- --nocapture`

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/cleat/src/protocol.rs crates/cleat/src/server.rs crates/cleat/tests/lifecycle.rs
git commit -m "protocol: expose vt support status for agents"
```

## Chunk 3: Non-functional-build guardrails and docs

### Task 5: Add no-Ghostty runtime guardrails

**Files:**
- Modify: `crates/cleat/src/cli.rs`
- Modify: `crates/cleat/src/server.rs`
- Modify: `crates/cleat/tests/lifecycle.rs`

- [ ] **Step 1: Write failing tests for no-Ghostty behavior**

Add `#[cfg(not(feature = "ghostty-vt"))]` tests that assert real-use commands fail loudly, for example:

```rust
let err = cli::execute(cli, &service).expect_err("non-functional build should reject launch");
assert!(err.contains("non-functional for real terminal usage"));
assert!(err.contains("ghostty-vt"));
```

Focus on at least:
- `create`/`launch` with no explicit VT
- `attach` lazy-create path with no explicit VT
- `capture` on passthrough session wording

- [ ] **Step 2: Run the targeted tests to verify they fail**

Run: `cargo test -p cleat --locked lifecycle::create_ -- --nocapture`

Expected: FAIL because current no-feature builds still normalize passthrough.

- [ ] **Step 3: Implement guardrails with minimal blast radius**

Recommended behavior:

- in no-`ghostty-vt` builds, creating or lazily attaching a new session without an explicit internal override returns an error explaining the build is non-functional
- if passthrough sessions already exist for tests/internal seams, `capture` and related errors should explicitly call them placeholder/test-only

One possible seam is a helper in `server.rs`:

```rust
fn reject_nonfunctional_build_for_real_use() -> Result<(), String> {
    if crate::vt::functional_vt_available() {
        Ok(())
    } else {
        Err("this cleat binary was built without ghostty-vt and is non-functional for real terminal usage; run ./tools/prepare-ghostty-vt.sh and rebuild with --features ghostty-vt".to_string())
    }
}
```

Use it before creating new user-facing sessions in no-feature builds while avoiding unnecessary disruption to internal test seams that construct passthrough directly.

- [ ] **Step 4: Run targeted lifecycle tests to verify they pass**

Run: `cargo test -p cleat --locked lifecycle:: -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/cleat/src/cli.rs crates/cleat/src/server.rs crates/cleat/tests/lifecycle.rs
git commit -m "server: reject real-use flows in non-functional builds"
```

### Task 6: Update README and any wording-focused tests

**Files:**
- Modify: `README.md`
- Optionally Modify: `crates/cleat/tests/vt.rs`
- Optionally Modify: `crates/cleat/tests/vt_contracts.rs`

- [ ] **Step 1: Add a failing doc assertion if there is a docs test seam; otherwise skip directly to implementation**

If no doc test seam exists, note that README updates are verified by review plus full test suite.

- [ ] **Step 2: Rewrite the README status/development sections**

Make the opening sections say, plainly:

- Ghostty is currently the only functional VT engine
- builds without `ghostty-vt` are non-functional placeholder builds for real usage
- passthrough is placeholder/test-only
- how to build a functional binary using `./tools/prepare-ghostty-vt.sh` and `--features ghostty-vt`

Do not leave “Ghostty stays out of the default build” as the dominant framing without the stronger warning.

- [ ] **Step 3: Update any test names/messages that normalize passthrough as first-class**

Examples:
- `capture_rejects_passthrough_sessions` should assert placeholder wording, not generic unsupported wording
- contract test descriptions should mention placeholder/test-only semantics where appropriate

- [ ] **Step 4: Run the relevant test subset**

Run: `cargo test -p cleat --locked vt -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add README.md crates/cleat/tests/vt.rs crates/cleat/tests/vt_contracts.rs crates/cleat/tests/lifecycle.rs
git commit -m "docs: document ghostty as the only functional vt engine"
```

## Final verification

- [ ] **Step 1: Run formatting**

Run: `cargo +nightly-2026-03-12 fmt --check`

Expected: PASS.

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

Expected: PASS.

- [ ] **Step 3: Run workspace tests**

Run: `cargo test --workspace --locked`

Expected: PASS.

- [ ] **Step 4: If VT support messaging touched feature-on paths, run the Ghostty feature verification too**

Run:

```bash
./tools/prepare-ghostty-vt.sh
find .tools/ghostty-install -maxdepth 3 | sort
LD_LIBRARY_PATH="$PWD/.tools/ghostty-install/lib" cargo build -p cleat --locked --features ghostty-vt
LD_LIBRARY_PATH="$PWD/.tools/ghostty-install/lib" cargo test -p cleat --locked --features ghostty-vt
```

Expected: PASS on Linux. On macOS, use `DYLD_LIBRARY_PATH` instead.

- [ ] **Step 5: Commit final verification-friendly state**

```bash
git add crates/cleat/build.rs crates/cleat/src/vt/support.rs crates/cleat/src/vt/mod.rs crates/cleat/src/cli.rs crates/cleat/src/protocol.rs crates/cleat/src/server.rs README.md crates/cleat/tests/cli.rs crates/cleat/tests/lifecycle.rs crates/cleat/tests/vt.rs crates/cleat/tests/vt_contracts.rs docs/superpowers/specs/2026-04-01-vt-engine-support-signaling-design.md docs/superpowers/plans/2026-04-01-vt-engine-support-signaling.md
git commit -m "vt: make ghostty-only support explicit"
```
