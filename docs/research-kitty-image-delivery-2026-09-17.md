# Kitty image delivery: cross-project exploration

The transport recommendation below was revised after discussion: use reusable daemon-owned files for local consumers, with byte transfer as fallback. The accepted implementation contract is [ADR 0005](adr/0005-retained-image-delivery.md). Ghostty backing retention, producer-owned shm leases, and Jackstay streaming are later stages.

2026-09-17. Cleat base: `c5eaa36` (merged #212). This is source research and proposed sequencing, not an implemented design or performance result. Other projects were inspected read-only; no builds, benchmarks, or live sessions were run.

The starting point is [#206](https://github.com/flotilla-org/cleat/issues/206), with [#103](https://github.com/flotilla-org/cleat/issues/103) covering daemon byte delivery and [#102](https://github.com/flotilla-org/cleat/issues/102) setting the descriptor/payload and future transport constraints.

## What the consumers actually need

[Katzensteg findings](research-katzensteg-images-2026-09-17.md) show that regular-file intake is essential: the WM path forces whole-file delivery even when direct mode is selected. Its current Kitty writers send raw RGBA, reuse mutable files without a per-upload acknowledgement fence, and support both explicit cropped placements and stable-ID Unicode placeholder frames. The worktrees are several incarnations of the same repository; the investigation traced current main, not every branch.

[Wheelhouse findings](research-wheelhouse-images-2026-09-17.md) show that native image rendering already exists: metadata, synchronous byte lookup, GPU caching, crops, and z planes. CI pins cleat `bf47bbb`; local builds use sibling `../cleat`. Updating the CI pin is useful compatibility work, but cannot repair missing live bytes. Its image ABI is unchanged. The inspected standalone binary links the sibling debug dylib; the revision loaded by a running process remains unverified.

[Zellij findings](research-zellij-assets-2026-09-17.md) provide useful prior art in the local fork: producer input becomes owned before success, encoded PNG survives without full decoding, and per-client output resources have lifetimes separate from retained assets. Its soft memory quota and render-count cleanup timeout should not be copied without reconsideration.

## Cleat's present boundary

[Live render packets](../crates/cleat/src/packet.rs:210) contain no image bytes. [History dispatch](../crates/cleat/src/session.rs:3935) includes captured bytes. The daemon provider [replaces pending bytes](../crates/cleat/src/provider_daemon.rs:535) on receipt, and the C provider [replaces its lookup vector](../crates/cleat/src/provider_ffi.rs:2019) when consuming an update. Merely sending each asset once inside a render packet would therefore fail after subsequent updates or GPU-cache eviction. A persistent lookup/fetch contract is needed.

The [packet payload ceiling](../crates/cleat/src/packet.rs:34) is 4 MiB. A 1920×1080 RGBA frame is 8,294,400 bytes before serialization, so a single inline frame cannot be the general transport. This is arithmetic, not a throughput measurement. Asset transfers need bounded chunks or another framed transfer mechanism independent of render snapshots and their acknowledgement gate.

The [pinned Ghostty fork](../tools/ghostty-toolchain.toml:15) reads shared memory into owned bytes and unlinks the original object in [graphics_image.zig](../.tools/ghostty-src/src/terminal/kitty/graphics_image.zig:210). Its [completion path](../.tools/ghostty-src/src/terminal/kitty/graphics_image.zig:524) eagerly decompresses and decodes PNG. Cleat's [live callback](../crates/cleat/src/vt/ghostty.rs:720) exposes that completed representation. Preserving encoded source data therefore needs a Ghostty retention/tap API change; simply forwarding the original command cannot provide it.

The official [Kitty protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/#the-transmission-medium) also specifies destructive POSIX shared-memory consumption. Direct output uses base64 chunks no larger than 4096 bytes. These terminal-side transfer rules are distinct from the cleat daemon socket's framing.

## Proposed contract

1. **Own content at intake.** Producer paths and offsets are inputs, not replay assets. Acquire bytes before producer success and retain them independently of viewers. Quiet uploads still require ownership even when no reply is sent. Katzensteg's time-based reuse can race a stalled intake; cleat cannot recover bytes already overwritten before reading.
2. **Identify immutable generations.** Scope resource identity to the session incarnation plus image ID and generation. Placement changes and resource replacements are separate. Acquiring a resource for a captured view must pin that exact generation atomically with capture, or define a recoverable stale-view outcome; fetching the current image ID later can race replacement.
3. **Separate transfer from view state.** Render updates reference resources. Bounded asset messages deliver them to a retained daemon-provider cache, with a reliable lookup or refetch path after eviction. Slow viewers must not create unbounded queues, and cancellation of an obsolete transfer must release its holds. Decide explicitly whether a client keeps its last complete view while replacement assets arrive.
4. **Give each consumer its own residency.** Wheelhouse converts available bytes to textures. CLI attach uploads to its terminal and emits clipped placements. Reconnect reconstructs each consumer's required resources. CLI ID remapping must account for Unicode placeholder cells, not only explicit placement commands.
5. **Separate acknowledgements and retention.** Producer replies concern intake; render acknowledgements concern view consumption; outer-terminal upload replies concern a particular viewer's transfer. Viewer replies must not leak into the application PTY. Terminal errors must release transfer resources and invalidate/retry residency rather than count as successful display.
6. **Keep representation extensible.** Initially the existing raw RGB/RGBA callback can restore Katzensteg. Preserve format metadata and the option of encoded PNG. Retained encoded data and deferred decoding are separate choices: avoiding decode does not eliminate ownership copies. File/shm output can later be generated from owned assets per compatible viewer.

History and reconnect need explicit retention budgets, including asset generations referenced by historical views. A placement disappearing from one viewport is not an asset deletion. Existing recording [ADR 0003](adr/0003-recording-multi-track-and-image-capture.md) already distinguishes exact live ownership from potentially lossy historical retention; this exploration does not replace that policy.

## Work that can proceed independently

| Workstream | Concrete handoff | Dependency |
| --- | --- | --- |
| Wheelhouse compatibility refresh | In a clean worktree, update CI's cleat pin to the merged revision, build against that exact checkout, run existing diagnostics and attach/reconnect/multiple-viewer checks. Preserve daily-driver changes. | Ready now; does not fix #206. |
| Katzensteg fixture producer | Add finite deterministic direct/file/file-offset workloads, same-ID replacement, shared crops, placeholder refresh, source reuse/deletion, and a selectable frame rate. Reuse existing repros and the Kitty test harness before adding overlapping infrastructure. | Ready now; no producer upgrade required. |
| Ghostty retention spike | Identify and prototype the smallest owned-source acquisition API; compare current decoded bytes with encoded retention. Measure decode time, copied bytes, peak memory, and acquisition latency for PNG and raw RGBA. Keep lazy decoding a separate question. | Can run alongside fixtures; finish before locking representation choices. |
| Cleat delivery design and implementation | Specify generation acquisition, chunking, cache/refetch, budgets, and cancellation; then restore daemon-provider bytes and CLI uploads against the same contract. | Main work here; start contract design now. |
| Wheelhouse cache diagnostics | Exercise lookup retry, replacement, eviction/reappearance, and crops through injected byte/texture collaborators. | Basic diagnostics ready now; new decoder/retention semantics wait for the cleat contract. |

My suggested first dispatch is the Wheelhouse compatibility refresh and Katzensteg fixtures, while retaining cleat's delivery design here. Neither is a blocking prerequisite. Zellij needs no preparatory changes. Do not couple a new Katzensteg ACK/reuse protocol to the first restoration patch without an explicit intake contract.

## Evidence needed before calling restoration complete

Run the same assets through CLI attach and daemon-backed Wheelhouse, including two simultaneous viewers, late attach, reconnect after producer exit, history/live transitions, stable-ID replacement, and deletion. Cover direct, file, and shared-memory inputs through the combined producer/test harnesses; Katzensteg alone does not cover PNG or shared memory. Include placeholders, multiple crops, z order, panned watchers and chrome boundaries.

For streaming, use declared 30/60 fps workloads plus an intentionally slow viewer. Check queue/cache memory, upload counts for unchanged generations, time to a complete visible frame, and cleanup after errors/disconnect. No current measurements establish the best transport. A small-image demo is insufficient evidence for the file-heavy WM workload or frame sizes above the packet ceiling.
