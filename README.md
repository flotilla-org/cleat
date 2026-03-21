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
```

On **Linux**:
```bash
LD_LIBRARY_PATH="$PWD/.tools/ghostty-install/lib" cargo build -p cleat --locked --features ghostty-vt
LD_LIBRARY_PATH="$PWD/.tools/ghostty-install/lib" cargo test -p cleat --locked --features ghostty-vt
```

On **macOS**:
```bash
DYLD_LIBRARY_PATH="$PWD/.tools/ghostty-install/lib" cargo build -p cleat --locked --features ghostty-vt
DYLD_LIBRARY_PATH="$PWD/.tools/ghostty-install/lib" cargo test -p cleat --locked --features ghostty-vt
```

The helper reads pinned inputs from [`tools/ghostty-toolchain.toml`](tools/ghostty-toolchain.toml), verifies Zig `0.15.2`, clones or refreshes Ghostty into `.tools/ghostty-src`, and installs the Ghostty VT headers and shared library into `.tools/ghostty-install`.

The `ghostty-vt` build path now defaults to the repo-local prefix at `.tools/ghostty-install`. You can still override it with `CLEAT_GHOSTTY_PREFIX`, but feature-on runs and tests must set the library path (`LD_LIBRARY_PATH` on Linux, `DYLD_LIBRARY_PATH` on macOS) so the loader can find the shared library.

```bash
find .tools/ghostty-install -maxdepth 3 | sort
```

Default builds and CI remain Ghostty-free unless the feature is enabled explicitly.
