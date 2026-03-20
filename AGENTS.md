# Agent Notes

## Test Command Defaults

- CI parity (format): `cargo +nightly-2026-03-12 fmt --check`
- CI parity (clippy): `cargo clippy --workspace --all-targets --locked -- -D warnings`
- CI parity (test): `cargo test --workspace --locked`

If you say a change matches CI locally, it should have been checked against these exact commands.

## Testing Philosophy

- Prefer behavior tests that exercise domain logic through injected collaborators rather than real filesystem or process orchestration when a narrower seam exists.
- When multiple implementations exist, define the behavior once and run the same contract tests against each implementation where practical.
- Keep the optional `ghostty-vt` path explicitly feature-gated and verify it separately from the default build when changing that area.

## Ghostty Build Metadata

- `ghostty-vt` stays optional and must not affect the default Rust-only build.
- The local helper at [`tools/prepare-ghostty-vt.sh`](tools/prepare-ghostty-vt.sh) reads pinned inputs from [`tools/ghostty-toolchain.toml`](tools/ghostty-toolchain.toml), verifies Zig `0.15.2`, clones or refreshes the Ghostty fork in `.tools/ghostty-src`, and installs the Ghostty VT headers and libraries into `.tools/ghostty-install`.
- Re-run the helper after changing the pinned ref or Zig version; it is expected to be idempotent and to refresh the repo-local checkout and install prefix.
- Verify the helper with `./tools/prepare-ghostty-vt.sh` followed by `find .tools/ghostty-install -maxdepth 3 | sort`.

## Repo Scope

This repository is the standalone home for `cleat`, the session daemon and control-plane CLI extracted from Flotilla.
