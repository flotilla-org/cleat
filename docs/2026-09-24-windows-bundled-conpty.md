# Bundled ConPTY on Windows: implementation evidence

Implements [ADR 0006](adr/0006-bundled-conpty-on-windows.md) for
[#234](https://github.com/flotilla-org/cleat/issues/234). Observed on Beaufort
(Windows 11 build 26200, Rust 1.98.1 MSVC) on 2026-09-24 unless marked as
source inference.

## Package behaviour

- The NuGet package's `conpty.dll` exports `ConptyCreatePseudoConsole`,
  `ConptyResizePseudoConsole` and `ConptyClosePseudoConsole` (`inc/conpty.h`),
  not the kernel32 names.
- It starts `OpenConsole.exe` from beside itself, then from an `<arch>\`
  subdirectory, and otherwise silently uses the system `conhost.exe`
  (source inference: microsoft/terminal `src/winconpty/winconpty.cpp` at
  `v1.24.11911.0`). Cleat therefore requires both files beside the executable.
- With flags 0, the bundled ConPTY's first output is
  `CSI 1 t`, `CSI c`, `CSI ? 1004 h`, `CSI ? 9001 h`. The inbox ConPTY sends
  `CSI ? 9001 h`, `CSI ? 1004 h` and a screen reset, and no DA1 query.
- Unanswered, the bundled ConPTY holds the program's console connection for
  3 s (`VtIo::StartIfNeeded` calls `WaitUntilDA1(3000)` in 1.24, source
  inference). Observed: a probe that never answers took 3.87 s per run against
  0.85 s inbox, each including 0.8 s of deliberate sleeps.

## Regression

`cargo test -p cleat --locked --lib conpty` runs a real ConPTY child (the test
binary re-invoked as an emitter) that writes Kitty APC and sixel DCS, and
asserts that both reach the session's VT engine verbatim and in order:

- Bundle staged beside the test executable: passes.
- `CLEAT_CONPTY=inbox`: fails; the engine receives `<BEGIN><AFTER-APC><AFTER-SIXEL><END>`
  with both sequences dropped.

`detached_conpty_session_starts_without_startup_query_delay` measures a detached
session to its first output: about 38 ms bundled and 37 ms inbox. With Cleat's
DA1 answer removed it failed at 3.04 s.

## Live check

A separately named daemon with its own runtime root ran a Python emitter that
enables VT processing and sends a 4x4 RGBA Kitty image (`a=T,c=4,r=2`):

| Daemon | `list` / `inspect` | `capture` | `packets` |
| --- | --- | --- | --- |
| default (bundled) | `conpty  bundled 1.24.260710001` | two cell rows reserved by the placement | `images=1/1` |
| `CLEAT_CONPTY=inbox` | `conpty=inbox (graphics degraded)`; `inspect`: `inbox, graphics pass-through degraded (forced by CLEAT_CONPTY=inbox)` | no reserved rows | `images=0/0` |

Detached sessions running `echo` produced output about 125 ms after `launch`,
including two CLI process startups.

`cleat packets` previously never answered the daemon's image file offer, so the
render that referenced an image waited forever. The simple packet client now
declines file offers, and the daemon sends the image bytes instead.

## Gates

- `cargo +nightly-2026-03-12 fmt --check`: passed.
- `cargo test --workspace --locked`: all passed (one existing ignored VT test),
  including `attach_existing`.
- Windows CI gate commands (`--no-default-features`: build, `--lib`,
  `windows_daemon_launch`, `attach_existing`): passed.
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: fails only on
  the pre-existing Windows diagnostics recorded in
  [2026-09-21-windows-pipe-eof.md](2026-09-21-windows-pipe-eof.md); the changed
  code adds none.
