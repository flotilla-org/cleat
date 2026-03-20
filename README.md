# cleat

Session daemon with a structured control plane for agents and terminal persistence.

## Status

This repository is being split out from the Flotilla monorepo. The first standalone import keeps the existing `cleat` crate, tests, and optional `ghostty-vt` integration path.

## Development

```bash
cargo build --locked
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

## Optional Ghostty VT Engine

The `ghostty-vt` feature is optional and currently expects a local `libghostty-vt` install.

```bash
cargo build -p cleat --locked --features ghostty-vt
```
