# Cleat Provider Threading And Progress Contract

## Context

Cleat's provider FFI is used by UI shells that own final rendering and input focus
while Cleat owns session lifetime, PTY IO, VT state, terminal input encoding, and
coherent snapshots. The UI must be able to keep hidden or unfocused terminal
sessions alive without coupling terminal progress to visible frame rendering.

This document records the current contract and the intended direction. Where the
current implementation is weaker than the target model, callers should treat that
as an explicit integration limitation rather than inventing renderer-side terminal
pumping.

## Thread Ownership

`cleat_session` handles are single-owner objects. Callers should invoke session
functions from one thread at a time, normally the UI/session-owner thread. The
current FFI does not promise concurrent calls into the same session.

`cleat_provider_set_wake_callback` installs one provider-wide callback. The
callback is not a rendering callback. It is only a nudge that provider state may
need service; the receiver should schedule work on its owning thread and return
quickly.

Wake callbacks may be synchronous with the Cleat call that caused the state
transition. Future provider implementations may invoke wake from provider-owned IO
threads. Callers must therefore make the callback thread-safe, non-blocking, and
independent of UI-thread-only state.

## Dirty State And Wake Edges

Dirty state is generation-based:

- Cleat increments a render generation when terminal-visible state changes.
- `cleat_session_snapshot` reports the generation in the snapshot.
- `cleat_session_mark_observed` marks a generation as rendered/observed.
- Snapshots do not clear dirty state by themselves.

Wake is edge-triggered for clean-to-dirty transitions from the caller's observed
point of view. Repeated output while a session is already dirty may coalesce into
one wake. Callers must use `cleat_session_dirty`, render updates or snapshots,
and generation observation to discover exact state; they must not count wake
callbacks as events.

## Pumping Model

### In-Process Provider

The in-process provider owns a per-session runtime actor. The actor owns
`SessionRuntime`, PTY IO, VT feed, dirty generation, and wake notification.
Provider APIs are synchronous commands/queries over that actor.

Consequences:

- A UI embedding the in-process provider does not need visible-frame polling to
  make terminal output progress.
- Hidden/materialized sessions keep their process lifetime and terminal model
  progress while the actor is running.
- A wake from the in-process provider may come from the provider-owned actor
  thread, so callbacks must remain thread-safe and non-blocking.

`cleat_session_poll` remains as a compatibility dirty-state query. It no longer
pumps PTY output on the caller thread.

### Daemon Provider

Daemon sessions are progressed by the daemon's session loop, not by the UI render
loop. The C-provider daemon backend currently observes daemon state through
request/response APIs, and this branch does not yet define a daemon-to-provider
wake subscription. Until that exists, daemon-backed embeddings should treat wake
as a local provider notification facility, not a complete cross-process event
stream.

## Target Model

The intended provider contract is:

- Terminal output progresses independently of visible frame rendering.
- Hidden and materialized sessions continue running.
- Provider-owned IO readiness or worker activity marks sessions dirty.
- The wake callback only schedules owner-thread service.
- The owner thread later queries dirty state, pulls a render update or snapshot,
  and marks observed generations.

In that model UIShell does not continuously request frames just to make PTY output
advance. Continuous rendering is driven by dirty state and UI invalidation, not by
terminal IO plumbing.

## API Guidance For Embedders

Use this sequence on the session-owner thread:

1. On wake, post a UI/session-owner wake and return immediately.
2. On the owner thread, call `cleat_session_dirty` for sessions that may need
   service.
3. If dirty state is not clean, call `cleat_session_render_update` for the
   operation-shaped render path, or `cleat_session_snapshot` for bootstrap,
   fallback, or simple clients.
4. Render or cache the update/snapshot.
5. Call `cleat_session_mark_observed` with the returned generation after the UI
   has consumed it.
6. Call the matching release function.

Do not call snapshot APIs from the wake callback. Do not rely on one wake per
PTY output chunk. Do not use visible-frame rendering as the only source of
terminal pumping once provider-owned progress exists.

## Render Updates And Scroll Damage

`cleat_session_render_update` returns versioned/sized update, operation, row,
cell, and style records. The in-process Ghostty path now asks the VT backend for
render updates directly instead of first materializing a full
`cleat_snapshot`. It emits:

- an initial/full-dirty `CLEAT_RENDER_OP_FULL_VISIBLE_REPLACE`
- `CLEAT_RENDER_OP_ROW_REPLACE` operations when Ghostty reports partial dirty
  rows
- full visible replacement when Ghostty only reports full dirty or when exact
  damage is unknown

Render rows carry row-level terminal metadata exposed by libghostty-vt, including
wrap/continuation, grapheme/styling/hyperlink presence, semantic prompt state,
Kitty virtual placeholder presence, and row dirty state. Render cells carry
graphemes plus resolved RGB colors and structured Ghostty style color tags for
foreground/background/underline color.

`CLEAT_RENDER_OP_SCROLL_COPY` is reserved in the ABI, but scrolling currently
falls back to full visible replacement until Ghostty/libghostty-vt exposes a
scroll/copy damage operation.

## Follow-Ups

- Add a daemon wake/subscription bridge if daemon-backed UI embeddings need the
  same callback semantics.
- Decide whether the API needs an explicit thread-safe command queue for calls
  made from non-owner threads.
- Keep snapshot buffers owner-thread scoped unless a later API explicitly states
  otherwise.
