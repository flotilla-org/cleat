# Ghostty re-pin ABI audit, 2026-10-10

Cleat now pins `rjwittams/ghostty`'s rebased `cleat-integration` head,
`1d7fb7a15838b82244a067bdaf7c8e4ed8130127`, replacing
`c361de9691f006f65c400be73896d1e48a8ec56c`. The old head remains archived at
`archive/cleat-integration-2026-10`. Zig stays **0.16.0** and the build flags stay
`-Demit-lib-vt=true -Dsimd=true -Doptimize=ReleaseSafe`.

The new dependency contains OSC 7501 program status. Cleat still registers
neither program-status nor title callbacks; consuming those effects is #326
step 2. No Rust FFI declarations or callback registrations needed changing.

## Header audit

Every hunk of `git diff c361de9691f006f65c400be73896d1e48a8ec56c
1d7fb7a15838b82244a067bdaf7c8e4ed8130127 -- include` was reviewed against
`crates/cleat/src/vt/ghostty_ffi.rs`, including its private clipboard and scoped
history declarations. All 13 changed headers are covered below. Paths after
the first row are relative to `include/ghostty/vt/`.

| Header | Changes and verdict |
| --- | --- |
| `include/ghostty.h` | Adds app-runtime window-resize action/struct/union member. **No binding change:** cleat uses the VT API, not this app-runtime action union. |
| `allocator.h` | Clarifies custom allocator alignment as a log2 exponent and `ghostty_alloc` returning NULL for zero bytes. **Compatible:** cleat supplies no custom allocator and checks the PNG allocation for NULL; its allocator pointer and allocation signature match. |
| `formatter.h` | Documents NULL/zero for empty allocating formatter output. **Compatible:** cleat uses the caller-owned buffer API; all nested sized formatter layouts and by-value constructor options match. |
| `kitty_graphics.h` | Expands documentation of the three scoped graphics functions; signatures and placement/image layouts are unchanged. **Compatible:** borrowed resources are copied under exclusive terminal access, and stale-origin `NO_VALUE` is handled. Ordinary and virtual placement structures and iterator signatures match. |
| `mouse.h` | Adds `GhosttyMouseShape` values 0–33 and includes `types.h`; existing event/encoder subheaders are unchanged. **Compatible:** cleat's mouse action/button/modifier/option IDs, position/size layouts and encoder signatures match; pointer-shape queries are unused. |
| `osc.h` | Adds unknown OSC command 27, program status 28, terminator enum, unknown-data selectors 2–4, option 0 and `ghostty_osc_set`; documents cancellation/NULL results. **No binding change:** cleat feeds terminal bytes, not standalone OSC parser handles. No status callback is installed. |
| `render.h` | Adds overscan layout, row identity layout, data 20/21, option 1 and row data 6/7; expands capture/hold/iterator documentation. **Compatible:** bound selectors retain values, sizes and signatures. Overscan defaults to zero; scoped capture ignores it, excludes cursor/selection and preserves live dirtiness. Cleat continues its existing synchronized-output handling. |
| `search.h` | Documents `INVALID_VALUE` from tick after terminal destruction. **No binding change:** cleat does not bind this search API. |
| `selection.h` | Documents NULL/zero for empty allocating selection output. **No binding change:** cleat does not call that API; the formatter's nullable selection pointer is unchanged. |
| `snapshot.h` | Adds decoder compression option 2 and data 9, false by default; snapshot format stays unchanged. **No binding change:** cleat does not use Ghostty snapshot decoding. |
| `sys.h` | Clarifies exact RGBA allocation length, allocator ownership, callback-duration borrows, zeroed output and failure handling. **Compatible:** cleat allocates the exact pixel byte length through the supplied allocator, rejects NULL, fills all image fields and transfers ownership on success. Image layout, callback and options match. |
| `terminal.h` | Adds memory-usage struct; unknown-OSC union member; program-status, semantic-prompt, reset and render-hold types/callbacks; options 40–46 and data 41/42. Expands reset/resize/mode/history documentation. **Compatible:** every bound selector and signature is unchanged, including clipboard option 26 and `requires_completion`. Existing sized history and clipboard layouts match. New options are left at defaults; no new callbacks are registered. |
| `types.h` | Guarantees valid non-NULL storage for empty library-produced strings. **Compatible:** string pointer/length layout is unchanged; cleat copies borrowed text synchronously. |

## Compiler evidence

Scratch probes extracted the actual Rust declarations, obtained their sizes,
alignments and field offsets with Rust 1.98.1, and checked those against the new
C headers with compiler assertions. They covered **63 value types**, **151 field
offsets and widths**, **212 enum values/constants**, **65 imported function
signatures**, and **five callback signatures** (PTY writes, terminal size, device
attributes, PNG decoding and clipboard writes).

The C signature probe maps Rust's erased scoped-history `*mut c_void` handles
to `GhosttyTrackedGridRef` and its output pointer; their ABI is pointer-sized.
The clipboard location/result and point tag use C integers with the matching
enum width; clipboard location values 0–2 and reply values 0–4 were checked
against `terminal.h`. The clipboard reply function's two const-pointer
parameters were inspected as well. This audit checks the consumed subset, not
unused additions to Ghostty's API.

Native Linux x86_64 probes passed against both source and installed headers.
The same C layout/signature assertions also compiled to objects with Zig 0.16.0
for macOS aarch64 and Windows x86_64. These are cross-compilation checks, not
native runtime tests. Mode macros, which call `ghostty_mode_new`, were checked
at runtime on Linux instead of pretending they are C constant expressions.

Representative 64-bit sizes (bytes):

| Layout | Size | Details |
| --- | ---: | --- |
| `GhosttyTerminalModeConfig` | 4 | mode at 0, bool at 2; alignment 2 |
| `GhosttyString` / `GhosttyBuffer` | 16 / 24 | pointer plus length / capacity and length |
| `GhosttyTerminalScrollViewport` / `GhosttyPoint` | 24 / 24 | tagged padded unions; value at 8 |
| `GhosttyStyle` / `GhosttyRenderStateColors` | 72 / 792 | size word at 0; colors/style flags unchanged |
| Formatter screen extra / terminal extra / options | 16 / 32 / 56 | each size word remains initialized; nested screen at 16, extra at 16 |
| `GhosttyKittyGraphicsPlacementRenderInfo` / virtual info | 56 / 72 | size word at 0; geometry unchanged |
| `GhosttyMouseEncoderSize` / `GhosttySysImage` | 40 / 24 | sized mouse geometry; PNG data at 8 and length at 16 |
| `GhosttyTerminalHistoryState` / `GhosttyGridRef` | 48 / 24 | sized history tokens; borrowed grid node at 8 |
| Clipboard content / reply / write | 32 / 16 / 80 | write `requires_completion` at 72; reply function at 64 |

All Rust-created sized inputs/outputs still initialize `size` with
`size_of::<Self>()`. Clipboard requests are library-created; cleat checks the
size through `requires_completion` before accessing the request and replies
with its own initialized size word.

### Mutation check

Two scratch copies of the actual Rust declarations were mutated, leaving the
repository untouched. Changing `ClipboardWrite = 26` to 25 failed the C enum
assertion. Moving `requires_completion` ahead of `ctx` failed the clipboard
request size and field-offset assertions. Restoring the actual declarations
made the complete probe pass again.

The namespace collision C reproducer also produced byte-identical output to
its original fixture against the new library; the historical fixture SHA is
retained as provenance.

## Local validation

- Clean `./tools/prepare-ghostty-vt.sh` and a repeat preparation both built the
  exact new SHA with Zig 0.16.0.
- `cargo clippy --workspace --all-targets --locked -- -D warnings` passed.
- `cargo +nightly-2026-03-12 fmt --check` passed.
- `cargo test --workspace --locked` passed with the containing session's
  `CLEAT_RUNTIME_DIR`, `CLEAT_DAEMON`, `CLEAT_SESSION` and `CLEAT_OUTPUT_DAEMON`
  unset for the test subprocess. On Linux, inherited session coordinates
  disagree with the lifecycle tests' explicit external-client declarations;
  the initial ambient run failed HTTP upgrade admission before VT processing.
  The isolated run passed all suites, including Kitty virtual declarations,
  clipboard relay, scoped captures, attachments and render feeds.
- A C smoke test confirmed OSC 7501 reports/support queries emit no PTY reply
  with only the existing write-PTY callback registered.

Native Windows preparation and tests run through the existing Windows Ghostty
VT CI job. The existing macOS CI job checks transfer primitives without VT;
macOS ABI evidence above is cross-compiled, not a native Ghostty runtime run.
