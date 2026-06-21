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
daemon, or a remote target. A placement request may be exact, or may allow Cleat
to fall back to a different hosting when policy, capability, or availability
requires it at Ensure Session time. Both the requested placement and the actual hosting are
discoverable by clients.
_Avoid_: backend, provider mode.

**Id**:
A session's single durable identity. Client-assignable at create (cleat
generates a uuid only as a fallback) and **reused across recreations** — so it is
the cross-restart handle a client persists in its layout, and it keys the
session's recording directory. There is no separate ephemeral handle.
_Avoid_: name (a human-friendly display alias is a deferred nicety, not a second
identity).

**Embedded** (hosting):
The session's VT runs inside the client's own process (e.g. uishell). Lowest
latency; ends with the client unless the session is recovered from its recording.

**Daemon** (hosting):
The session's VT and PTY run in a separate cleat process; clients reach it over a
socket. Survives client restarts.

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
