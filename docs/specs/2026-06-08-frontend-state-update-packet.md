# Frontend-Relevant State Belongs in the Update Packet

Design principle for how cleat surfaces terminal state to a frontend (uishell, a
web UI, an agent harness): **every piece of state the frontend renders or makes
decisions from travels in the one update packet - never a side-channel query -
and any change to that state is what makes a packet due.**

## Context

This crystallised while wiring mouse input. uishell forwards mouse events to
cleat, which encodes them via libghostty's mouse encoder. The encoder gates each
event against the program's live mouse *tracking mode* (`none / x10 / normal /
button / any`) and format (SGR / SGR-pixels / ...), so correctness is already
handled server-side. But uishell would like to *know* the tracking mode too, in
order to:

- skip forwarding moves nothing wants (hover only matters in any-event mode), and
- choose between driving its own local text selection vs. forwarding the mouse to
  the program (with a modifier to override).

The question was how uishell should learn the mode: a per-frame query, or a field
in the render update. The answer generalises past mouse mode into a rule for all
frontend-relevant state.

## The principle

Model frontend-relevant state as **state carried in the update packet**, not as an
event and not as a query:

- It is *state*, not an event - a level the program sets that persists until
  changed. The frontend only ever needs "what is it now," at the moment it
  renders or handles input. An event would just be cached back into state anyway.
- It travels in the **same update packet** as cell/cursor deltas. One
  self-contained, serializable unit.
- **A change to any frontend-relevant state is a reason to emit a packet** - even
  when no visible cell changed.

### What counts as "frontend-relevant"

Anything the client renders *or* decides input handling from. Non-exhaustive:

- cells, cursor (position/style/visibility), scrollback extent, images +
  placements, selection
- title, working directory (OSC 7), palette / colors, bell, clipboard (OSC 52)
- **reported modes**: mouse tracking level + format, bracketed paste, focus
  reporting, alternate screen, application cursor keys, ...

The membership test is simply: *does the frontend read it?* If yes, its mutation
owes an update.

## Why packet, not query

A per-frame query is a **synchronous round-trip**. That is cheap across a thread
(the in-process actor), but:

- over an **ssh tunnel** it is a latency hit on every frame, and
- a **push-only web client** (SSE / WebSocket) often cannot synchronously call
  back at all.

The single update packet is the serializable unit that behaves identically
whether it crosses a thread, a socket, or a tunnel - the same invariant that lets
the same session drive a local compositor, a remote attach, and a web/agent
frontend. A side-channel query is precisely the thing that does *not* survive the
local -> remote -> web move. So folding state into the packet is the principled
choice, not merely the tidier one.

This is the same shape as the jackstay / "pipe any interactive surface anywhere"
direction: a stream of self-contained packets, transport-negotiated per endpoint.

## Enforce by construction, not by discipline

The trap with "a state change makes a packet due" is that the dirty-mark lives at
each mutation site. The day a new packet field is added and one of its write paths
forgets to mark dirty, you get **silent staleness** - exactly the failure where a
bare `\x1b[?1003h` mutates the mouse mode, touches no cells, produces no packet,
and a remote client never learns the mode changed.

So derive the "packet due" signal from a **snapshot diff** of frontend-relevant
state against what was last acknowledged, rather than scattered manual marks. Then
adding a field to the snapshot makes it participate automatically; there is no
mark to forget. cleat already has the bones for this (the generation /
`MarkObserved` model).

Practical split:

- **Scalar state** (modes, cursor, title, cwd, palette, ...) - cheap to snapshot and
  diff wholesale each tick. Do that.
- **The cell grid** - expensive to diff; keep libghostty's incremental row-level
  dirty tracking.

Both converge into one packet. The cheap stuff gets the invariant for free; the
grid stays efficient.

## Client cache is advisory, server gating is authoritative

The frontend's copy of this state is an **optimization / UX hint**, never the
source of truth. For mouse: the encoder re-reads the live mode via
`setopt_from_terminal` at encode time, so the real gating always happens
server-side against current state. The frontend knowing the mode only lets it
avoid wasted sends and pick local-selection-vs-forward.

This is what *permits* the eventually-consistent push model: a frame of staleness
in the client's cache is harmless, because nothing load-bearing depends on it.

## Near-term application (mouse mode)

When we wire the mouse-mode exposure:

1. Add mouse **tracking level** + **format** as fields in the render-update packet.
   Today `TerminalModeState` carries only `mouse_tracking: bool`; derive the level
   (`none / normal / button / any`) from the DEC mode queries cleat already makes
   (`ghostty_terminal_mode_get` for 1000 / 1002 / 1003, as it does for 1006 / 1016).
2. Mode transitions mark the session for an update (per the snapshot-diff rule).
3. uishell (or the web UI) caches them from the stream; uses them to gate hover
   and switch local-selection vs. forwarding. Encoder still re-gates.

## Implementation status

The first application of this rule is implemented for mouse state:

- `TerminalSnapshot` and `TerminalRenderUpdate` carry `TerminalModeState`.
- `TerminalModeState` includes mouse tracking level (`none / x10 / normal /
  button / any`) and report format (`legacy / sgr / sgr_pixels`), while keeping
  the coarse booleans used by existing input paths.
- The C provider ABI exposes the same state in `CleatTerminalModeState`.
- In-process observation tracks scalar terminal mode state; a scalar-only change
  advances the render generation and can produce a partial update with no row
  operations.

Future frontend-readable scalar fields should follow the same pattern: add them
to the packet state, include them in the observation snapshot diff, and avoid a
side-channel query.

**Future: pack the mode booleans into a `u64` flags word.** The reported modes
(`mouse_tracking`, `mouse_sgr`, alt-screen, app-cursor-keys, …, and the ones still
to come: bracketed paste, focus reporting) are currently one C `bool` field each.
That makes every new mode a struct-layout change — ABI bump, header edit, rebuild
everyone (the churn that added `terminal_modes`). A single `u64 mode_flags` makes
a new mode just a new *bit*: struct size/layout unchanged, no version bump, old
clients ignore bits they don't know. It also collapses "did the modes change?" to
one `u64` compare in the snapshot diff. The non-boolean enums
(`mouse_tracking_mode`, `mouse_report_format`) stay as small int fields alongside.
Worth doing before the mode set actually starts growing, since that's when the
bool-per-mode tax compounds.

## Non-goals

- Not proposing an event/notification channel for state changes - state rides the
  existing packet stream.
- Not reworking the cell-grid dirty tracking; only the scalar state moves to
  snapshot-diff.
