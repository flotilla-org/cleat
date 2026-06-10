# Session hosting is a per-session property; persistence is recreation-from-recording first

A [session](../../CONTEXT.md) is one hosting-agnostic thing with a single durable
identity; *where* its VT runs (embedded in a client's process, or in a separate
daemon) is a property of the session that can change over its life, not a global
mode of the client. Persistence is built on **recreation from recording** — on client restart, replay
the session's recording into a fresh VT and re-invoke the command. This is the
**irreducible floor**, not merely the easy first step: a reboot (or daemon death,
or OOM) kills *every* process, including daemon-hosted ones, so recording-based
recreation is the only recovery that always works. Live transfer of the running
process (FD handoff) is a later **optimization** for the narrower case where the
host survived — it never replaces recreation, so recreation comes first.

## Considered options

- **Two session types** (embedded sessions that die with the client vs. daemon
  sessions that persist), chosen at create and coexisting. Rejected: makes
  "embedded + daemon coexist" a type split rather than a hosting detail, and bakes
  a mode distinction into every client decision.
- **Live process transfer first** — promote an embedded session to a daemon via
  FD handoff (SCM_RIGHTS) so the running program survives a client restart.
  Deferred, not rejected: it's the lossless end-state for the host-survived case
  and quick to add later, but it only ever sits *on top of* recreation (it can't
  help across a reboot), so it isn't a prerequisite.
- **A separate ephemeral `id` plus a durable client `name`.** Collapsed: the only
  reason they differed was that cleat auto-allocated `id` per instance.
  `RuntimeLayout::create_session` already accepts a client-supplied `id` (uuid is
  just a fallback), so one identity, reused across recreations, suffices. A
  human-friendly display name remains a possible future alias, not a second
  identity.

## Consequences

- Recreation is **lossy**: the running process is gone; the recorded history
  becomes scrollback above a freshly-invoked command (which should run inside the
  user's interactive shell so exiting it is predictable — deferred detail).
- The provider C API must plumb the client-supplied `id` onto `cleat_session_desc`
  (today it hardcodes `None`), and cleat needs one new primitive: create a session
  seeded by replaying a recording.
- Persistence depends on recording being **on** for recoverable sessions, which
  tensions cleat's "recording is opt-in / off by default" principle. Resolved in
  [ADR 0002](0002-recording-on-by-default.md): recording is on by default.
