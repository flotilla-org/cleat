# Ghostty migration, 2026-09-07

Cleat now pins Ghostty `2a4777cd774be6bc59ab9353cab97faef8b215fc` and Zig
`0.16.0`. The previous pins were Ghostty
`64daa599c531e6938bc4c52d9198a91f1e6ce8cf` and Zig `0.15.2`.

The dependency URL remains `https://github.com/rjwittams/ghostty.git`.
`git ls-remote` verified that `refs/heads/cleat-integration-staging` points to
the new full SHA. Fresh preparation fetched that exact SHA from GitHub on
macOS, Linux, and Windows. The migration can be reproduced from the published
dependency, subject to the compatibility limits below.

Work started from cleat `dbeeca6` in a separate worktree and branch,
`maintenance/ghostty-zig-016`. The original checkout and its prepared
installation were left intact. No branches or releases were pushed.

## Binding changes

| Surface | Migration |
| --- | --- |
| Terminal creation | Replace the by-value options struct with separate `uint16_t` columns and rows. Set the scrollback byte budget before registering callbacks; free the terminal if setting it fails. |
| Scrollback | Use option 27, `SCROLLBACK_MAX_BYTES`, with `size_t*`. Zero disables history. Leave `SCROLLBACK_MAX_LINES` unlimited. Both limits prune at page granularity. |
| Mode queries | Replace the removed `ghostty_terminal_mode_get` with `ghostty_terminal_get`, data 37. Initialize `GhosttyTerminalModeConfig.mode`; read `value` on success. Layout: `u16` at offset 0, `bool` at offset 2, size 4, alignment 2. |
| Render colors | Replace the removed `ghostty_render_state_colors_get` with `ghostty_render_state_get`, data 19, using the existing sized colors struct. |
| Temporary-file medium | Option 17 now consumes `GhosttyString*`, not `bool*`. Pass the process temporary directory as a borrowed UTF-8 pointer and byte length; Ghostty copies it during the call. Pass NULL to disable. |
| Results | Include `IO_ERROR=-5`, `LIMIT_EXCEEDED=-6`, and `REJECTED=-7`. |
| Pointer declarations | Use `GhosttyAllocator*` for allocator parameters and C `char*` for paste/mouse output buffers; cast owned Rust byte buffers at the call sites. |

The old terminal C header described `max_scrollback` as lines, but
`src/terminal/c/terminal.zig` forwarded it to `Screen.zig` and
`PageList.zig` as a byte allocation budget. The new constructor defaults to
10,000 bytes and no line limit. Explicitly setting the caller's byte budget
preserves cleat's previous behavior, including zero disabling history.

The temporary-file policy remains the one chosen for the fork: reads outside
approved deletion directories/names remain permitted; deletion requires an
allowed directory and a `tty-graphics-protocol-` filename. Ghostty also
recognizes its built-in temporary directories. The regression test checks
both deletion of an allowed filename and retention of another filename,
raw RGB transmission without an explicit `S`, and disabling the medium.

## ABI audit

The target's source headers and installed headers were inspected alongside
the old-to-new header diff and `ghostty_type_json()` manifest. Temporary
compiler probes checked all 54 imported function signatures and all four
callback signatures. Rust and C probes checked 51 declared value types,
119 field offsets and widths, and 195 declared enum values. These probes
passed on macOS aarch64, Linux x86_64, and Windows x86_64. Windows C probes
used `zig cc -c`; Unix used `cc -fsyntax-only`.

The audit covered:

- Allocator and system PNG callback: allocator pointer, image dimensions,
  allocated RGBA buffer, ownership transfer, userdata and callback options.
- Terminal new/free/resize/write/scroll/get/set, mode packing, scrollbar,
  screen identifiers, byte/line limits, color options, image media options,
  size reports, and all three terminal callbacks used by cleat.
- Formatter new/format-buffer/free, by-value terminal options, nested sized
  extra structs, format values, and the nullable selection pointer.
- Render-state new/free/update/get/set; row iterator new/free/next/get/set;
  row-cell new/free/next/select/get-multi; cell get-multi and row get.
  This includes colors, styles, tagged unions, buffers, raw cell/row words,
  graphemes, cursor fields, and dirty-state values.
- Kitty graphics/image selectors, image metadata and generation, placement
  iterator lifecycle, placement getters/render info, and all four retained
  virtual-placement iterator exports.
- Paste encoding; mouse encoder/event lifecycle, options, action/button/modifier
  values, position/size layouts, synchronization from terminal state, and
  output buffers.

Callback userdata remains heap-stable until terminal destruction. Callbacks
run synchronously and do not re-enter VT writes. PNG output uses the allocator
supplied by Ghostty and transfers ownership to Ghostty. Image byte pointers
remain borrowed only for the synchronous callback, without terminal mutation;
generation checks reject stale resources. Formatter and render buffers remain
caller-owned. Iterator handles are freed by their existing RAII wrappers.

The new image DATA_PTR documentation permits NO_VALUE for a pending restored
payload. Cleat does not call the snapshot decoder/compression APIs that produce
that state; its existing getter reports an error rather than dereferencing
a missing pointer. Animation continues to use Ghostty's image generation
stamps. This pass adds no snapshot restoration or animation scheduler.

No runtime metadata dependency or binding generator was added to cleat.

## Setup and CI

The Unix helper now reads the Zig version from the TOML pin. If the PATH
version differs or Zig is absent, it downloads the matching host archive into
the worktree's `.tools`, verifies its pinned SHA-256, and uses it without
changing the system Zig. Checksums for the supported macOS, Linux, and Windows
architectures come from the Zig 0.16.0 release index. The Windows installer
also verifies its archive checksum.

Both preparation helpers shallow-fetch the exact Ghostty commit on every run.
PowerShell now checks native Git and Zig exit codes. GitHub CI uses Zig 0.16.0;
the Forgejo worker no longer requires the old system Zig version before the
helper can install the pinned version.

Ghostty remains feature-gated. In this checkout it is enabled by default;
the Rust-only build uses `--no-default-features`. Existing linkage selection
is preserved: shared library when present, static fallback on Unix, and
import library plus DLL on Windows.

## Validation

| Host | Evidence |
| --- | --- |
| macOS aarch64 | Unix preparation passed twice, including downloading/checksumming Zig with system Zig still at 0.15.2. Feature build passed. All 42 VT integration tests passed with shared linking and again with an isolated static-only prefix; one known-gap test was ignored. The focused unit tests passed, including the new option regressions. Exact format and Clippy commands passed. Rust-only build and tests passed. |
| Linux x86_64, feta | Fresh preparation and feature build passed. Exact format, Clippy, workspace tests, explicit feature tests, and Rust-only build/tests passed. VT integration: 42 passed, one known-gap test ignored. |
| Windows x86_64, gouda | PowerShell preparation passed twice. An isolated Cargo harness compiled the migrated `ghostty_ffi.rs` directly and passed its nine unit tests plus a smoke test for PNG callback pixels, render colors/dimensions, ordinary relative placements, and virtual placements against the prepared candidate DLL, then passed all 10 tests again against the helper's fresh install. Full cleat build is blocked as described below. |

Required commands executed:

```sh
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build -p cleat --locked --features ghostty-vt
cargo test -p cleat --locked --features ghostty-vt
cargo build -p cleat --locked --no-default-features
cargo test -p cleat --locked --no-default-features
```

On macOS, both full test commands fail at
`cleat_attach_exits_when_session_is_killed`: 97 lifecycle tests pass, one
fails, and one helper is ignored. The isolated test fails on the unchanged
`dbeeca6` baseline with the old Ghostty install too. Capturing stderr showed
`missing session alpha` during attach/shutdown. This is not a new-Ghostty-only
failure; no lifecycle workaround is included.

Windows full compilation fails at `provider_daemon.rs:398` because
`platform::ipc::windows::SessionStream` lacks `shutdown`. The same compile
error occurs with `--no-default-features`. Those source files are unchanged
by the migration. Direct FFI validation establishes binding/DLL compatibility,
not a successful Windows cleat application build.

## Remaining Ghostty compatibility gap

Ordinary relative placements work through the C API. A chain rooted at a
virtual placement remains invisible through
`src/terminal/c/kitty_graphics.zig:placementViewportPos`, even though
Ghostty's native renderer supports it. The retained virtual-placement iterator
still exposes the parent placeholders; it does not enumerate their relative
children.

The ignored integration test asserts the desired child position and was run
explicitly. It fails after confirming the virtual parent is visible, because
image 2's relative placement is absent:

```sh
cargo test -p cleat --locked --features ghostty-vt --test vt \
  vt_ghostty_relative_placement_uses_virtual_parent_position -- --ignored --exact
```

This belongs in Ghostty's C viewport/relative-placement integration, alongside
the retained `patches/libvt-virtual-placement-resolver` patch. It cannot be
fixed merely by changing the Rust struct layout or using the existing virtual
iterator. No Ghostty refs were rewritten and no cleat-side placement workaround
was added. The migration is reviewable with this explicit compatibility gap;
it does not establish full virtual-parent relative-placement support.

Local validation artifacts are retained under
`.tools/migration-validation/` in the migration worktree: compiler probes,
the ABI manifest, macOS logs, Linux CI logs, and Windows DLL/build logs.
The Windows harness also remains at
`C:\Users\rober\cleat-windows-ffi-2026-09-07`; Linux validation remains at
`~/dev/cleat-ghostty-update-2026-09-07` on feta.
