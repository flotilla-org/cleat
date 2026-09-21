# Windows SSH attach row flicker

The operator's `flicker.mov` captures a 6.817-second idle Codex session on
kiwi/Ghostty, attached through Windows OpenSSH to Beaufort. Ghostty-vt is the
session engine. Color output is present; this is separate from the launcher
inheriting `NO_COLOR`.

Frame inspection shows a lighter horizontal strip on the otherwise black row
immediately above the animated composer. The strip ends at different columns
as the row is repainted. Of 310 captured frames, 51 have more than 10% non-black
pixels in that normally blank strip (crop x=40, y=623, width=1170, height=17;
red channel >15). Frames 78–86 give a compact visual example. The original
recording and extracted diagnostic artifacts remain outside the repository.

## Reproduction and change

`PacketTerminalRenderer` erased every dirty row with EL2 before repainting its
retained cells. That erase uses the current host background, which can differ
from the session's resolved cell backgrounds. When intermediate output becomes
visible, a row which should not change flashes to that background and is then
overwritten from left to right.

The regression `packet_render_preserves_unchanged_background_during_repaint`
feeds real renderer output through Ghostty-vt one byte at a time, with the
synchronized-update wrappers removed to model a console path that does not
preserve their presentation boundary. It checks an unchanged row at every
prefix, not just the final screen. It failed before the change and passes after.
This proves the renderer's transient corruption; it does not independently
prove Windows' synchronization behavior or replace an operator SSH retest.

The renderer now overwrites covered cells directly. It erases only the unused
right margin and rows outside the session grid. Wide characters clipped at
either viewport edge are explicitly replaced with styled blanks. Tests cover
those clipped edges and clearing old content when the session grid shrinks.

## Validation

- `cargo test --workspace --locked`: passed on Windows with Ghostty-vt enabled.
- `cargo +nightly-2026-03-12 fmt --check`: passed.
- `cargo test -p cleat --locked --no-default-features --lib`: 201 passed.
- `cargo build -p cleat --locked --features ghostty-vt`: passed; staged alongside
  its Ghostty DLL at `C:\dev\windows-parity-plan\cleat-render-bin`.
- Strict workspace Clippy remains blocked by existing Windows warnings in
  `host/actor.rs`, `session_runtime.rs`, and `platform/{ipc,pty}/windows.rs`.
- The operator reattached from kiwi using the staged client and reported that
  the previous row-flash pattern was no longer occurring. A slight cursor
  flicker remained, but was also observed in direct Windows Codex. It is not
  claimed fixed here. The hollow cursor became solid when Ghostty was focused.
  Cursor defaults and conhost versus Windows Terminal presentation remain
  separate questions. The agent and daemon were not restarted for this test.
