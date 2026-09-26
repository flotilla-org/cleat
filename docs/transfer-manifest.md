# Transfer manifest (Unix)

The synchronous `fd_transfer` primitive operates on an existing `UnixStream`
between any two hostings. Run it on a worker, never the daemon servicing loop.
It is adapted from `ceda0b99` (closed PR #166); sibling/daemon-specific behavior
was deliberately excluded. Daemon-to-daemon transfer (`cleat transfer`, below)
is its first caller.

A transfer sends one marker byte with SCM_RIGHTS, a big-endian u32 JSON byte
length, then the manifest. Bounds are 16 descriptors and 1 MiB of JSON. Callers
set socket read/write timeouts. After any error discard the transport stream;
framing recovery is not supported.

The manifest carries `version`, `min_supported_version`, indexed `fds`,
`session` (id, tags, cwd, command, engine, environment, initial size/colors),
current `size`, `cell_pixel_size` (width/height, zero when unknown), `child_pid`,
`hosting_epoch`, and `replay_snapshot`. The snapshot has exactly the recording
`S` payload shape: `{engine, cols, rows, state}`, where state is the UTF-8 replay
payload generated with conservative client capabilities by the source VT engine,
as in `write_replay_snapshot`. It represents the current screen, not scrollback.
`hosting_epoch` is the epoch the adopter will hold: the source's current epoch
plus one, which the source writes to `<session directory>/epoch` when it commits.
Optional `markers` (name to cast offset) and `recording_paused` carry recording
state the adopter needs for marker-relative capture and `cleat record`.

| Role | Descriptor contract |
| --- | --- |
| `pty_master` | Live PTY master |
| `recording` | Recording file opened with O_APPEND |
| `pidfd` | Linux pidfd for child_pid, acquired before releasing the source |
| `child_status` | Read end of source exit-status forwarding stream |

Indexes are unique and contiguous from zero; roles are unique nonempty strings.
Unknown roles are preserved and owned just like known roles. Optional fields and
roles may be added without changing the version; new required semantics or
incompatible changes require a version bump. Transport validates the envelope
and index mapping; the adopting caller must validate required roles, descriptor
types, append mode, and snapshot/engine compatibility before adoption.

The current support window is `[1, 1]`. A manifest is accepted only if its version
is in the receiver's window AND its minimum satisfies `1 <= minimum <= version`. Future versions are rejected even if their advertised
minimum overlaps: there is no implicit downgrade. The version envelope is parsed
before the body, so even an unknown future schema gets an explicit NACK.
A validation ACK is byte 1. A NACK is byte 2, u32 JSON length, then
`{sender_version, receiver_version, reason}`. This is validation, **not** a session
commit. Version rejection does not touch the sender's descriptors or session.
Malformed framing/I/O errors terminate the operation; callers must discard the
stream rather than infer successful adoption from a timeout.

`send` borrows descriptors and waits for validation. It never closes originals,
kills processes, or deletes state. `receive` owns every installed descriptor
immediately, including those delivered in truncated ancillary data. All error
paths and dropping `ReceivedTransfer` close those duplicates. Darwin's receive
buffer accommodates the kernel maximum of 512 rights because XNU installs all
rights before truncating the control-data copy; iteration is also bounded by the
actual returned buffer length, not just the header's declared length. `commit()` explicitly
returns the owned descriptors to the caller; it has no wire effect. CLOEXEC is
applied per received descriptor (atomically on Linux); O_NONBLOCK and O_APPEND
are shared open-file-description flags and must not be toggled during receipt.
macOS transport sockets also enable SO_NOSIGPIPE so framing and response writes
cannot terminate an embedded host that retains the default signal disposition.
On platforms without atomic ancillary CLOEXEC, callers must serialize concurrent
fork/exec with receipt. No quiescence or session authority is implied by ACK.

`hosting_epoch::{read, increment}` uses `<session dir>/epoch`, matching #252:
positive decimal integer, missing means 1. Increment uses checked arithmetic and
atomic replacement; callers must already hold exclusive session authority and
serialize increments. A directory sync failure after rename may leave the new
epoch installed: reread before recovery, do not blindly retry. The recording
`transferred` event is an asciicast `m` marker whose string data is JSON:
`{"event":"transferred","epoch":2,"address":"..."}`. Standard readers can ignore
it; capture output slicing skips it while preserving surrounding output and the
original recording. This structured marker is distinct from a user label:
lookup by the plain name `transferred` will not match its JSON payload. Marker
indexing/discovery and a naming policy belong to the later actor integration.
This slice defines emission but does not wire callers.

`ChildObserver` registers before exit, then waits with a bounded timeout. Linux
uses pidfd_open/poll/waitid(WNOWAIT); `from_pidfd` accepts a transferred pidfd.
macOS uses kqueue NOTE_EXIT/NOTE_EXITSTATUS, falling back to NOTE_EXIT when status
access is denied. Observation does not reap. Non-child status is not generally
available: Linux waitid returns ECHILD; macOS restricts NOTE_EXITSTATUS. `None`
means exit observed without status; timeout is a distinct error. The source can
send a four-byte big-endian raw Unix wait status through `child_status` using
`forward_status`; EOF (including a partial frame) means unknown. Later actor
wiring must combine both sources and record status-unknown if neither supplies
status, never fabricate an exit code.

References: [Linux waitid](https://man7.org/linux/man-pages/man2/waitpid.2.html),
[macOS kqueue](https://keith.github.io/xcode-man-pages/kqueue.2.html).

## Daemon-to-daemon transfer (issue #254)

`cleat transfer ID --to NAME` asks the session's daemon to release it. The
source freezes the session (role changes, geometry, tags, recording control and
lifetime operations wait; PTY input and output keep flowing and are recorded),
then a worker thread runs the exchange below over a fresh connection to the
target daemon's socket. The servicing loop only polls the worker's channel and
checks the deadline on its normal tick; worker socket timeouts are a backstop.

1. **Probe.** `GET /` reports the target's `packet_protocol` range. The source
   applies the compatibility gate: every attached packet client negotiated the
   source's own protocol version, so a target that refuses that version would
   strand them, and stream attachments can never follow. Such clients are listed
   and the transfer refused unless `--drop-incompatible`.
2. **Transport.** `POST /transfer` with `Upgrade: cleat-transfer/1`, then
   `fd_transfer::send`. Roles sent: `pty_master` (a duplicate), `recording` (a
   fresh O_APPEND open of the cast), `pidfd` (Linux, when available), and
   `child_status` (one end of a socketpair). At the snapshot the source actor
   starts collecting the PTY output it reads afterwards (bounded at 16 MiB).
3. **Adoption.** The target validates the id (not live, no retained directory,
   daemon not draining), that the epoch exceeds any it has seen for the id,
   required roles and descriptor kinds (terminal device, O_APPEND regular file,
   socket) and engine compatibility; builds the runtime seeded from the snapshot
   without reading the PTY; and replies `READY` (byte 1, u32 0) or `REFUSED`
   (byte 2, u32 length, `Rejection` JSON).
4. **Commit.** On READY within the deadline the source stops reading the PTY,
   advances the epoch (an ambiguous post-rename sync error counts as committed;
   a definitive failure still aborts), writes the `transferred` marker with
   address `daemon:<name@generation>`, closes its recording descriptor, renames
   the session directory into the target's `sessions/` under the target name's
   layout lock (copying across runtime roots), and closes every packet channel
   with `MSG_CONTROL_REDIRECT` (the new address, epoch and protocol range)
   followed by the usual `ControlError`. It sends `COMMIT` (byte 1, u32 length,
   the output tail) and keeps answering HTTP requests for the id with 421 and the
   redirect for 30 seconds. The target replays the tail into its engine, starts
   reading the PTY, publishes a Directory delta, and replies `COMMITTED`.
   On REFUSED, a transport failure, or the deadline the source resumes and
   unfreezes and nothing is destroyed; the target never adopts without COMMIT,
   except that a target whose COMMIT frame is lost adopts without the tail when
   it finds the directory already moved at the new epoch.

The source keeps reaping the child and forwards its raw wait status on
`child_status` while it lives. The adopter observes the child through the pidfd
(Linux) or kqueue (macOS), waits briefly for the forwarded status once it sees
the exit, and otherwise records `{"event":"exit","status":"unknown"}` as a
marker instead of an exit event. Mutating HTTP requests may carry
`x-cleat-hosting-epoch`; a mismatch is refused as a stale holder.
