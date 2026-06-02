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
- The local helpers at [`tools/prepare-ghostty-vt.sh`](tools/prepare-ghostty-vt.sh) and [`tools/prepare-ghostty-vt.ps1`](tools/prepare-ghostty-vt.ps1) read pinned inputs from [`tools/ghostty-toolchain.toml`](tools/ghostty-toolchain.toml), verify or install the configured Zig version, clone or refresh the Ghostty fork in `.tools/ghostty-src`, and install the Ghostty VT headers and libraries into `.tools/ghostty-install`.
- Re-run the helper after changing the pinned ref or Zig version; it is expected to be idempotent and to refresh the repo-local checkout and install prefix.
- `cleat` treats Ghostty as a prefix dependency: headers at `.tools/ghostty-install/include/ghostty/vt.h` and libraries under `.tools/ghostty-install/lib`. Static linking is preferred on Unix when `libghostty-vt.a` is available; shared library linkage remains a fallback. Windows links via `ghostty-vt.lib` and copies `ghostty-vt.dll` next to the built executable.
- Verify the helper with `./tools/prepare-ghostty-vt.sh` on Unix or `powershell -NoProfile -ExecutionPolicy Bypass -File tools\prepare-ghostty-vt.ps1` on Windows, then `cargo build -p cleat --locked --features ghostty-vt` and `cargo test -p cleat --locked --features ghostty-vt`.

## Repo Scope

This repository is the standalone home for `cleat`, the session daemon and control-plane CLI extracted from Flotilla.
