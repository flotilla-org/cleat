# Pending Windows pipe reads must report EOF consistently

On Beaufort, a Codex agent was running in a functional Ghostty-backed Cleat
session, but `send-keys` returned pipe-ended error 109 after delivering input.
Local `attach` exited instead of attaching. Read-only session inspection remained
available.

The HTTP response reader uses `read_to_end`. The Windows blocking stream mapped
an immediate `ERROR_BROKEN_PIPE` to EOF, but passed the same error through when
`ReadFile` first returned `ERROR_IO_PENDING` and completion observed the closure.
The correction normalizes read errors after either completion path, preserving
the existing cancellation behavior. Write semantics are unchanged.

`pending_read_reports_eof_when_peer_closes` uses a real named pipe, consumes a
response, starts a further read while the server remains open, then closes the
server. It failed with error 109 before the fix and passed afterward. The corrected
client attached to the original, unmodified daemon and existing Codex process;
inspection reported controller identity `beaufort-local`.

Validation on Windows with Rust 1.98.1 and the normal pinned Ghostty setup:

- `cargo build -p cleat --locked`: passed.
- `cargo test --workspace --locked`: 430 passed, one ignored.
- `cargo +nightly-2026-03-12 fmt --check`: passed.
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: fails on
  pre-existing Windows diagnostics (unused imports/dead code, unnecessary mutable
  references, sorting and repeat iterator suggestions). No clean CI claim.

Built under a separate `CARGO_TARGET_DIR` and copied the client plus Ghostty DLL
to a separate runtime directory, preserving the loaded daemon binary.
No daemon restart, persisted-session migration, or pipe protocol change is needed
for the client-side correction.
