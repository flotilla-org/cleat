# Multiplayer attachments

Packet attachments share one PTY and one Ghostty terminal. A CLI attachment
and daemon-backed library clients can drive the same session together.
Watchers receive renders and can browse history without writing application
input. Embedded library sessions remain in-process and are not attachable.

## Attachments and views

A packet channel is an **attachment**: one client's subscription to one session.
It holds one or more **views**, one per place the client shows the session
([ADR 0009](adr/0009-views-within-an-attachment.md)). `cleat attach`, `cleat
watch` and clients that did not opt in hold exactly one view, opened with the
channel, and see no difference.

| Per attachment | Per view |
| --- | --- |
| Live render stream and its ack credit | Role (controller or watcher), take control |
| Image transfer and local file offers | Window size vote and reconciliation |
| Clipboard effects and loss counts | Focus and cell pixel size |
| Program status opt-in | History position, history frames and their credit |

Live views share the attachment's one render stream: a second live view adds no
frames. The C provider keeps the attachment's live grid and gives each view its
own updates; a view that falls behind or returns from history catches up with one
coalesced update, and render generations never go backwards for a view. A view in
history receives its own frames, wrapped for that view. Typing into a controller
view returns that view, and only that view, to live. An attachment with no live
views receives no live frames until one returns.

Clipboard effects reach an attachment once while any of its views is a controller,
and never an attachment with only watcher views.

### Protocol

Views are protocol 16. A client opts in with `"views": true` in its `/connect`
body; only then does the daemon advertise 16. Other clients keep the hello and
messages they saw before.

- A channel opens with its default view, `0`, with the role in `OpenChannel`.
- `MSG_SESSION_OPEN_VIEW` (`OpenView { view, role, take }`) opens another view;
  the grant arrives as that view's `RoleState`. The client chooses view ids;
  `u32::MAX` is reserved.
- `MSG_SESSION_CLOSE_VIEW` (`CloseView { view }`) closes one. Closing the last view
  leaves the channel open and touches nothing else on the connection.
- `MSG_SESSION_VIEW` wraps a per-view message (`ViewFrame { view, msg_type,
  payload }`). Clients wrap input, resize, viewport, role requests and history
  acks; unwrapped ones address the default view. The daemon wraps history renders,
  view state and role state for every view, the default view included. Live
  renders and their acks stay unwrapped: they belong to the attachment.
- A size policy is accepted from any attachment with a controller view.

### C API

A `cleat_session` is the attachment and keeps its default view: every existing
`cleat_session_*` call behaves as before and acts on that view.
`cleat_session_open_view(session, &desc)` returns a `cleat_view` for daemon
sessions (null for in-process and mock ones). `cleat_view_*` mirrors the
per-view session calls: role, set role and take control; resize and geometry;
input, input batches and raw bytes; poll and dirty; scrollback extent, scrollbar
and scrolling; render updates, their release and virtual placements; image bytes
and backings; `cleat_view_state_json`; connection state and
`cleat_view_close_reason`. `cleat_view_destroy` closes the view.
`cleat_session_close_default_view` leaves the attachment to its other views; the
session's per-view calls then fail. Ending the session, fixed size, clipboard,
transfer and adoption stay on the session handle. Adoption and transfer refuse
while other views are open. Against a daemon older than protocol 16 an opened
view reports `CLEAT_SESSION_CLOSED` with a reason, and the default view works as
before. Daemon sessions never served snapshots, so views have no snapshot call;
a view's first render update carries the whole grid.

## Roles, geometry and input

Attaching normally requests shared driving. `cleat attach --take` requests
exclusive driving and demotes other drivers to watchers without disconnecting
them. New driving requests become watchers while exclusivity is held. Returning
the exclusive client to ordinary driving releases exclusivity; the other
watchers must request driving themselves. `--strict` fails if driving cannot be
granted. `send-keys` remains an out-of-band operation and works regardless of
attachment roles.

The PTY uses the minimum columns and minimum rows reported by controller views,
across all attachments; two controller views of one attachment both vote.
Watchers do not vote. With no drivers, the last applied size remains. An explicit
session resize sets a fixed size; the packet and C APIs can restore automatic
sizing. The earliest driving view (connection, then channel ID, then view ID)
supplies the application's cell pixel size; other views' mouse coordinates are
converted from their own cell size to those units. Application focus is the union
of controller views' focus, gated by the application's focus-reporting mode.
Taking control on a view demotes every other controller view, including the other
views of its attachment.

Text, keys and pastes from a driver view return that view to live before writing.
A paste is one actor operation. The application still owns one cursor; history
views hide it. Clients own local selection. Disconnecting an attachment leaves
the session running. Reconnecting library clients request their last granted
role without taking exclusivity. Offline input is rejected and never replayed.

Packet clients leave terminal query responses, including cursor-position reports,
to the session engine. Only a raw-stream controller forwards those queries to its
outer terminal. Query authority changes on takeover even when a driver remains
attached throughout.

The old raw-stream attach endpoint retains its exclusive compatibility policy.
Ghostty CLI attach/watch and daemon library sessions use packet attachments.

## CLI command mode

The default prefix is Ctrl-]. Set `CLEAT_COMMAND_PREFIX=^A`, for example, to
choose another control character. Doubling the prefix sends it literally;
Escape cancels command mode. Unknown commands are consumed and show a hint.
Bracketed paste bypasses command parsing, is buffered atomically up to 1 MiB,
and is discarded on overflow or if the client lacked driving permission during
its receipt.

| Following key | Action |
| --- | --- |
| `d` | Disconnect this attachment |
| `g` | Request shared driving, or release your exclusivity |
| `w` | Watch |
| `x` | Take exclusive driving |
| `c` | Toggle the status strip |
| `a` | Restore automatic controller-intersection sizing |
| `[` / `]` | Oldest history / live view |
| `k` / `j` | Move up / down ten history rows |
| Arrow | Enter pan mode and move one cell; further arrows pan until Escape |
| `r` | Reveal the application cursor in the live view |

A one-row text status strip appears when another attachment joins or the shared
grid exceeds the client window, and stays visible until hidden. It reports roles, participant counts, view state and size
policy. Its row is subtracted from the CLI's geometry vote. Library clients
provide their own chrome.

Panning moves only this attachment's window across the current shared grid.
It works for watchers and fixed-size drivers, including alternate-screen programs
and retained history. It leaves the PTY size, other attachments and history
positions unchanged. There is no automatic cursor following; prefix then `r`
moves the live view just far enough to reveal the application cursor. In history,
return to live with prefix then `]` before revealing it.

The strip puts clipping directions (`<`, `>`, `^`, `v`) and the zero-based origin
(`@column,row`) first, followed by the visible and shared grid dimensions. Right
and bottom borders occupy spare cells immediately outside the shared grid;
they reserve no extra rows or columns. Prefix then `c` hides the strip, borders
and command hints while leaving pan commands usable. Escape leaves pan mode;
ordinary typing and bracketed paste are consumed while that mode is active.
Prefixed commands remain available while panning, so you can toggle chrome,
change roles, reveal the cursor or detach without pressing Escape first. These
commands leave pan mode active until Escape.

Offsets clamp after geometry changes. Cursor visibility and mouse coordinates
use the same crop as rendering, and borders and visible chrome reject mouse
events, including releases. Hiding chrome returns its row to the local view;
for an automatically sized driver it also updates that driver's geometry vote.

## Captures and transport

The live path continues to use the shared incremental render reader. History
readers and anchors are created only when requested. Each history view has a
tracked primary-screen anchor, including while the application uses the
alternate screen. Terminal mutation invalidates the small shared capture cache;
identical ranges can share an owned base capture between mutations.

The host permits 128 anchored views per session, caches at most four base frames,
and allows at most two historical captures per servicing pass. A history channel
is limited to one capture per 34 ms. Capture failures retain the last frame,
report stale state, and retry after 250 ms. A capture is limited to 32,768 cells,
1 MiB of URI/image bytes and an 8 MiB retained-data estimate that includes vector
capacities and placement metadata. These are data budgets, not an allocator-wide
memory ceiling. Client output has a 4 MiB backlog limit; stalled clients cannot
grow the daemon's output queue indefinitely.

Each channel has one unacknowledged render and an independent delivery sequence.
History frames carry owned image bytes and hyperlink metadata. The daemon-backed
C provider retains those resources for the current render lease. History erasure
or terminal reset returns the affected view to live; eviction clamps it to the
oldest retained row with a notice. Browsing does not move the shared live viewport
or consume its dirty state.

Packet protocol version 7 adds participant presence, explicit exclusivity,
view status, history resources and size policy. Client and daemon versions must
match. The C interface adds `cleat_session_set_role`,
`cleat_session_set_fixed_size` and `cleat_session_attachment_state_json` without
changing existing C render structs. The JSON state includes presence, view state
and current-frame hyperlinks; image bytes use the existing image callback.

## Validation and remaining work

Behavior tests cover shared driving, exclusive demotion/release, geometry votes,
watcher input rejection, independent browsing, typing back to live, and a real
CLI attachment alongside library clients. Unit tests cover command parsing,
paste boundaries, focus aggregation, offline-input rejection and lazy shared
captures.

On macOS, this slice passes the repository's exact format, clippy and workspace
test commands, the explicit `ghostty-vt` build and tests, and a separate serial
`--no-default-features` workspace test run. The C header passes a compiler syntax
check. The existing ignored tests remain ignored.

The `attachment_latency` example measures input-to-render round trips. On macOS,
three alternating release runs against base `14ba6c5` measured median solo latency
of 25.62 ms before and 25.59 ms with multiplayer, with p95 around 30.0 ms in both.
Each run used 100 warmup iterations and 500 samples on a 120x40 terminal. This
measures the whole path, including the daemon's servicing tick; it does not
isolate CPU or allocation cost. `--history` adds two browsing watchers and a
continuous-output producer. One 500-sample run with those watchers measured
18.76 ms median and 34.25 ms p95. A transport fix waits for Unix socket writable
space instead of sleeping between partial writes; before that fix the same
workload measured 367.90 ms median and 497.39 ms p95. Continuous output changes
the servicing cadence, so this is not directly comparable to the idle solo
workload. `CLEAT_LATENCY_SAMPLES` overrides the sample count.
Build the matching release binary and example, then set `CARGO_BIN_EXE_cleat` to
the absolute path of that binary when running it.

Kitty graphics chrome, additional terminal chrome variants, richer local
selection UI and native Linux/Windows runtime validation remain separate work.
The CLI currently renders cells; carrying history graphics to library clients
does not add a CLI graphics renderer or repair the existing live daemon image
transport limitation. Embedded-session migration and multiple application
cursors are outside this slice.
