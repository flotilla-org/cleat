# Live clipboard writes

Cleat pins Ghostty `c361de9691f006f65c400be73896d1e48a8ec56c` from
`rjwittams/ghostty` branch `patches/libvt-clipboard-write-completion`.
OSC 52 is parsed by that VT, never by a separate Cleat OSC parser. Its synchronous
callback validates and copies one UTF-8 `text/plain` (or
`text/plain;charset=utf-8`) representation, or an explicit clear with zero
representations. NUL-containing, invalid UTF-8, unsupported MIME/multiple
representations and oversized content are rejected before host exposure.
Zero-length representations are distinct from clear and are unsupported by this
OSC 52 relay; Ghostty reports empty OSC 52 requests as explicit clears.

The callback's borrowed request/content pointers and reply function never escape
its lifetime. `requires_completion=false` permits success for bounded queue
admission, not a claim that desktop I/O has completed. A true value (OSC 5522)
is answered synchronously as unsupported and is never enqueued. Missing metadata
is treated as unknown and the request is denied; names and grant flags are not
used to guess origin. No clipboard-read callback is registered. OSC 52 `?`
therefore never exposes host data or becomes a write.

## Bounds and loss

Each VT callback queue, actor delivery queue, and daemon-provider receipt queue
allows at most **16 events**, **64 KiB per text payload**, and **256 KiB total
text bytes**. Clear consumes an event slot but no payload bytes. An invalid
request or full queue drops the newest request; there is no parser blocking or
unbounded pending data. Each queue also stores bounded event headers and one loss
counter. Packet decoding is subject to the existing 4 MiB frame limit before
effect validation. The daemon's existing 4 MiB connection output backlog applies
to serialized clipboard packets too; overflowing it closes only the slow client
and discards its output. Terminal output continues. No retry or acknowledgement
is added for clipboard effects.

`cleat_session_clipboard_dropped` reports locally observed losses. For daemon
providers it combines the receipt-queue counter with actor losses carried in
`MSG_SESSION_CLIPBOARD_LOSS`. Invalid base64 discarded by Ghostty before the
callback is not counted by Cleat. Losses after a transport attempt (for example a
broken socket or a host crash) cannot be counted exactly without acknowledgements.
Counters are observations for a hosting/connection incarnation, not a global
exactly-once ledger. A new host may reset the upstream counter.

## Eligibility, ordering and identity

There is exactly one eligible packet recipient: the controlling attachment with
the lowest `(connection ID, channel ID)` among currently connected controllers.
These transport IDs are distinct even when attachment labels match. Shared
controllers other than this recipient receive no writes; watchers receive none.
A raw-stream controller disables the synthesized effect recipient. Embedded
hosting has one controlling provider handle and uses the same actor queue.

Each event identifies a fresh actor UUID (`session_epoch`), a monotonically
increasing recipient activation (`connection_epoch`), and an actor sequence
number. These are live identities, never persisted in render updates, captures,
recordings or transfer manifests. Events preserve parser order. A recipient
change clears pending actor events and advances its connection epoch. Demotion,
disconnection and reconnection clear pending provider receipts. Disconnect
followed by attach is a fresh subscription, without old effects. Hosting transfer
suspends delivery, drops pending effects and discards effects parsed from the
adoption tail; the new actor has a fresh UUID. An aborted transfer resumes
future delivery without restoring discarded events.

Delivery is an **at-most-once attempt** to the eligible recipient at the point of
transport enqueue. Bytes already attempted on a socket cannot be revoked during
a later takeover; they are never reassigned or replayed to a new controller.
Repaint, full frames, resize and history capture do not carry effects. Live writes
continue to the controller even while it views history. Effects wake consumers
when cells are clean or synchronized-output presentation is held, and do not
consume render credit or depend on render acknowledgements.

## Packet and native consumers

Packet protocol **12** requires matching protocol peers via the existing hello
range check; protocol 11 peers are rejected before session frames. Role state
reports `clipboard_writes` for the actual session VT. `MSG_SESSION_CLIPBOARD` is
a distinct live-event frame, separate from `MSG_SESSION_RENDER`.

Provider ABI **11** rejects older requested ABI versions. Use
`cleat_session_clipboard_supported` to check the actual session capability; mock
and passthrough sessions return false, and a daemon handle reports false until
its role/capability metadata arrives. The Rust-only client can receive effects
from a functional daemon; it cannot produce effects from its own passthrough VT.

After a wake, repeatedly call `cleat_session_acquire_clipboard_event` until it
returns NULL. Each acquisition removes one event and owns its UTF-8 bytes.
`kind=1` is text, `kind=2` is clear. Destinations are standard=0, selection=1,
primary=2. The pointer remains valid after the session/provider is destroyed or
changes hosting. Release each non-NULL acquisition exactly once through
`cleat_clipboard_event_release`; its opaque owner field must not be modified.
Native hosts apply or discard events according to their clipboard policy.

The attach CLI maps destinations to `c`, `s`, `p`, emits UTF-8 as base64, and
terminates with ST. Explicit clear emits an empty OSC 52 payload, matching
Ghostty's semantics. Screen rendering, chrome painting and clipboard output
share the output lock, so synthesized escape sequences cannot interleave.

Recorded CLI replay switches to real-VT reconstructed rendering once an escape
sequence appears, and discards transient effects and query replies. Plain-text
replay retains its byte output. The cast header supplies initial geometry.
A Rust-only build refuses replay containing terminal control sequences because
it lacks the parser needed to suppress effects safely.

## Validation and desktop acceptance

`cargo test -p cleat --locked --features ghostty-vt --lib clipboard` exercises
BEL/ST, every two-chunk boundary, one-byte output parsing, Unicode/destinations,
clear, query separation, invalid/binary/oversized input, bounded queues,
terminal progress and actual packet -> attach serialization -> enclosing VT.
`cargo test -p cleat --locked --features ghostty-vt --test clipboard --test replay`
uses real ephemeral daemons, PTYs, providers and attach consumers for ownership,
effect-only wakes, withheld render credit, takeover/watchers, late subscribers,
disconnect, slow consumers, views/resize/snapshots, hosting transfer, recorded
replay and nested local attachment. The C caller fixture is
`crates/cleat/tests/fixtures/clipboard_abi.c`; run it with
`./tools/test-clipboard-abi.sh` against a fresh temporary runtime. On shared-library
builds, explicitly select the pinned Ghostty library path so an ambient installed
library cannot shadow it.

Human acceptance uses an isolated runtime and the PR build, not production
session daemons or Wheelhouse's daily driver:

1. In desktop Ghostty, attach to an isolated test session. Have its child print
   `printf '\033]52;c;aGVsbG8=\007'`. Paste into a separate application and verify
   `hello`. Repeat with ST, Unicode and empty clear. A `?` query must expose no
   clipboard data. Verify clipboard policy permits writes in this test terminal.
2. Repeat the same fixture through the actual SSH/Flotilla attachment route,
   recording endpoints/builds and the pasted result. Do not replace this check
   with a fake SSH transport.
3. Exercise the fullscreen TUI copy action in the isolated route and record the
   result. Wheelhouse desktop consumption belongs to Wheelhouse #71 and needs
   its later consumer pin; the nested-Cleat/native fixture verifies this shared
   contract independently.

Test commands launched from inside a Cleat session should remove only the
ambient `CLEAT_SESSION`, `CLEAT_RUNTIME_DIR`, `CLEAT_DAEMON`, and
`CLEAT_OUTPUT_DAEMON` variables from the test subprocess environment. Existing
external-client handshake fixtures must not inherit the crew's containing session
identity. Nested attachment fixtures establish their own real source coordinates.

The Linux automated fixtures do not claim that these desktop/SSH/TUI checks ran.
