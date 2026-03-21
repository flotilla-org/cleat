# Ghostty Build Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the standalone `cleat` repo’s optional `ghostty-vt` feature reproducible for both local development and CI using a pinned Ghostty fork/ref and pinned Zig toolchain, preferring static `libghostty-vt.a` linking to avoid runtime library-path problems.

**Architecture:** `cleat` keeps `ghostty-vt` feature-gated and Rust-only by default. A checked-in helper prepares a repo-local Ghostty source checkout and install prefix from a pinned `rjwittams/ghostty` ref. `build.rs` validates and prefers static `libghostty-vt.a` from that prefix, falling back to the shared library only if static output is unavailable. CI adds a dedicated feature-on job that uses the same helper flow.

**Tech Stack:** Rust/Cargo workspace, Zig 0.15.2, GitHub Actions, local shell helper scripts, Ghostty fork (`rjwittams/ghostty`).

---

### Task 1: Add pinned Ghostty/Zig build metadata

**Files:**
- Create: `/home/robert/dev/cleat/tools/ghostty-toolchain.toml`
- Modify: `/home/robert/dev/cleat/README.md`
- Modify: `/home/robert/dev/cleat/AGENTS.md`

- [ ] **Step 1: Write the failing metadata-consumer test (or equivalent parser unit test if you add one)**

If you introduce a Rust parser/helper for the metadata file, add a focused unit test under `crates/cleat/tests/` or `crates/cleat/src/` proving it reads the expected Ghostty repo/ref and Zig version.

- [ ] **Step 2: Create the metadata file**

Write `tools/ghostty-toolchain.toml` with exact pinned values:
- `zig.version = "0.15.2"`
- `ghostty.repo = "https://github.com/rjwittams/ghostty.git"`
- `ghostty.ref = "vt-static-lib"` (or exact commit SHA if preferred once the experiment is pushed)
- `ghostty.build_step = "lib-vt"`

Keep the shape minimal and only include fields the helper/build flow will actually consume.

- [ ] **Step 3: Document the metadata contract**

Update `README.md` and `AGENTS.md` so they explain that:
- `ghostty-vt` is optional
- the helper uses a pinned Ghostty fork/ref plus pinned Zig version
- default CI/builds remain Ghostty-free

- [ ] **Step 4: Verify the docs/metadata change**

Run: `cargo test --workspace --locked`
Expected: PASS (metadata/docs alone should not disturb the current default build)

- [ ] **Step 5: Commit**

```bash
git add tools/ghostty-toolchain.toml README.md AGENTS.md
git commit -m "docs: pin ghostty toolchain metadata"
```

### Task 2: Add helper script to fetch/build/install Ghostty VT locally

**Files:**
- Create: `/home/robert/dev/cleat/tools/prepare-ghostty-vt.sh`
- Modify: `/home/robert/dev/cleat/README.md`
- Modify: `/home/robert/dev/cleat/AGENTS.md`

- [ ] **Step 1: Write a shell-level smoke check script or documented manual verification target**

Define the exact expected outputs before implementation:
- source checkout at `.tools/ghostty-src`
- install prefix at `.tools/ghostty-install`
- resulting files include:
  - `.tools/ghostty-install/include/ghostty/vt.h`
  - `.tools/ghostty-install/lib/libghostty-vt.a`
  - `.tools/ghostty-install/lib/libghostty-vt.so`

- [ ] **Step 2: Implement the helper script**

`tools/prepare-ghostty-vt.sh` should:
- read pinned values from `tools/ghostty-toolchain.toml`
- verify `zig version` is exactly `0.15.2`
- clone/fetch the Ghostty fork into `.tools/ghostty-src`
- checkout the pinned ref
- run `zig build lib-vt --prefix "$REPO/.tools/ghostty-install"`
- leave the install prefix in a deterministic repo-local path

Keep it idempotent. Re-running should refresh the checkout and rebuild/install without manual cleanup.

- [ ] **Step 3: Document the helper workflow**

Update `README.md` with a local feature-on flow:

```bash
./tools/prepare-ghostty-vt.sh
cargo build -p cleat --locked --features ghostty-vt
```

- [ ] **Step 4: Run the helper and verify the install tree**

Run from `/home/robert/dev/cleat`:
```bash
./tools/prepare-ghostty-vt.sh
find .tools/ghostty-install -maxdepth 3 | sort
```
Expected: headers + `libghostty-vt.a` + `libghostty-vt.so` exist under the install prefix

- [ ] **Step 5: Commit**

```bash
git add tools/prepare-ghostty-vt.sh README.md AGENTS.md
git commit -m "build: add ghostty vt prepare helper"
```

### Task 3: Prefer static `libghostty-vt.a` in `cleat` build/linking

**Files:**
- Modify: `/home/robert/dev/cleat/crates/cleat/build.rs`
- Modify: `/home/robert/dev/cleat/crates/cleat/src/vt/ghostty_ffi.rs` (only if needed)
- Test: `/home/robert/dev/cleat/crates/cleat/tests/vt.rs`

- [ ] **Step 1: Write the failing build-contract test or add assertions to existing build validation tests if present**

If `build.rs` is hard to unit-test directly, add a focused regression test or a narrow helper function that can be tested in Rust. The important behavior to pin down is:
- static archive preferred when both `.a` and `.so` are present
- clear error when the prefix is missing or incomplete

- [ ] **Step 2: Update `build.rs` to prefer static linking**

Implement this order when `ghostty-vt` is enabled:
1. locate the Ghostty prefix (likely `.tools/ghostty-install`, plus env override if you keep one)
2. confirm header exists
3. if `lib/libghostty-vt.a` exists:
   - emit `cargo:rustc-link-search=native=...`
   - emit `cargo:rustc-link-lib=static=ghostty-vt`
4. else if shared library exists:
   - emit dynamic link instructions as fallback
5. else fail with a precise error

Do not auto-run the helper from `build.rs`.

- [ ] **Step 3: Verify the feature-on build against the prepared prefix**

Run:
```bash
./tools/prepare-ghostty-vt.sh
cargo build -p cleat --locked --features ghostty-vt
```
Expected: PASS, with static linking preferred if the archive is present

- [ ] **Step 4: Run the relevant feature-on tests**

Run:
```bash
cargo test -p cleat --locked --features ghostty-vt
```
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/cleat/build.rs crates/cleat/tests/vt.rs
git commit -m "build: prefer static ghostty vt linking"
```

### Task 4: Add dedicated Ghostty feature CI job

**Files:**
- Modify: `/home/robert/dev/cleat/.github/workflows/ci.yml`
- Modify: `/home/robert/dev/cleat/README.md`

- [ ] **Step 1: Add the dedicated CI job without disturbing default jobs**

Keep existing default jobs:
- format
- clippy
- test

Add one new job, e.g. `ghostty-vt`, that:
- installs Zig 0.15.2
- caches `.tools/ghostty-src` and `.tools/ghostty-install`
- runs `./tools/prepare-ghostty-vt.sh`
- runs:
  - `cargo build -p cleat --locked --features ghostty-vt`
  - `cargo test -p cleat --locked --features ghostty-vt`

- [ ] **Step 2: Choose the Zig install path in CI**

Use either:
- `mlugg/setup-zig` pinned by commit SHA, or
- direct `ziglang.org`/mirror download in the workflow

Prefer the simplest reproducible option and document it in the workflow comments.

- [ ] **Step 3: Document CI expectations**

Update `README.md` to say:
- default CI is still Rust-only
- one extra job validates `ghostty-vt`
- current Ghostty dependency is pinned to the temporary fork until upstream catches up

- [ ] **Step 4: Verify workflow syntax and local commands**

Run locally:
```bash
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build -p cleat --locked --features ghostty-vt
cargo test -p cleat --locked --features ghostty-vt
```
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/ci.yml README.md
git commit -m "ci: add ghostty vt validation job"
```

### Task 5: Final standalone repo verification and handoff

**Files:**
- Modify: `/home/robert/dev/cleat/README.md` (only if final clarification is needed)
- Modify: `/home/robert/dev/cleat/AGENTS.md` (only if command guidance changed)

- [ ] **Step 1: Run final default-path verification**

Run:
```bash
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```
Expected: PASS

- [ ] **Step 2: Run final Ghostty-path verification**

Run:
```bash
./tools/prepare-ghostty-vt.sh
cargo build -p cleat --locked --features ghostty-vt
cargo test -p cleat --locked --features ghostty-vt
```
Expected: PASS

- [ ] **Step 3: Confirm the repo status is clean and summarize the contract**

Run:
```bash
git status --short
```
Expected: only intended tracked changes, no accidental build artifacts committed

Summarize in docs/comments:
- pinned Ghostty fork/ref
- pinned Zig version
- repo-local `.tools` working directories
- static `libghostty-vt.a` preferred
- shared `.so` retained only as fallback / by-product

- [ ] **Step 4: Commit final doc polish if needed**

```bash
git add README.md AGENTS.md
git commit -m "docs: finalize ghostty vt build contract"
```

- [ ] **Step 5: Push and open PR**

```bash
git push -u origin <branch>
```
Then open a PR in `flotilla-org/cleat` with the Ghostty build/CI hardening summary and exact verification commands.
