# Windows generation startup: closed readiness probe

Issue [#289](https://github.com/flotilla-org/cleat/issues/289) tracks a
ten-second named-pipe timeout during daemon startup. The listener correction
and deterministic regression already landed in
[PR #282](https://github.com/flotilla-org/cleat/pull/282), commit `bab953a`.
This investigation verifies whether that correction addresses the generations
suite failure rather than increasing a startup timeout.

## Original runner evidence

Both reported jobs predate the correction:

| Run / job | Generations start (UTC, 2026-09-27) | Failure (UTC) | Suite duration | Failing endpoint |
| --- | --- | --- | --- | --- |
| [36321382724 / 108625795156](https://github.com/flotilla-org/cleat/actions/runs/36321382724/job/108625795156) | 13:44:06.9228472 | 13:44:17.1347786 | 10.21 s | `legacy/default/socket` |
| [36323655663 / 108632215952](https://github.com/flotilla-org/cleat/actions/runs/36323655663/job/108632215952) | 13:50:42.8564681 | 13:50:52.9715766 | 10.12 s | `default@2/socket` |

The first job fails `live_legacy_registration_keeps_its_directory_until_it_dies`;
the second fails `alias_reads_across_live_generations_and_creates_on_current`.
Both report `timed out waiting for named pipe` and OS error 121. The legacy
endpoint corrects the issue snapshot's claim that every failure is `default@2`.
The other daemon-launching tests finish within 0.89 s in the first job and
0.41 s in the second. These are suite-relative completion times, not measured
daemon initialization times; the old logs do not contain startup-stage timings.

## Mechanism under test

`ensure_daemon_started` / `wait_for_socket` opens and immediately drops a
readiness connection. On Windows, that connection can close before the first
`SessionListener::accept` calls `ConnectNamedPipe`. The old listener treated
`ERROR_NO_DATA` as `WouldBlock`, leaving the closed connection attached to its
only pipe instance. Subsequent clients get `ERROR_PIPE_BUSY` and exhaust
`open_pipe`'s ten-second wait. A longer budget cannot release this instance.

The [Win32 contract](https://learn.microsoft.com/en-us/windows/win32/api/namedpipeapi/nf-namedpipeapi-connectnamedpipe#remarks)
requires disconnecting a previous client before reusing its pipe instance.
PR #282 calls `DisconnectNamedPipe` on `ERROR_NO_DATA` and resets the pending
connect state. Its regression,
`listener_recovers_when_probe_disconnects_before_first_accept`, deliberately
closes a probe before the first accept and checks that the next client receives
data.

## Windows validation

Runner experiments and the required twenty consecutive generations runs are
being collected on this PR. No local Windows environment is available in the
Linux crew container. The final report will link the runner evidence and record
the measured launch durations.
