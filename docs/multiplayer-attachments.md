# Multiplayer attachments

Packet attachments share one PTY and one Ghostty terminal. A CLI attachment
and daemon-backed library clients can drive the same session together.
Watchers receive renders and can browse history without writing application
input. Embedded library sessions remain in-process and are not attachable.

## Roles, geometry and input

Attaching normally requests shared driving. `cleat attach --take` requests
exclusive driving and demotes other drivers to watchers without disconnecting
them. New driving requests become watchers while exclusivity is held. Returning
the exclusive client to ordinary driving releases exclusivity; the other
watchers must request driving themselves. `--strict` fails if driving cannot be
granted. `send-keys` remains an out-of-band operation and works regardless of
attachment roles.

The PTY uses the minimum columns and minimum rows reported by drivers.
Watchers do not vote. With no drivers, the last applied size remains. An explicit
session resize sets a fixed size; the packet and C APIs can restore automatic
sizing. The earliest driver connection, then lowest channel ID, supplies the
application's cell pixel size; other clients' mouse coordinates are converted to
those units. Application focus
is the union of driver focus, gated by the application's focus-reporting mode.

Text, keys and pastes from a driver return its own view to live before writing.
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
| `k` / `j` | Move up / down ten rows |

A one-row text status strip appears when another attachment joins and stays
visible until hidden. It reports roles, participant counts, view state and size
policy. Its row is subtracted from the CLI's geometry vote. Library clients
provide their own chrome. A smaller CLI watcher clips the shared grid at its
right and bottom edges and hides an offscreen application cursor.

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
