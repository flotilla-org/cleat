# Recording is on by default; persistence is best-effort and the format stays exportable

[ADR 0001](0001-session-hosting-and-recreation.md) made recreation-from-recording
the irreducible floor for persistence and left one question open: the
record-all-vs-opt-in policy. We resolve it here. **Recording is on by default.**
If recreation is the floor, then recording-off-by-default means most sessions have
no floor at all — persistence would be the opt-in surprise rather than the default
behaviour. Turning recording on by default is what makes a session actually
recoverable without the user having opted in ahead of time.

Persistence is **best-effort**, not lossless-or-bust. Losing a prior activation's
scrollback on recreation is a nice-to-have to avoid, not a system-breaking failure:
the session identity, the recreated command, and a working VT are what matter; old
scrollback above them is a convenience.

The recording format is **asciinema** today, and the invariant going forward is that
**asciinema export is always possible**. That decouples how recordings are stored
from what consumers can read, so the on-disk format can evolve without touching
anything downstream. The invariant is about *exportability*, not byte-for-byte
player compatibility of the raw on-disk file — see the appended-cast timestamp
caveat in the consequences.

## Considered options

- **Recording opt-in / off by default** (cleat's original principle). Rejected for
  recoverable sessions: it leaves the persistence floor unbuilt unless the user
  remembered to opt in, which defeats recreation-as-the-default.
- **Lossless persistence as a hard requirement** — guarantee exact reconstruction
  including all scrollback across recreations. Rejected as over-strong: a reboot
  already loses the live process (ADR 0001), and treating scrollback loss as a
  failure would force expensive VT-state machinery for marginal value.
- **Front-truncation of very long recordings now** — bound recording size by
  dropping old history at safe cut points. Deferred, not rejected. Because
  per-activation scrollback is droppable, the **activation boundary is itself a
  natural safe cut point** — "truncate everything before the current activation"
  needs no checkpoint machinery. The harder within-activation case (one session
  running for days without a restart) degrades to best-effort "cut at the last
  clear-screen, accept some loss" rather than exact VT-state reconstruction.
- **A custom/compressed recording format now** (e.g. a tiny terminal-trained LLM
  driving an arithmetic coder via logprobs — terminal output is highly predictable,
  so cross-entropy is low and compression could go well past gzip). Deferred: the
  asciinema-export invariant means this can be adopted later without downstream
  changes, so there's no reason to take the complexity on now.

## Consequences

- New sessions record by default. The default-on policy lives in the **CLI/client
  layer**, not the FFI: `cleat_session_desc.record` stays a plain required bool the
  caller always sets explicitly, so a provider embedder keeps its own default and
  the C API stays unopinionated.
- The CLI opt-out is a negatable, mutually-overriding `--record` / `--no-record`
  pair defaulting to on (effective value `!no_record`); the positive flag is kept so
  it can override an env-var opt-out. `CLEAT_RECORD` changes role from a
  presence-means-on flag to a boolish opt-out (`CLEAT_RECORD=0`/`false` disables),
  which is the behaviour-change to get right since the attribute looks unchanged.
- Toggling a *running* session is already supported (`Service::record(id, enable)`),
  so only the launch-time flag and env var need rework.
- Recreation may start a session with no prior scrollback and that is acceptable
  behaviour, not a bug to fix.
- Front-truncation is deferred work; when it lands, activation boundaries are the
  primary cut points and within-activation truncation is explicitly best-effort.
- The on-disk recording format is free to change as long as asciinema export
  remains lossless; downstream consumers should depend on the export, not the
  storage format.
- Recording is **fail-closed at session start**: a session created with recording
  on whose recorder cannot be opened (full disk, permissions, path collision)
  fails `spawn` rather than silently degrading to no recording. This is the
  deliberate consequence of recording-first — a session that was supposed to have
  a persistence floor must not start without one — but it is a user-visible
  behaviour change: a transient I/O problem that previously degraded silently now
  surfaces as a session-create failure. (Toggling recording on a *running* session
  is independent and stays best-effort.)
- Appended casts currently restart their timeline at the activation boundary: each
  recreation reopens the cast and emits new events from `t=0`, so a multi-activation
  file has **non-monotonic timestamps** and a naive player rewinds at each boundary.
  This is an MVP limitation, not a violation of the export invariant — a faithful
  asciinema export can re-base each activation's timeline. Making the raw on-disk
  file itself monotonic (carry the prior activation's last timestamp as the new
  base offset) is deferred follow-up work, naturally subsumed by front-truncation
  and the custom codec.
