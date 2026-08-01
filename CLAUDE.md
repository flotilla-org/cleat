# cleat

## Core commands

```bash
./tools/prepare-ghostty-vt.sh   # once per checkout; fetches + builds the pinned Ghostty VT
cargo build --locked
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

## Notes

- `ghostty-vt` is a **default feature**: a plain `cargo build` produces a functional binary, and fails with an actionable message if the prepared Ghostty install is missing (run the prepare script above).
- The VT-less placeholder variant (testing only) is an explicit opt-out: `cargo build -p cleat --locked --no-default-features`. CI's `no-vt` job keeps it building.
