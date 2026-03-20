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

The `ghostty-vt` feature is optional and stays out of the default build. Use the repo-local helper to fetch the pinned Ghostty fork/ref and build a local install prefix under `.tools/`.

```bash
./tools/prepare-ghostty-vt.sh
CLEAT_GHOSTTY_PREFIX="$PWD/.tools/ghostty-install" cargo build -p cleat --locked --features ghostty-vt
cargo test --workspace --locked
```

The helper reads pinned inputs from [`tools/ghostty-toolchain.toml`](tools/ghostty-toolchain.toml), verifies the configured Zig version, clones or refreshes Ghostty into `.tools/ghostty-src`, and installs the Ghostty VT headers and libraries into `.tools/ghostty-install`. Until Task 3 updates `build.rs`, feature-on builds should point `CLEAT_GHOSTTY_PREFIX` at that repo-local install prefix explicitly.

```bash
find .tools/ghostty-install -maxdepth 3 | sort
```

Default builds and CI remain Ghostty-free unless the feature is enabled explicitly.
