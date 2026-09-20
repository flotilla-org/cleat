# Structured keyboard input

Cleat encodes structured keyboard events inside the session actor, using
libghostty-vt's key encoder and the live application's terminal modes. Native
in-process providers and daemon packet clients share this path. Application
cursor mode, legacy modifier encoding and Kitty keyboard enhancement flags are
read from the terminal for each event.

CLI attach negotiates Kitty keyboard input with the outer terminal and feeds
the same structured event path. CLI and native mouse events also share
per-attachment button ownership and application-mode encoding. Existing raw-byte and text injection remain
explicit operations; they do not acquire held-key ownership or synthesize
release events.

## Event identity

`TerminalKeyEvent.key` is the logical, unshifted Unicode scalar or functional
key. Existing named keys remain available. `TerminalKey::Code` accepts extended
functional names from the pinned Ghostty/W3C code vocabulary, including
`ShiftLeft`, `ControlRight` and `NumpadEnter`. It is not a replacement for the
logical Unicode scalar of a printable key.

`physical_key` is an optional layout-independent W3C code such as `KeyW`.
Sources without physical information leave it absent. Physical names unknown
to the pinned encoder remain available for ownership matching but fall back to
logical-key encoding; unsupported logical functional names still return errors. `platform_keycode`
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

## CLI terminal input

Packet attach queries the outer terminal for Kitty keyboard support. After a
valid reply it requests all five enhancement flags (31), then reads back the
flags supported by that terminal. Terminals that do not reply retain the legacy
byte path. Cleat owns one keyboard-mode stack entry on the active screen: it
pops before changing screens, pushes on the new screen, and pops on detach.

The decoder preserves press/repeat/release actions, associated text, shifted
alternates and an optional physical identity from an explicit base-layout
alternate. Reports without key-up support become press/release taps. Releases
and repeats retain the identity of the original press when later reports omit
alternate fields. Hyper, Meta and functional keys outside the pinned Ghostty
encoder's vocabulary (including F26–F35) are discarded with a local hint.

Prefix commands and viewport panning consume their own key events. Entering pan
mode releases application keys held by that attachment; stale repeats and
releases are ignored. Bracketed paste shields its contents from keyboard report
and command interpretation. Reports may span reads; oversized reports are
bounded and drained without forwarding their tails as application input.

## Mouse input

CLI attach queries mode 1016 support and the outer terminal's cell dimensions
(`CSI 16 t`). It requests SGR pixels only after both replies, then confirms the
mode before decoding pixel reports. Other terminals retain SGR cell reports.
Cell dimensions are queried once a second to catch font and display changes.
Known Kitty/Ghostty outer terminals use zero-based pixels; other terminals use
xterm's one-based convention. Application pixel reports use Ghostty's zero-based
convention consistently for buttons, motion and wheels.
The coordinate format belongs to the attachment and stays independent of the
application's tracking mode. Detach disables pixel reporting.

Pixel positions retain their fraction within a cell through viewport panning.
Cleat reports the source cell dimensions to the daemon, which scales positions
to the application's cell dimensions. Cell reports use cell centres. Presses on
chrome or outside the visible grid are ignored; releases still reach the daemon
to end existing holds. Vertical and horizontal wheels and Back/Forward buttons
are supported. Malformed reports are discarded locally. The wire format follows
[xterm's SGR-pixel protocol](https://invisible-island.net/xterm/ctlseqs/ctlseqs.pdf).

Buttons follow the same attachment lifecycle as keys: disconnect, demotion,
takeover and history entry release that source's holds. Equal buttons held by
multiple drivers produce one application press and a final release when the last
owner lets go. Orphan releases and stale drags do not acquire ownership. CLI pan
mode releases its held buttons before consuming further input. Native in-process
clients use source zero and the same ownership rules.

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

A daemon/PTY contract runs the same two-driver keyboard and mouse scenarios with final disconnect,
history entry and exclusive takeover. It checks the exact application input
bytes and verifies that demoting one driver does not release the other's hold.

Validation commands on macOS:

```sh
cargo +nightly-2026-03-12 fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
./tools/prepare-ghostty-vt.sh
cargo build -p cleat --locked --features ghostty-vt
cargo test -p cleat --locked --features ghostty-vt ghostty_key
cargo test -p cleat --lib --locked --no-default-features keyboard
cargo test -p cleat --lib --locked --no-default-features attach_
cargo test -p cleat --lib --locked --features ghostty-vt packet_keyboard
cargo test -p cleat --lib --locked --no-default-features mouse
```

The full suite includes the Ghostty encoder contracts and daemon/PTY ownership
contract. CLI contracts cover fragmented reports, paste isolation, local commands
and panning. A Ghostty-backed outer terminal test checks keyboard-mode restoration
after repeated screen switches and detach; another checks the decoded input
against both legacy and Kitty application modes. No live Wheelhouse, Katzensteg
or outer-terminal GUI input validation is claimed.
