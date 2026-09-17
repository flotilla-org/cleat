# Image fallback scheduling measurements, 2026-09-18

Work for [#215](https://github.com/flotilla-org/cleat/issues/215), measured against merged commit `332ea6d` on macOS arm64. Each result below is the median of three separate release-mode processes. These are socket-path measurements on one development machine, not terminal rendering benchmarks.

The 10 ms servicing tick limits throughput when a chunk drains the socket completely: the old wait logic sees an empty byte queue and sleeps even though the image transfer can produce more chunks. With the default small Unix socket buffer, partial writes keep that queue nonempty and the old path already wakes promptly. The original 6.4 MB/s estimate therefore describes a conditional bottleneck.

The change counts ready image transfers as writable work on Unix, rotates channels after each frame, and batches image frames up to a 256 KiB prefetch high-water mark. Each service pass writes at most 256 KiB and checks a soft 1 ms budget between frames and writes. One frame and one write attempt are always allowed to guarantee progress. A byte chunk can exceed the prefetch mark by its own encoded size (64 KiB plus headers). Large render/control frames still use the existing 4 MiB hard output limit. File offers awaiting acquisition replies are excluded from writable work.

| Scenario | Completion ms, before → after | Payload MB/s, before → after | Input/control probe ms, before → after | Process peak RSS MiB, before → after |
| --- | ---: | ---: | ---: | ---: |
| single | 24.57 → 21.18 | 337.52 → 391.60 | 3.07 → 0.83 | 29.62 → 28.08 |
| roomy | 1505.00 → 13.84 | 5.51 → 599.11 | 12.26 → 0.33 | 25.70 → 25.73 |
| multi | 106.66 → 73.90 | 311.06 → 448.92 | 5.23 → 0.64 | 57.88 → 58.28 |
| slow | 1374.87 → 1372.96 | 24.13 → 24.17 | 88.00 → 13.15 | 59.02 → 57.86 |
| stalled | 385.91 → 373.66 | 42.99 → 44.40 | 7.40 → 1.09 | 47.33 → 36.59 |

Each image is 1920 × 1080 RGBA (8,294,400 bytes). `single` uses one channel and the default Unix socket buffer. `roomy` requests a 256 KiB send buffer. `multi` uses four channels on one connection. `slow` uses four channels and sleeps 2 ms after each received frame. `stalled` uses two connections; one does not read for its first 350 ms.

The healthy connection in `stalled` completes in a median 23.88 ms before and 23.08 ms after, independently of the stalled reader. On one connection, round-robin scheduling makes the four images finish together; previously a channel could finish most or all of its image before the next channel received its share. This improves fairness but delays the first completed image when all four are requested at once.

The largest sampled pending byte queue across these runs falls from 2,155,274 bytes to 319,598 bytes. Samples are taken after flushing; the queue contract test checks the prefetch bound before flushing. RSS is the entire benchmark process high-water mark, including the shared source image, receiver caches, threads, allocator and harness. It is not an isolated daemon memory measurement. The receiver still needs memory for the images it assembles.

Completion ranges across the three samples:

| Scenario | Before ms | After ms |
| --- | ---: | ---: |
| single | 19.65–30.83 | 17.52–22.14 |
| roomy | 1495.25–1505.73 | 11.06–14.06 |
| multi | 77.33–199.06 | 72.81–82.48 |
| slow | 1368.29–1419.56 | 1362.65–1379.81 |
| stalled | 371.02–387.01 | 370.76–379.03 |

The input/control probe sends a valid Input frame after the first received frame. The service loop drains that input and queues a DirectoryDelta response. This measures protocol input servicing plus control output queue delay; it does not include PTY processing or application echo. `max_service_us` includes all clients in one loop pass and is diagnostic, not a hard real-time guarantee. OS scheduling can exceed the soft budget.

The benchmark uses the actual packet encoder, pending-output queue, nonblocking socket writes, input drain and output wait functions, plus ImageReceiver assembly and render commit. It omits capture, PNG decoding, Kitty upload and GPU display. It shares one immutable source image across transfers. No new image compression, transport format or file-copy operation is introduced. Windows retains the existing timer-based wait; these Unix results do not establish Windows performance.

Run a scenario with:

```sh
CLEAT_IMAGE_BENCH=roomy cargo test -p cleat --release --lib --locked \
  --no-default-features benchmark_image_fallback -- --ignored --nocapture
```

Valid scenarios are `single`, `roomy`, `multi`, `slow`, and `stalled`. Run each in a separate process three times. For the baseline, check out `332ea6d` in a separate worktree, copy the benchmark module through the end of `benchmark_image_fallback` (omit the new scheduler contract tests), and add its `cfg(all(test, unix))` module declaration to `session.rs`. Use separate target directories, or preserve the baseline executable and force recompilation after switching source trees. The scheduling benchmark deliberately disables Ghostty; normal repository checks exercise the default Ghostty build.

Regression contracts cover a queue that stops allocating under backpressure, round-robin progress across service passes, one-frame progress with an exhausted time budget, and file-offer waits that become writable after acknowledgement. Local-file acquisition remains the preferred delivery path.

Visual Kitty/Ghostty smoke validation remains outstanding: computer-use access was denied during the preceding image-delivery work. These results make no visual-rendering claim.

Validation passed:

```sh
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo test -p cleat --lib --locked --no-default-features session::packet_output_tests
```

The manual release benchmark is ignored by the normal test suite and was run separately as described above.
