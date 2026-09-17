# Retained image delivery validation

This change implements [ADR 0005](adr/0005-retained-image-delivery.md) on cleat base `c5eaa36`, using pinned Ghostty `c3dbb925e6cbcfceafba5749f81a486dd2275099`. Validation ran on arm64 macOS. The public C ABI remains version 8; the daemon packet protocol changes from 7 to 8, requiring matching daemon/client cleat builds.

## Automated checks

- `./tools/prepare-ghostty-vt.sh`: passed.
- `cargo build -p cleat --locked --features ghostty-vt`: passed.
- `cargo +nightly-2026-03-12 fmt --check`: passed.
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: passed.
- `cargo test --workspace --locked`: 532 passed, 2 ignored helper/platform tests.
- `cargo test -p cleat --locked --features ghostty-vt`: 532 passed, 2 ignored.
- `cargo test -p cleat --lib --locked --no-default-features`: 203 passed.

New behavior tests cover exact-generation acquisition, shared file lifetime across two readers, fallback after rejected file acquisition, 1920×1080 RGBA delivery beyond the packet ceiling, unchanged-generation reuse, replacement, eviction/reappearance, and malformed/incomplete transfer rejection. The large-image assertion checks transport framing and content, not throughput.

The Ghostty-feature ingress test accepts direct, regular-file and POSIX shm input, removes the producer's source, and then captures and delivers the retained pixels to a late receiver. The shm case checks that the engine has unlinked the producer's name. These are real external-media operations, separate from the injected capture tests.

A terminal-engine round trip feeds CLI renderer output into a second Ghostty VT engine and checks the resulting pixel bytes and placement state. It covers file upload replies, replacement, panning/crop coordinates and deletion. Another test checks Unicode placeholder input becomes a resolved output placement with no placeholder glyph leakage. Unit tests cover outer-terminal file errors falling back to direct upload and fragmented upload replies being kept out of application input.

## Wheelhouse C ABI integration

Reused `tools/cleat-image-baseline.c` and `tools/probe-cleat-reconnect.py` from `~/dev/wheelhouse-worktrees/cleat-image-readiness` at `0216a5b`. Compiled the probe against this checkout's header and debug dylib, with an explicit rpath. Used the existing `explicit-rgb` stage from the clean image-suite checkout at `c33f076`.

All sessions were isolated under `/tmp/cleat-206-validation/`, daemon name `wh-image-baseline`. Results:

| Scenario | Observed result |
| --- | --- |
| Daemon live, primary viewer | Successful lookup, 4,704 bytes |
| Simultaneous second viewer | Successful lookup, 4,704 bytes |
| Destroy/recreate second viewer | Successful lookup, 4,704 bytes |
| Forced socket disconnect/reconnect | Disconnected and recovered states observed; successful image lookup after recovery |
| Image scrolled into history, then TOP | Viewport kind 3, offset 63, successful lookup of 4,704 bytes |

The probe counts returned updates and lookup calls, not unique GPU uploads or wire transfers. No GPU-cache redesign was needed. Logs are retained locally in `/tmp/cleat-206-validation/`; they are not portable test fixtures.

## Limits of this evidence

Computer-use safety controls denied access to both Kitty and Ghostty applications. No visual GUI assertion is claimed. The terminal-engine checks establish protocol/pixel/placement state, not GPU compositing or screenshots. The native Wheelhouse GUI was not rebuilt as part of this cleat change.

The default local path shares one file backing per captured generation; acquiring an attachment name uses a hard link and Unix mapping rather than copying pixels. Ghostty still eagerly decodes and retains its own pixels. The additional write into daemon-owned backing remains. Decode time, disk pressure and sustained 30/60 fps performance have not been measured; no zero-copy or frame-rate claim is made.

Standard receiver-deleted shm output, producer-owned shm leases, deferred decoding and Jackstay streaming are deferred. CLI output accepts the existing uncompressed RGB/RGBA/PNG representations; the engine currently returns decoded RGB/RGBA. Unsupported or unresponsive outer terminals stop receiving uploads after the bounded reply timeout. Normal cleanup is tested; abrupt process death can leave temporary names, as recorded in the ADR.
