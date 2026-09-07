# Attach and Windows follow-up, 2026-09-07

This follow-up fixes the lifecycle race and Windows IPC compilation failure
found during the [Ghostty migration](ghostty-migration-2026-09.md).

## Recording before the foreground grant

The CLI previously enabled recording after the daemon granted an attachment.
The lifecycle test observes the foreground marker and immediately kills the
session. On macOS, that kill consistently beat the later recording request,
which returned `missing session alpha` and made the CLI exit with status 1.

Instrumentation confirmed the error came from `SessionService::record`.
The unchanged pre-migration revision failed too. Temporarily opting out of
recording made the test pass.

`AttachOptions.record` now carries the recording policy into
`SessionService::attach`. New sessions receive the policy at creation;
existing sessions have recording enabled before attachment. The CLI performs
no recording request after the grant. False preserves the existing recording
state, matching the previous opt-out behavior.

The original attach/kill regression now passes. A service-level test also
checks that recordings contain the initial attach event, for both new and
existing sessions, and that opting out neither enables an unrecorded session
nor disables a recording already in progress.

The fix is commit `d4cd415`.

## Windows provider shutdown

Commit `ec166e1` (2026-07-06) added a direct Unix socket `shutdown` call to
the provider connection. Windows's named-pipe stream has no such method.

The provider now calls the existing `platform::ipc::shutdown_stream`
abstraction. Unix shuts down the socket; Windows cancels pending overlapped
I/O. The provider then joins its reader thread as before.

The provider tests now use the platform transport on both systems. A regression
test runs 20 shutdown iterations while the server deliberately keeps its side
open, asserting that shutdown wakes and joins the reader without waiting for
peer closure. Five provider tests pass on Windows and six on Unix; the
Unix-only test relies on socket read timeouts, which the pipe wrapper does not
implement.

Two existing Windows library-test fixtures also needed corrections: one
`SessionMetadata` initializer lacked `environment`, and the XDG test used
`/xdg/state`, which is not an absolute Windows path. These changes affect
test inputs only.

GitHub CI gains a Windows core job running:

```sh
cargo build -p cleat --locked --no-default-features
cargo test -p cleat --locked --no-default-features --lib
```

This gate compiles the application and exercises the provider/named-pipe tests
without requiring a Ghostty build.

## Validation

- macOS: exact repository format, Clippy, and workspace commands passed.
  The lifecycle suite now passes all 99 active tests. Rust-only build and
  tests passed.
- Linux (feta): exact format, Clippy, and workspace commands passed for the
  attach/provider changes. The subsequent portable XDG fixture change also
  passed the focused runtime tests.
- Windows (gouda): the unmodified-dependency Rust-only application build and
  all 154 library tests passed. All five provider tests passed with Ghostty
  enabled as well.
- With the diagnostic import-library correction described below, Windows
  Ghostty application/DLL linking passed, all 164 library tests passed, and
  all 42 VT integration tests passed. The known virtual-parent relative
  placement reproducer remains ignored.

The exact Unix checks were:

```sh
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

A broader Windows run still fails intermittently in the existing CLI test
`daemon_target_resolution_prefers_source_session_over_ambient`, during
session creation with pipe-ended error 109. Replacing its Unix `sleep`
command with Windows `ping` made an isolated run pass but did not fix the
full-suite failure; that experiment was reverted. The Windows CI gate
therefore covers core tests, not the full CLI integration suite.

## Additional Ghostty import-library defect

Removing the Rust compile error exposed a separate Windows DLL link failure.
The prepared Ghostty DLL has 203 exports: 202 `ghostty_*` symbols and
`_DllMainCRTStartup`. The generated import library includes that CRT startup
symbol. It can satisfy the consuming DLL's startup reference from Ghostty's
import library, preventing the consumer's normal CRT startup linkage.
Cleat then fails to resolve symbols such as `memcpy`, `free`, and
`_CxxThrowException`.

This is specific to DLL consumption: the Rust test executable links and runs,
and the Rust-only cleat DLL builds. Loading the Visual C++ developer
environment did not resolve the Ghostty-enabled DLL link failure.

A diagnostic prefix on gouda, `.tools/ghostty-import-validation`, contains
the original headers and DLL plus an import library rebuilt by MSVC `lib`
from a DEF file listing all 202 Ghostty API exports, excluding only
`_DllMainCRTStartup`. Selecting this prefix with `CLEAT_GHOSTTY_PREFIX`
makes the cleat DLL link and permits the feature tests above. No DLL code
was changed.

Ownership is Ghostty's Windows lib-vt export/import-library generation and
Zig's generated DLL startup. Zig 0.16.0's `lib/std/start.zig` explicitly
exports `_DllMainCRTStartup`. This is not the retained simdutf lazy-init
patch. A library-side correction should keep that implementation detail out
of the public import library; adding CRT flags to unrelated cleat code would
hide the dependency defect.

No diagnostic import-library rewrite was added to cleat's build or setup
scripts. The published dependency remains affected; the local Ghostty fix
and its validation are recorded below.

Logs and the exported-symbol listing are retained in
`.tools/migration-validation/` in the migration worktree. The isolated
Windows prefix and diagnostic scripts remain on gouda for the Ghostty
maintenance follow-up.


## Ghostty packaging fix

Ghostty commit `766af569c1317fa80b3ad7afcc79d76c88969fc0` on local branch
`patches/libvt-windows-import-startup` fixes the installed Windows import
library. The worktree is `/Users/robert/dev/ghostty-windows-import-fix`, based
on the published migration target `2a4777cd774be6bc59ab9353cab97faef8b215fc`.

`GhosttyLibVt.zig` now generates the import definition from the built DLL's
`ghostty_*` exports and invokes Zig's bundled `dlltool`. The DLL retains its
startup implementation. The installed import library exposes all 202 public
API symbols and excludes `_DllMainCRTStartup`. No external MSVC tool is needed
for this packaging step.

The new `test/windows/test_vt_dll_consumer.cmd` regression builds a C DLL and
loads it from a separate executable. It checks that the consumer's own
`DllMain` ran before creating and freeing a Ghostty terminal. With the published
library it reports `DLL consumer failed: 1`: the DLL loads but its own startup
is skipped. With the fixed package it passes. Ghostty's Windows CI build now
runs this regression as well.

Validation on gouda used the normal `zig-out` package from the fixed source,
with no manual rewriting of generated files:

- Native ReleaseSafe SIMD build passed twice.
- DLL consumer regression passed, including the exact CI command.
- Export/import comparison confirmed all 202 API symbols are retained and
  the startup symbol is absent from the import library.
- `cargo build -p cleat --locked --features ghostty-vt` passed, including
  cleat's DLL.
- `cargo test -p cleat --locked --features ghostty-vt --lib` passed all
  164 tests.
- `cargo test -p cleat --locked --features ghostty-vt --test vt` passed
  all 42 active tests; the existing virtual-parent reproducer remains ignored.

On macOS, the native Ghostty ReleaseSafe SIMD build, cleat feature build,
and all 42 active VT tests passed against this source. Zig formatting and
Git whitespace checks passed. The broader Windows CLI integration issue
noted above remains outside this packaging fix.

Nothing was pushed and no Ghostty integration refs were rewritten. Cleat's
tracked pin remains the published migration target. Publishing the Ghostty
fix and then updating cleat's pin are still required before the normal setup
helper can fetch this correction. Local validation selected the fixed install
with `CLEAT_GHOSTTY_PREFIX`.
