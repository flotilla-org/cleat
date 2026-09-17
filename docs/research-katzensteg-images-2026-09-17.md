# Katzensteg image producers for cleat #206

Read-only inspection on 2026-09-17. Main checkout: `~/dev/katzensteg`, commit `6f8ba89`. No builds or live producers were run. Sources below refer to that local checkout. Applicable agent guidance describes `termscene` as the reusable Kitty backend, the preload runtime as the active workstream, and the launcher as the supported way to run profiles.

## Producer shapes and checkouts

`git worktree list` identifies these sibling directories as worktrees of the same repository: `katzensteg-bootstrap` (`jackstay-source-bootstrap`, `39c4237`), `katzensteg-hosted-panels` (`hosted-producer-panels`, `c55673f`), `katzensteg-input` (`jackstay-input`, `db10cb2`), `katzensteg-wrap` (`wm-wrap`, `260d6c5`), `katzensteg-zig16` (`zig-0.16`, `5b724f0`), and `katzensteg.mac-capture` (`mac-capture`, `0274c44`). There is also an SDL3 worktree at `~/.codex/worktrees/5458/katzensteg`. These are branch incarnations, not independent projects to upgrade individually. This inspection traced current main rather than auditing every historical branch. `katzensteg-site` has its own `flotilla-org/katzensteg-site` remote.

Main already supports several presentation shapes:

- Direct terminal presentation from the SDL2/SDL3, GL, Vulkan, and Metal capture machinery. Reusable `termscene` also drives `ttytris` and `termscene-demo`.
- Hosted producer batches: JSONL groups of Kitty uploads, placements, and deletes, applied to a terminal by a host. Groups apply in delete/upload/place/after-delete order. See [terminal_batch_applier.zig](/Users/robert/dev/katzensteg/src/katzensteg/terminal_batch_applier.zig:9).
- WM positioned presentation: multiple producers with explicit cropped placements and z order.
- WM/headless/wrap placeholder presentation: host-owned image IDs and Unicode placeholder grids. Each completed frame replaces pixels under a stable image ID and refreshes the same virtual placement. Headless and wrap select placeholder mode. See [wm/cli.zig](/Users/robert/dev/katzensteg/src/katzensteg/wm/cli.zig:95), [render_batch_sink.zig](/Users/robert/dev/katzensteg/src/katzensteg/render_batch_sink.zig:213), and [architecture.md](/Users/robert/dev/katzensteg/docs/architecture.md).
- Jackstay publication is another output branch, selected before normal terminal-backend setup. Its borrowed media vocabulary includes RGBA8/BGRA8, stride, timestamp, and sequence. It is not evidence of Kitty `t=s` support. See [runtime.zig](/Users/robert/dev/katzensteg/src/katzensteg/runtime.zig:327) and [jackstay/media.zig](/Users/robert/dev/katzensteg/src/jackstay/media.zig:7).

## Bytes, ownership, and replies

The inspected Kitty writers send raw RGBA (`f=32`), with width/height and numeric image ID. Direct data is base64 chunked into 3072-character payloads with `m=1/0`; regular-file uploads encode a path, either whole-file or with `S` byte length and `O` offset. See [protocol.zig](/Users/robert/dev/katzensteg/src/termscene/kitty/protocol.zig:32) and [protocol.zig](/Users/robert/dev/katzensteg/src/termscene/kitty/protocol.zig:128).

No Kitty PNG (`f=100`), compressed-payload, temporary-file (`t=t`), or shared-memory (`t=s`) writer was found in the inspected main source/examples. PNG code is used for observation export, not the Kitty upload path: [frame_observation.zig](/Users/robert/dev/katzensteg/src/katzensteg/frame_observation.zig:39). This producer therefore supports a raw-pixel first implementation, but cannot settle the broader encoded-retention choice for other producers.

Whole-file mode rotates through 256 regular files. It overwrites their bytes, syncs before emitting the path, and reuses the file after cycling the pool. Offset mode wraps a mutable file at a configured high-water mark (10 MiB default in the backend). Neither path waits for a per-upload terminal acknowledgment before reuse. Deinitialization deletes the files. See [backend.zig](/Users/robert/dev/katzensteg/src/termscene/kitty/backend.zig:9), [backend.zig](/Users/robert/dev/katzensteg/src/termscene/kitty/backend.zig:96), [render_batch_sink.zig](/Users/robert/dev/katzensteg/src/katzensteg/render_batch_sink.zig:253), and [render_batch_sink.zig](/Users/robert/dev/katzensteg/src/katzensteg/render_batch_sink.zig:536).

Normal commands use `q=2`. Direct-runtime debug mode changes quiet mode to `q=0` for reply logging. Capability probes request a graphics response and separately query whole-file and offset-file support; profile selection prefers offset, then whole file, then direct. See [runtime.zig](/Users/robert/dev/katzensteg/src/katzensteg/runtime.zig:384), [capabilities.zig](/Users/robert/dev/katzensteg/src/termscene/kitty/capabilities.zig:68), and [profile.zig](/Users/robert/dev/katzensteg/src/termscene/kitty/profile.zig:10).

The WM is stricter than that fallback sequence: its JSONL producer path maps a requested or probed `direct_apc` profile to `file_whole`. Thus a direct-only cleat implementation would miss a central use case even if basic demos worked. See [wm_host.zig](/Users/robert/dev/katzensteg/src/katzensteg/wm_host.zig:2115).

Design implication: cleat must ingest file bytes while processing the producer's command and retain an owned asset. A queued reference to a producer path/offset can silently become a newer frame, disappear on producer exit, or be inaccessible to a remote viewer. Existing file reuse is time-based, so an intentionally stalled intake test may expose a producer-side ownership problem too. Adding a consumer copy after an arbitrary delay cannot eliminate that upstream race. Producer ACKs, if introduced, should describe daemon intake, not completion by every viewer; quiet uploads currently supply no such fence.

## Identity and placement behavior

Explicit placement commands use cursor addressing plus `a=p,C=1,i,p,c,r,x,y,w,h,z`. Exact deletion targets `(image_id, placement_id)` using `d=i`; data retirement uses uppercase `d=I`. The backend tracks logical sprite keys and retains placement IDs across updates. Reuploading an existing image marks it for re-placement even when the scene diff contains no change. See [protocol.zig](/Users/robert/dev/katzensteg/src/termscene/kitty/protocol.zig:88) and [backend.zig](/Users/robert/dev/katzensteg/src/termscene/kitty/backend.zig:121).

Frame composition also allocates fresh image IDs, uploads a strip shared by several cropped tile placements, and deletes obsolete placements/data once no tiles reference them. IDs wrap inside the builder's configured allocation range. See [frame_builder.zig](/Users/robert/dev/katzensteg/src/katzensteg/frame_builder.zig:2482), [frame_builder.zig](/Users/robert/dev/katzensteg/src/katzensteg/frame_builder.zig:2950), and [frame_builder.zig](/Users/robert/dev/katzensteg/src/katzensteg/frame_builder.zig:3072).

Placeholder mode does the opposite: it repeatedly uploads a host-selected stable image ID and issues `a=p,U=1,p=1,c=...,r=...`. Cell-only resize reissues placement; pixel-size changes rescale/recompose and upload. The sink retains completed-frame pixels and can restore after a terminal clear. Its tests explicitly cover stable IDs, changed dimensions, restoration, and owned pixels. See [render_batch_sink.zig](/Users/robert/dev/katzensteg/src/katzensteg/render_batch_sink.zig:213) and its tests at [line 792](/Users/robert/dev/katzensteg/src/katzensteg/render_batch_sink.zig:792).

Cleat asset identity therefore needs a generation independent of producer image ID. A stable-ID frame replacement must invalidate viewer caches. Placement changes must not retransmit an unchanged asset. Several placements may share one asset with different crops; deleting one placement must not evict data needed by the others or by retained history. Placeholder image/placement identity must remain consistent with encoded placeholder cells if an attachment remaps terminal IDs.

## Validation and useful independent work

Start with bounded deterministic fixtures derived from existing code, before launching real applications:

1. Tiny direct RGBA upload, placement, same-ID pixel replacement, exact placement delete, and data delete.
2. Whole-file and offset-ring uploads with deliberately distinct patterns before and after reuse. Attach a second viewer after the original file is overwritten/deleted; reconnect after producer exit.
3. One image shared by multiple crops and z layers; pan smaller watchers across both axes and check border/chrome clipping.
4. Stable-ID virtual placement with a Unicode placeholder grid; replace pixels, resize cell dimensions, resize pixel bounds, clear/restore, and reconnect.
5. Frame churn at a declared 30/60 fps test rate with one slow viewer, checking bounded queue/asset memory and newest complete frame. These rates are proposed test parameters, not measured throughput: current runtime defaults to no explicit presentation cap and derives its interval from `present_fps` when set ([runtime.zig](/Users/robert/dev/katzensteg/src/katzensteg/runtime.zig:284)).

Existing seeds include [kitty-placement-repro](/Users/robert/dev/katzensteg/examples/kitty-placement-repro/main.zig:1), the RGBA/batch unit tests, `kitty-show-ppm`, `termscene-demo`, and the recorded [pi-agent-frame-batch-fixture.jsonl](/Users/robert/dev/katzensteg/examples/pi-agent-frame-batch-fixture.jsonl). A later end-to-end pass should use launcher profiles for an SDL probe, a streaming/game profile, and a multi-producer WM or wrap session.

An independent agent could prepare a small deterministic producer/fixture harness now: selectable direct/file/file-offset transport, finite frame count, exact expected IDs/crops, stable-ID placeholder mode, and file-lifetime cases. That requires no cleat protocol decision and gives both CLI and Wheelhouse the same stimuli. A separate producer task could specify/test file reuse ownership under stalled readers; do not combine an ACK redesign with cleat's first restoration patch without agreeing which layer acknowledges intake. No Katzensteg dependency upgrade is required merely to begin: current main already contains the relevant presentations.
