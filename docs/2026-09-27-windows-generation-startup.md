# Windows generation startup: closed readiness probe

Issue [#289](https://github.com/flotilla-org/cleat/issues/289) tracks a
ten-second named-pipe timeout during daemon startup. The listener correction
and deterministic regression already landed in
[PR #282](https://github.com/flotilla-org/cleat/pull/282), commit `bab953a`.
The controlled Windows comparison below reproduces the reported timeout with
the old listener and eliminates it with that correction. No startup budget
increase or additional production change is needed.

## Original runner evidence

All three reported occurrences predate the correction:

| Run / job | Date (UTC) | Generations start (UTC) | Failure (UTC) | Suite duration | Failing endpoint |
| --- | --- | --- | --- | --- | --- |
| [36247272710](https://github.com/flotilla-org/cleat/actions/runs/36247272710) (PR #268, `c1b46ca`) | 2026-09-26 | 14:07:19.1738779 | 14:07:29.2673257 | 10.09 s | `legacy/default/socket` |
| [36321382724 / 108625795156](https://github.com/flotilla-org/cleat/actions/runs/36321382724/job/108625795156) | 2026-09-27 | 13:44:06.9228472 | 13:44:17.1347786 | 10.21 s | `legacy/default/socket` |
| [36323655663 / 108632215952](https://github.com/flotilla-org/cleat/actions/runs/36323655663/job/108632215952) | 2026-09-27 | 13:50:42.8564681 | 13:50:52.9715766 | 10.12 s | `default@2/socket` |

The first two jobs fail `live_legacy_registration_keeps_its_directory_until_it_dies`;
the third fails `alias_reads_across_live_generations_and_creates_on_current`.
All report `timed out waiting for named pipe` and OS error 121. The legacy
endpoint and PR #268's September 26 timestamp correct the issue snapshot's
claims that every failure is `default@2` and all three occurred September 27.
The other daemon-launching tests finish within 0.89 s in the PR #281 job and
0.41 s in the PR #279 job. These are suite-relative completion times, not measured
daemon initialization times; the old logs do not contain startup-stage timings.
The source at all three failing heads (`c1b46ca`, `536b661`, `ad97bfc`)
contains the old `ERROR_NO_DATA | ERROR_PIPE_LISTING_ALIAS => return Ok(false)`
branch, without the disconnect correction.

## Root cause

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

The successful [Windows Ghostty VT job](https://github.com/flotilla-org/cleat/actions/runs/36328064092/job/108644602389)
at `f567751` ran the normal workspace suite, then this controlled comparison:

| Listener | Timing | Result | Generations suite duration |
| --- | --- | --- | --- |
| Old synchronous `ERROR_NO_DATA` behavior restored | 100 ms before first accept | Three launch tests fail with the reported OS error 121 | 10.08 s |
| Existing PR #282 fix | Identical 100 ms delay | All four tests pass | 0.89 s |
| Existing fix | Normal timing, 20 consecutive invocations | 80 tests pass, zero failures | 0.66–1.01 s per suite |

The delay was inserted after listener binding and pid/build registration, before
the daemon's first accept. Only the synchronous `ERROR_NO_DATA` branch changed
between the first two runs; the overlapping-connect recovery remained intact.
The negative control makes the race occur on first-generation startup too,
demonstrating that it does not require competing generations or a shared pipe
name. The historical logs lack internal listener traces, but the exact symptom,
the affected source at all three failing heads, and this controlled comparison
identify the closed-probe race rather than slow Ghostty initialization.

The twenty normal runs began at 15:05:18.8147329 UTC on September 27 and the
last suite passed at 15:07:18.2929008 UTC. Each invocation used:

```text
cargo test -p cleat --locked --features ghostty-vt --test generations -- --nocapture
```

Per-launch `Instant` measurements around `SessionService::create` recorded:

| Launch | Samples | Minimum | Maximum |
| --- | --- | --- | --- |
| `new` on explicitly selected `default@2` | 20 | 43.53 ms | 84.59 ms |
| `fresh` after advancing a dead current daemon | 20 | 52.59 ms | 195.14 ms |
| `legacy-session` on the legacy path | 20 | 43.59 ms | 91.14 ms |

These include daemon startup, pipe readiness, and session creation, so they
bound startup from above; they are not isolated DLL or VT initialization
timings. The generations tests request the passthrough engine even in the
Ghostty-linked job. No local Windows environment was available in the Linux
crew container; all Windows behavioral evidence comes from the linked runner.

## Reproduction and retained coverage

The temporary experiment is preserved in
[commit f567751](https://github.com/flotilla-org/cleat/blob/f567751759bdc678147d64899baf93822a97543b/tools/verify-windows-startup.py).
On a disposable Windows checkout of that commit, prepare the pinned Ghostty VT
and ConPTY dependencies with `tools/prepare-ghostty-vt.ps1`, then run
`python -X utf8 tools/verify-windows-startup.py` with the pinned Rust toolchain.
The source patches are restored in `finally`. Its complete successful evidence
step took about 157 seconds, within the existing 60-minute job budget.

The loop stops at the first unexpected failure; it never retries a failed test.
After each completed invocation, it terminates lingering test daemons restricted
to that checkout's exact `target/debug/cleat.exe` path. This releases loaded DLLs
before Cargo's next build-script invocation; it does not intervene within a
test or between the generations under test. Earlier experiment revisions
stopped on Python locale decoding, loaded-DLL restaging, or a direct-executable
missing-socket failure; none count toward the twenty-run result. The successful
run uses the Cargo invocation above throughout.

The temporary script and CI step have been removed from the final change.
PR #282's deterministic probe-before-accept regression remains in the normal
Windows core and Ghostty library suites, and the unchanged generations suite
remains in the Windows Ghostty workspace gate. The production pipe timeout
remains ten seconds; there is no longer startup budget to document or maintain.
