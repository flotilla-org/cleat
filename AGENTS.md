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
- The future Ghostty helper will read pinned inputs from [`tools/ghostty-toolchain.toml`](tools/ghostty-toolchain.toml), including Ghostty fork/ref and Zig `0.15.2`.
- Keep the metadata file minimal and only add fields that the helper/build flow will actually consume.

## Repo Scope

This repository is the standalone home for `cleat`, the session daemon and control-plane CLI extracted from Flotilla.
