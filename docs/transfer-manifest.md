# Transfer manifest (Unix)

The synchronous `fd_transfer` primitive operates on an existing `UnixStream`
between any two hostings. Run it on a worker, never the daemon servicing loop.
It is adapted from `ceda0b99` (closed PR #166); sibling/daemon-specific behavior
was deliberately excluded. This slice has no callers or adoption protocol.

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
is in the receiver's window and its nonzero minimum is no greater than its version
or the receiver's version. Future versions are rejected even if their advertised
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
original recording. This slice defines emission but does not wire callers.

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
