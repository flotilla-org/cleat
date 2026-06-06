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
one wake. Callers must use `cleat_session_dirty`, `cleat_session_poll`, and
snapshot generations to discover exact state; they must not count wake callbacks
as events.

## Pumping Model

### In-Process Provider

The in-process provider currently pumps PTY output inside `cleat_session_poll`.
`cleat_session_dirty` only reports the already-known dirty state; it does not
read from the PTY and it does not advance the VT engine.

Consequences:

- A UI embedding the in-process provider must call `cleat_session_poll`
  periodically or from an external wake/timer bridge until Cleat grows a
  provider-owned waitable output source or worker.
- Hidden/materialized sessions keep their process lifetime, but their terminal
  model does not necessarily advance unless the owner polls them.
- A wake from the in-process provider can occur synchronously during
  `cleat_session_poll` or another API call that marks state dirty.

This is acceptable as an intermediate implementation, but it is not the desired
long-term UIShell integration model.

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
- The owner thread later calls `cleat_session_poll` or the eventual equivalent to
  reconcile provider state, then snapshots and marks observed generations.

In that model UIShell does not continuously request frames just to make PTY output
advance. Continuous rendering is driven by dirty state and UI invalidation, not by
terminal IO plumbing.

## API Guidance For Embedders

Use this sequence on the session-owner thread:

1. On wake, post a UI/session-owner wake and return immediately.
2. On the owner thread, call `cleat_session_poll` for sessions that may need
   service.
3. If the returned or queried dirty state is not clean, call
   `cleat_session_snapshot`.
4. Render or cache the snapshot.
5. Call `cleat_session_mark_observed` with the snapshot generation after the UI
   has consumed it.
6. Call `cleat_session_release_snapshot`.

Do not call snapshot APIs from the wake callback. Do not rely on one wake per
PTY output chunk. Do not use visible-frame rendering as the only source of
terminal pumping once provider-owned progress exists.

## Follow-Ups

- Add a provider-owned worker or waitable output source for the in-process
  backend.
- Add a daemon wake/subscription bridge if daemon-backed UI embeddings need the
  same callback semantics.
- Decide whether the API needs an explicit thread-safe command queue for calls
  made from non-owner threads.
- Keep snapshot buffers owner-thread scoped unless a later API explicitly states
  otherwise.
