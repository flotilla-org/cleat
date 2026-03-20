# cleat

## Core commands

```bash
cargo build --locked
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

## Notes

- `ghostty-vt` is optional and should stay out of the default build.
- When validating Ghostty integration changes, run feature-on checks explicitly.
