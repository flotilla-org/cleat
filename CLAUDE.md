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

## Notes

- `ghostty-vt` is a **default feature**: a plain `cargo build` produces a functional binary, and fails with an actionable message if the prepared Ghostty install is missing (run the prepare script above).
- The VT-less placeholder variant (testing only) is an explicit opt-out: `cargo build -p cleat --locked --no-default-features`. CI's `no-vt` job keeps it building.

## Roadmap and issue triage

Read [ROADMAP.md](ROADMAP.md) before choosing work. It records the ordered queue,
accepted decisions and the [tracker label conventions](ROADMAP.md#tracker-conventions).
Issue priority and readiness are independent. Update the roadmap and issue status
when decisions change; use native blocked-by relationships for actual dependencies.
