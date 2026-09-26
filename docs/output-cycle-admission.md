# Output cycle admission (v1)

Every live output subscription is admitted by the daemon before replay,
render capture, role changes, or geometry changes. This covers byte-stream
attach/watch, packet controller/watcher channels, the daemon provider/FFI,
the packet-debug CLI, and screen-activity subscriptions. Activity subscriptions
hold a lease for each selected session; a membership change that would close
a cycle stops that subscription with a control error before emitting activity
events. Directory metadata alone is not a terminal-output subscription. A channel rejected for a cycle does not exist, so
subsequent input/resize frames on that channel cannot affect the session.
Ordinary acyclic nesting needs no flag. Foreground clients show `nested in
<daemon>/<session>`; matching session names alone are not a cycle.

## Protocol and rollout

`POST /sessions/{id}/attach`, `/watch`, and `POST /connect` require the HTTP
header `x-cleat-output-context` containing one of:

```json
{"version":1,"context":{"kind":"external"}}
{"version":1,"context":{"kind":"session","source":{"runtime_root":"/absolute/root","daemon":"default","session":"shell"}}}
```

`source` is the containing session receiving the client's rendered output.
It is not the target being watched. `external` explicitly asserts that output
will not be fed into any cleat session. Missing, malformed, unsupported, or
unverifiable declarations receive HTTP 426 with an explanatory JSON error.
There is no metadata-free compatibility fallback. Cycles receive HTTP 409
for stream upgrades or a channel-scoped packet `ControlError`.

Successful upgrades return `x-cleat-output-admission: 1`. Updated clients
require it before relaying any output, and refuse old daemons that ignore
the new request header. The packet payload format is unchanged. Upgrade
both client libraries/embedders and **all participating local daemons**.
Stop old daemons/clients and restart or recreate their sessions before
claiming protection: existing connections in older processes are not
retrofitted by replacing the binary. Old-client/old-daemon pairs remain
unsafe. No automatic downgrade is provided.

## Source verification and trust boundary

The client obtains its containing session from the managed `CLEAT_RUNTIME_DIR`,
`CLEAT_OUTPUT_DAEMON`, and `CLEAT_SESSION` environment. The daemon exports
`CLEAT_OUTPUT_DAEMON` as its physical generation; `CLEAT_DAEMON` stays logical
for normal command targeting. Sessions missing the new physical coordinate
must be restarted before a nested client can subscribe. The daemon validates names,
requires an existing source session directory, and canonicalizes the physical
daemon directory. Runtime-root and daemon symlink aliases collapse to the same
identity, preserving physical generation suffixes. Equal names in separate
live generations remain distinct, including while the logical alias changes.

On Linux the daemon obtains the socket peer PID from `SO_PEERCRED` and reads
its initial environment through `/proc/<pid>/environ`. If that environment
identifies a containing session, the declaration must match it; claiming
`external` or another source fails. Failure to read the peer environment also
fails closed. On macOS and Windows source declarations are a protocol contract
with the upgraded client, without this additional kernel-peer corroboration.

This prevents accidental attachment feedback among conforming clients, not
malicious code running as the same user. Environment scrubbing before exec,
custom forwarding of a socket/output to another process or terminal, and
lying about an external sink violate the contract. The daemon cannot discover
arbitrary output forwarding from ancestry alone. Embedders must preserve the
source of their actual output sink, and reconnect if it changes; they must
not reuse an external connection to forward output into a session.

## Coordination, cleanup, and scope

The graph stores dependencies from containing source session to watched target.
Before inserting an edge, admission searches for a path from target back to
source. A per-user OS file lock serializes the check plus lease publication
across local daemons and runtime roots. Acquisition is nonblocking: contention
returns a `coordinator busy; retry output admission` error without stalling
the daemon event loop. A rejected activity membership stops that subscription
(including on contention), so the client must reconnect to retry. Each stream or packet channel owns a
separate locked lease, including duplicate subscriptions. Role changes keep
that lease; channel close, detach, connection failure, session exit, and failed
admission drop it. Daemon crashes release OS locks; the next admission removes
unlocked files, including interrupted writes. Disconnected clients are reaped
by the daemon's normal service loop, so a racing retry can briefly be rejected
until that loop observes EOF.

On Unix the coordinator is `/tmp/cleat-output-<uid>` (private permissions),
independent of runtime roots and environment-selected temporary directories.
On Windows it is `%LOCALAPPDATA%/cleat-output`. Participating daemons must share
that local filesystem/lock namespace. Do not remove the coordinator directory
while daemons are running. Containers, different users, network filesystems,
and cross-host relationships are outside the shared graph.

Remote output forwarding is unsupported: `{"version":1,"context":{"kind":"remote"}}`
is rejected. The supplied clients send that declaration when `SSH_CONNECTION`
or `SSH_CLIENT` is present, so SSH attach/watch and packet subscriptions fail
explicitly rather than assert safety across hosts. Custom remote transports
must also declare `remote`; they must not label a forwarded stream `external`.
A future distributed admission protocol is needed to support remote nesting.

Finite capture/transcript/replay operations are snapshots or bounded recording
slices, not live subscriptions, and do not retain graph edges. Arbitrary shell
loops that repeatedly feed those snapshots back into a session are outside
this attachment protocol. Recording limits remain a separate safeguard.
