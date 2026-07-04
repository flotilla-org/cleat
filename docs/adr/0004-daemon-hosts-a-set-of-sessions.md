# A daemon hosts a set of sessions; isolation comes from named daemons, not one process per session

Cleat began with an emphatic 1:1 model: **one daemon per session**, the daemon's
lifetime pinned to its one child process, and the per-session socket file doubling
as liveness indicator and discovery mechanism. This ADR retires that model.

A [daemon](../../CONTEXT.md) is now a **host for a set of sessions** — structurally
the same thing as the embedded uishell host (which already runs many sessions'
runtime actors in one process), minus the UI and plus a socket. Daemons are
**named** (default: `default`), auto-start on first use of their name, and when
empty linger for a grace period before exiting. A daemon is an **isolation
boundary** (a k8s-namespace analogue: separate project checkouts, an agent loop's
process backing, test scratch space); flat **tags** on sessions handle grouping
*within* a daemon. Session address is `(daemon, id)`, unqualified id meaning the
default daemon.

The pivotal realization: the codebase already trusts many-sessions-per-process —
embedded uishell runs exactly that. Keeping the daemon 1:1 wasn't buying safety;
it was a historical artifact of the daemon predating the shared host shape.

## Considered options

- **Keep 1:1 hosting, add a "hub" directory/router in front.** A client-facing
  process that enumerates per-session daemons and fans their streams in, preserving
  per-process crash isolation. Rejected: strictly more moving parts for a model we
  already run in-process, and the isolation it preserves is largely illusory — the
  VT engine is not assumed unstable, per-session faults can be contained with a
  `catch_unwind` boundary around each session's work (the PTY fd survives in the
  process, so a panicked session rebuilds rather than dies), and true process death
  is already covered by recreation-from-recording (ADR 0001) with FD transfer as the
  lossless upgrade path.
- **One resident daemon, always running.** No teardown race, warm forever.
  Rejected as the default: leaves an idle process behind and adds a step to "fully
  reset cleat." The grace-period linger captures the benefit (no race between
  last-session-exit and an incoming launch) without permanent residency.
- **Exit immediately when empty (tmux-server-like).** Honest process table, but
  "empty" is mushier for cleat than for tmux — a session with a recording remains
  recreatable after its child dies, serial agent workloads would bounce the daemon
  per session, and last-exit vs. incoming-launch is a genuine race. Rejected in
  favor of the linger.
- **Hierarchical session groups inside a daemon (attach-by-path).** Rejected: a
  hierarchy forces each session into exactly one grouping; sessions legitimately
  belong to several. Flat tags + selectors (the k8s labels move) compose; daemons
  cover the case that actually needs a hard boundary.

## Consequences

- **Liveness and discovery reroute.** Today socket-per-session = liveness =
  discovery via filesystem stat. With socket-per-daemon, session liveness and
  enumeration become daemon queries; `list`, stale-cleanup,
  `is_session_daemon_alive`, and kill fallbacks all move onto the daemon protocol.
  This is the largest mechanical chunk of the change, and it depends on the
  progress/servicing split (the daemon answering these queries must be the
  never-blocked servicing side — see the provider threading spec).
- **The directory is a subscription.** The daemon's session set + metadata is
  frontend-relevant state: multi-pane clients subscribe (optionally via tag
  selector) over one multiplexed connection that also carries their session
  streams; `cleat list` is a one-shot read of the same state. The single-session
  surfaces (`attach`, `watch`, one-shot CLI ops) remain first-class alongside —
  the CLI is not collapsed into the uishell model.
- **Blast radius is managed, not eliminated.** A daemon crash now takes down every
  session it hosts. Mitigations, in order: per-session `catch_unwind` containment,
  FD transfer for planned handoff/upgrade, recreation-from-recording as the
  always-works floor (ADR 0001). Users who want hard isolation put sessions in
  different named daemons — that is what daemon names are *for*.
- **Auto-start needs bind-is-the-lock.** Two concurrent commands naming the same
  daemon race to start it; first to bind the socket wins, the loser connects.
- **Docs debt:** the README's "One daemon per session" section and the runtime
  directory layout description are superseded and must be rewritten when this
  lands.
