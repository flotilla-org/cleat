# Cleat

Cleat is a terminal session control plane: it owns terminal sessions (PTY, VT
state, and the running program) and exposes them to clients — a local UI shell,
remote attaches, agents — over a hosting-agnostic API.

## Language

### Sessions & hosting

**Session**:
A first-class, named, persistable terminal — its PTY, VT state, and the program
running in it. Identity and lifetime are independent of where it currently runs.
_Avoid_: terminal (the on-screen widget), tab, pane.

**Hosting**:
Where a session's VT engine currently runs. A property *of the session* that can
change over its life — not a global mode of the client or provider.
_Avoid_: mode, backend (the code-level term, not the domain concept).

**Placement**:
A client's requested hosting target or policy for a session, such as embedded,
daemon, or a remote target. A placement request may be exact or advisory. When
advisory, Cleat may fall back to a different hosting when policy, capability, or
availability requires it at Ensure Session time. Both the requested placement
and the actual hosting are discoverable by clients.
_Avoid_: backend, provider mode.

**Id**:
A session's single durable identity. Client-assignable at create (cleat
generates a uuid only as a fallback) and **reused across recreations** — so it is
the cross-restart handle a client persists in its layout, and it keys the
session's recording directory. There is no separate ephemeral handle.
_Avoid_: name (a human-friendly display alias is a deferred nicety, not a second
identity).

**Host** (hosting):
A process that owns the VT + PTY for one *or more* sessions, running each session's
runtime actor. The two hostings below are the same host shape differing only in
whether it carries the UI and a socket; a session can move between them by Transfer.
Per-session faults are contained inside a host (a panicking VT engine unwinds its
own session without killing siblings; the PTY fd survives in-process).

**Embedded** (hosting):
A host running inside the client's own process (e.g. uishell), which may hold many
sessions. Lowest latency; a session ends with the client unless recovered from its
recording.

**Daemon** (hosting):
A standalone host process — an embedded host minus the UI, reachable over a socket.
Owns one *or more* sessions and **outlives any single session** (lifetime is no
longer pinned to one child process). Survives client restarts.
_Avoid_: "one daemon per session" (the historical 1:1 model; a daemon now hosts a
set of sessions).

Daemons are **named** (default: `default`); the socket derives from the name. A
daemon auto-starts on first use of its name and, when it hosts no sessions, lingers
for a grace period before exiting (kills the empty-teardown race without leaving a
resident process forever). A daemon is an **isolation boundary** — analogous to a
k8s namespace: separate project checkouts, an agent loop's process backing, test
scratch space. It is *not* an organizational grouping mechanism.

**Tag**:
A flat label on a session, used to organize and select sessions *within* a daemon —
analogous to k8s labels inside a namespace. A session may carry many tags; clients
select subsets by tag. Tags are the answer to grouping; daemons are the answer to
isolation.
_Avoid_: hierarchical groups / paths (a hierarchy forces each session into exactly
one grouping; sessions legitimately belong to several).

**Directory**:
The live set of sessions a daemon hosts, with their metadata (tags, liveness,
control state, recreatability). Frontend-relevant state like any other: a
multi-pane client **subscribes** to it (optionally through a tag selector) and
receives the current set plus pushed changes; a one-shot listing is a single read
of the same state, never a separate polling mechanism. A session's address is
`(daemon, id)` — unqualified means the default daemon; tag selectors select *sets*,
never address a single session.

**Client connection shapes**: a multi-pane client (e.g. uishell) holds one
multiplexed connection to a daemon carrying the directory subscription and its
session streams; single-session tools (`cleat attach`, `cleat watch`, one-shot CLI
commands) keep their simple per-session surface. Both are first-class; the
single-session shape is not collapsed into the multiplexed one.

**Attach**:
A client resuming a session that already exists — the program is still running,
so it is lossless and live. Distinct from recreation.
_Avoid_: connect, open.

**Ensure Session**:
Making a durable session id usable for a client by attaching to the live session,
recreating it from recording, or creating it fresh. Attach is one possible
outcome; it is not the umbrella term for the whole operation.
_Avoid_: ensure attached, attach-or-create.

**Session Handle**:
A client's live reference to a session returned by Ensure Session. Multiple
clients may hold handles to the same session; the handle is not the session's
identity and does not determine where the session is hosted.
_Avoid_: session id, attach.

**Update Packet**:
The single self-contained, serializable unit by which a session surfaces its
current state to a client: cell/cursor deltas, scrollback extent, images and
placements, and every frontend-relevant scalar (modes, title, cwd, palette,
selection). It is the *one* attach transport — the same unit whether it crosses a
thread, a socket, or an ssh tunnel. There is no separate "raw byte-stream" attach;
a thin client that paints to a dumb terminal is just the simplest renderer of this
stream.
_Avoid_: frame, render frame (a packet is state, not a video frame), side-channel
query (frontend-relevant state rides the packet, never a query).

**Capability-complete packet**:
The Update Packet carries everything the VT engine knows at full fidelity. Each
client **downsamples** the packet to what its own renderer can express, so a weak
host terminal never limits what a richer client attached to the same session can
render. Client capability is therefore a one-way client-side concern, not a
negotiation.
_Avoid_: capability negotiation (it is downsampling, not negotiation).

**Capability policy** (what the child sees):
What the session advertises *to the PTY child* in answer to its capability queries
(DA/DSR/kitty/XTGETTCAP/…) is not fixed:
- **Default**: the engine answers with full (ghostty) capabilities.
- **Passthrough**: the child's queries pass through to the real attached terminal,
  so a recording faithfully reproduces what the program does in that emulator (e.g.
  xterm). This is the capability-policy sense of "passthrough" — distinct from the
  test-only placeholder VT engine of the same name. Passthrough binds to the
  **primary controller** (below); with no controller it falls back to the default.

**Controller**:
An attachment allowed to drive the session — its input reaches the PTY. A session
may have more than one; interference between concurrent controllers is the
controllers' own responsibility, not cleat's.

**Watcher**:
An attachment that consumes the Update Packet read-only. It renders the session
but its input does not reach the PTY. The basis for "you are not in control"
indication.

**Primary controller**:
The first-attached controller. The query target for Passthrough capability policy.
The primary is the *query target*, which is independent of which controller's input
is currently reaching the PTY.

**Grid geometry** (what size the child sees):
The PTY grid is one `(rows, cols)` the child lays out to. Who decides it:
- **Control-driven** (default): the grid is sized by control. With one controller
  the grid is that controller's size; with concurrent co-controllers (deferred) it
  is the **intersection** (min rows, min cols) of the controllers' sizes, so every
  driver can see and type into every cell.
- **Watchers never vote.** A watcher reconciles its own window against the grid
  client-side from the full grid in the Update Packet — letterbox/center if larger,
  pan a viewport or show a "too small" indicator if smaller.
- **Pinned** (optional): an explicit fixed geometry set at launch or by resize;
  overrides control, so the child never reflows just because someone attached.
  Intended for headless/agent sessions that want a deterministic layout.
Taking control resizes the grid to the new controller (unless pinned).
_Avoid_: "smallest client wins" across *all* attachments — a watcher must never
shrink the grid the controller is driving.

**Take control**:
A watcher promoting itself to controller. In the first cut a session has at most
one controller, so taking control is a **forced handoff**: the requester becomes
controller and the previous controller is **demoted to watcher** (not detached),
which it learns from its own control state. Concurrent co-controllers are a
deferred extension; until then, interference is impossible because control is
exclusive. Control state (own role, who holds control) rides the Update Packet
like any other frontend-relevant state — never a side-channel query.

**Recording**:
The append-only log of a session's output (asciicast v3), optionally with VT
snapshots for seek. The basis for recreation.

**Recreation**:
Rebuilding a gone session from its recording — replaying the recorded history as
scrollback, then re-invoking the command as a fresh process. **Lossy**: the
original running process is not preserved. Distinct from attach.
_Avoid_: restore, reattach (reattach implies the live process survived).

**Transfer** (a.k.a. handoff):
Moving a running session's PTY between hostings via FD transfer, preserving the
running process (e.g. promoting an embedded session to a daemon before the client
exits). Lossless. Deferred — not in the first persistence cut.
