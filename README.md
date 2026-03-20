# cleat

Session daemon with a structured control plane for agents and terminal persistence.

## Status

This repository is being split out from the Flotilla monorepo. The first standalone import keeps the existing `cleat` crate, tests, and optional `ghostty-vt` integration path.
The Ghostty path stays feature-gated and out of the default build.

## Development

```bash
cargo build --locked
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

## Optional Ghostty VT Engine

The `ghostty-vt` feature is optional and currently expects a local `libghostty-vt` install. The future helper flow will consume pinned inputs from [`tools/ghostty-toolchain.toml`](tools/ghostty-toolchain.toml), including Ghostty fork/ref and Zig `0.15.2`.

```bash
cargo build -p cleat --locked --features ghostty-vt
```

Default builds and CI remain Ghostty-free unless the feature is enabled explicitly.
