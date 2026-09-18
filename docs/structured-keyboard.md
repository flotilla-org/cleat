# Structured keyboard input

Cleat encodes structured keyboard events inside the session actor, using
libghostty-vt's key encoder and the live application's terminal modes. Native
in-process providers and daemon packet clients share this path. Application
cursor mode, legacy modifier encoding and Kitty keyboard enhancement flags are
read from the terminal for each event.

This is the application-facing slice. CLI attach still reads legacy terminal
input; Kitty negotiation/decoding on the outer terminal and SGR pixel mouse
input are subsequent work. Mouse-button ownership cleanup is also part of that
mouse slice. Existing raw-byte and text injection remain explicit operations;
they do not acquire held-key ownership or synthesize release events.

## Event identity

`TerminalKeyEvent.key` is the logical, unshifted Unicode scalar or functional
key. Existing named keys remain available. `TerminalKey::Code` accepts extended
functional names from the pinned Ghostty/W3C code vocabulary, including
`ShiftLeft`, `ControlRight` and `NumpadEnter`. It is not a replacement for the
logical Unicode scalar of a printable key.

`physical_key` is an optional layout-independent W3C code such as `KeyW`.
Sources without physical information leave it absent. `platform_keycode`
remains diagnostic metadata; cleat never guesses a portable identity from it.
The internal Ghostty enum values do not cross the public API or packet boundary.

Generated text, consumed modifiers and press/repeat/release actions survive
transport. Generated text should be the text before Ctrl/Meta transformations;
C0 control text and macOS function-key private-use characters are excluded from
the Ghostty text field so the encoder can derive the sequence from the key.
Shift, Ctrl, Alt, Super, Caps Lock and Num Lock are represented. Physical-key
identity does not replace the logical character: logical `z` with physical
`KeyW` and generated text `Z` remains distinguishable. Remapped functional keys
use their logical identity for encoding (Caps Lock remapped to Escape sends
Escape). Ghostty's key API cannot encode an independent physical location for
every functional key; that metadata remains preserved in cleat's event model.
Use an explicit functional name such as `Numpad1` when keypad identity matters.

## Ownership

Each packet attachment owns its structured key holds. Physical identity matches
a release to its press when available; otherwise logical identity is used.
The logical identity from the press is retained if the layout changes mid-hold.
Across attachments, equal logical identities share a hold: the first press is
delivered and the last release ends it, using the first delivered press identity. Repeats from an attachment that has not
pressed the key are ignored. A source may hold at most 256 keys.

Disconnect, channel close, demotion, exclusive takeover, explicit session detach
and entry into history release the affected attachment's holds. Other drivers'
holds remain active. Returning from history or regaining control does not replay
held inputs; a fresh press is required. A stray release/repeat does not return a
history viewer to live view. In-process providers use a single source; destroying
that session terminates its actor and child.

The ownership layer is independent of the PTY and encoder. Synthetic releases
use the application's current modes: legacy applications do not receive Kitty
release sequences merely because cleat tracks a hold.

## Upgrade requirements

Both the C provider ABI and packet protocol are version **9**. Rebuild native
clients such as Wheelhouse against the matching header/library and use matching
client/daemon versions. Existing version checks reject incompatible peers.

The C input structure adds `physical_key` and `physical_key_len` beside
`platform_keycode`. Supply `CLEAT_KEY_CODE` with a functional W3C name in
`text/text_len`, or continue using `CLEAT_KEY_NAMED` / `CLEAT_KEY_UNICODE_SCALAR`.
Function codes `CLEAT_KEY_FUNCTION_BASE + 1` through `+25` are accepted.
Use `generated_text/generated_text_len` separately for produced text. All text
pointers are borrowed for the API call and copied before asynchronous transport.
Clients must send key-up events for keys they report as structured presses;
text-only sources should use text input instead.

## Verification

Encoder contracts exercise live legacy/Kitty mode changes, repeat and release,
application cursor mode, modifier keys and differing logical/physical identities.
FFI tests compare in-process actor events with daemon packet events. Ownership
contracts cover shared holds, layout changes and orphan releases/repeats.

A daemon/PTY contract runs the same two-driver scenario with final disconnect,
history entry and exclusive takeover. It checks the exact application input
bytes and verifies that demoting one driver does not release the other's hold.

Local validation passed on macOS:

```sh
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
./tools/prepare-ghostty-vt.sh
cargo build -p cleat --locked --features ghostty-vt
cargo test -p cleat --locked --features ghostty-vt ghostty_key
cargo test -p cleat --lib --locked --no-default-features keyboard
```

The full suite includes the Ghostty encoder contracts and daemon/PTY ownership
contract. No live Wheelhouse or Katzensteg GUI input validation is claimed.
