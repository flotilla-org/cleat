# cleat

## Core commands

```bash
./tools/prepare-ghostty-vt.sh   # once per checkout; fetches + builds the pinned Ghostty VT
# Windows: tools\prepare-ghostty-vt.ps1 (also fetches the pinned bundled ConPTY, ADR 0006)
cargo build --locked
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

## Rust toolchain

`rust-toolchain.toml` pins the compiler and Clippy to Rust 1.98.1, the version
used by the successful CI run preceding this pin. Run Cargo through rustup from
this checkout; it installs the pinned toolchain and Clippy automatically. GitHub
and Forgejo CI use the same file. When updating it, run the commands above and
require the platform CI jobs to pass. Rust build cache keys include the pin.

Formatting intentionally keeps its separate `nightly-2026-03-12` pin because
`rustfmt.toml` uses nightly options; use the explicit format command above.
If Clippy differs between a crew image and CI, check `rustup show active-toolchain`
and `cargo clippy --version` first. An explicit `+toolchain`, `RUSTUP_TOOLCHAIN`,
or a rustup directory override can take precedence over the file.

## Container lifecycle tests

The unfiltered `cargo test --workspace --locked` remains the CI gate. In crew
containers, PTY/session teardown and orphan reaping can differ from a normal
runner: an exited process can remain a zombie if PID 1 does not reap it, and
`kill(pid, 0)` still reports that PID as present. Allocating a PTY alone does not
provide an init process that reaps orphans.

PR #248 reproduced these `crates/cleat/tests/lifecycle.rs` failures on its
unchanged base while the unfiltered CI suites passed:

- `daemon_provider_ffi_attach_roles_directory_and_close` (Ghostty only): exercises
  real PTY attachments, role changes, CLI detach and closed-channel notification.
  This was a container-only lifecycle failure; a new failure still needs its
  failing wait/assertion checked rather than assuming every failure is a zombie.
- `kill_terminates_background_children_in_leader_process_group`: used to mistake
  unreaped zombies for live children. The current test uses
  `signal_fixture_is_running`, which treats Linux zombies as terminated, so this
  historical failure should no longer require exclusion on the current base.

Run the named test alone to inspect a recurrence, for example:
`cargo test -p cleat --locked --test lifecycle daemon_provider_ffi_attach_roles_directory_and_close -- --exact --nocapture`.
If it fails, compare against the unchanged base in the same container. Only for a
confirmed baseline/environment failure may a supplemental local run use
`-- --skip daemon_provider_ffi_attach_roles_directory_and_close`; report the
exclusion and require unfiltered CI to pass. Do not add blanket container skips
or dismiss new failures under these names. Use a runner with working PTYs and
orphan reaping when validating process teardown itself.

## Notes

- `ghostty-vt` is a **default feature**: a plain `cargo build` produces a functional binary, and fails with an actionable message if the prepared Ghostty install is missing (run the prepare script above).
- The VT-less placeholder variant (testing only) is an explicit opt-out: `cargo build -p cleat --locked --no-default-features`. CI's `no-vt` job keeps it building.

## Roadmap and issue triage

Read [ROADMAP.md](ROADMAP.md) before choosing work. It records the ordered queue,
accepted decisions and the [tracker label conventions](ROADMAP.md#tracker-conventions).
Issue priority and readiness are independent. Update the roadmap and issue status
when decisions change; use native blocked-by relationships for actual dependencies.
