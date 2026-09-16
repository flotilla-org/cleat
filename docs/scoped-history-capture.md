# Scoped history capture

Cleat pins Ghostty `c3dbb925e6cbcfceafba5749f81a486dd2275099`, published on
`rjwittams/ghostty` branch `patches/libvt-scoped-capture`. This adds the capture
foundation for independent attachment views. Attachment scheduling, transport,
roles, command mode and chrome are separate work.

`GhosttyVtEngine::history_view` tracks a position on a chosen screen.
`capture_history` captures a terminal-sized view starting there, moving the
effective origin upwards when needed to fill the last viewport. It leaves the
tracked cell in place so Ghostty can follow it through reflow. Primary-screen
history remains readable while an application uses the alternate screen.

The Ghostty patch adds explicit-screen tracked references, history observation
tokens, a scoped render-state capture, and scoped graphics queries. Capture
reuses the existing render-state row/cell readers. It rebuilds the scratch
state without consuming terminal dirtiness or moving the shared viewport.
History frames exclude the application cursor and global selection.

Cleat creates the history reader lazily. Its existing live reader continues to
use incremental updates. Frames own their cells and link bytes; ready image
versions use `Arc` and a generation-keyed weak cache. Frames survive later
terminal mutations, image replacement and engine destruction. Pending images
retain placement metadata and acquire bytes on a later capture; incomplete
versions never enter the cache.

The existing limitation for relative image placements rooted at virtual
placements remains; the corresponding integration test stays ignored.

Automatic eviction moves a lost view to the oldest surviving row and reports
`history_discarded` on the next successful capture. A failed capture does not
consume that notice. Explicit history erasure, reset or screen replacement
returns `ReturnToLive`. Foreign-engine views are rejected.

Callers provide per-capture cell and resource budgets. The resource budget
counts URI and ready image bytes, including shared image bytes, rather than
total allocator usage. It does not bound grapheme storage, placement metadata,
Ghostty scratch allocations, or frames retained by callers. The future
attachment host must bound retained frames and schedule captures fairly.
Fallible allocations and Ghostty errors are returned without publishing a
partial frame; this is not a guarantee of recovery from every Rust allocation
failure. Callers can keep their last successful frame after an error.

## Validation

The focused Ghostty tracked-reference, render and graphics test targets pass
(209 test executions across overlapping targets), including capture allocation
failure injection. The full ReleaseSafe suite still fails to compile at the
pre-existing `Terminal: fullReset tracked pins` assertion requiring
`slow_runtime_safety`. This also occurred on the baseline during prototyping.

C compiler probes checked the new history struct and the imported point and
grid-reference layouts for macOS aarch64, Linux x86_64 and Windows x86_64.
Rust tests check the corresponding 64-bit layouts. Behavioral tests cover
live dirtiness, inactive-primary links, independent reflow, foreign views,
reset/clear, budget retry, eviction notices, shared image versions and retained
image bytes. Cross-target layout probes do not substitute for native Linux
or Windows runtime validation.

On macOS, the published pin passes the preparation helper, the exact workspace
format/clippy/test commands from `AGENTS.md`, and the explicit `ghostty-vt`
build and tests. `cargo test --workspace --locked --no-default-features` also
passes in a separate serial run.
