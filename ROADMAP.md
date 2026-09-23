# Cleat roadmap

Updated 2026-09-23. This file records the agreed order of work; GitHub issues hold
scope, acceptance criteria and implementation status. Update both when the order
or a decision changes. Priority labels are queue markers, not hard dependencies.

## Current queue

| Order | Work | Tracking and exit condition |
| --- | --- | --- |
| Delivery pending | Build/version visibility | [#113](https://github.com/flotilla-org/cleat/issues/113): implemented and tested in `a510f8c`, included with this roadmap update for PR delivery. Keep the issue open until merged. |
| 1 — next | Attachment-cycle safety | [#226](https://github.com/flotilla-org/cleat/issues/226): daemon/protocol admission rejects direct and indirect output cycles, including watch clients; define older-client enforcement and concurrent/cross-daemon behavior. Ordinary acyclic nesting remains allowed with a visible indicator. |
| 2 | Recording limits and safe cleanup | [#227](https://github.com/flotilla-org/cleat/issues/227): explicit per-session/root budgets and retention. [#228](https://github.com/flotilla-org/cleat/issues/228): status, pause/resume and safe discard without ending the program. [#150](https://github.com/flotilla-org/cleat/issues/150): termination escalation, reproduced during cleanup. These do not wait for image recording or semantic compaction. |
| 3 | Release profiling and measured repaint work | [#229](https://github.com/flotilla-org/cleat/issues/229): a reproducible direct-attach release baseline, with deliberate nesting tested separately. Use results to select [#197](https://github.com/flotilla-org/cleat/issues/197), [#213](https://github.com/flotilla-org/cleat/issues/213), [#161](https://github.com/flotilla-org/cleat/issues/161) or later image-backing work. Audit the graphics acceptance matrix in [#206](https://github.com/flotilla-org/cleat/issues/206) alongside this. |
| 4 | Long-lived daemon upgrades | [#191](https://github.com/flotilla-org/cleat/issues/191): new sessions use the upgraded protocol while existing sessions survive. Choose drain/compatibility/handover scope; full live transfer [#8](https://github.com/flotilla-org/cleat/issues/8) is not automatically required. This precedes durable image recording. |
| 5 — design first | Durable recording with Jackstay | Resolve continuity and external compressed video export in [#230](https://github.com/flotilla-org/cleat/issues/230), then implement [#73](https://github.com/flotilla-org/cleat/issues/73). Define replacement, placements/resize, reconnect, producer restart, recreation/handover and synchronization with terminal events. Do not start with an arbitrary temporary raw-frame storage format. |
| Later | Retention refinements and image transport | [#74](https://github.com/flotilla-org/cleat/issues/74): semantic compaction after reachability/fidelity rules are explicit. [#71](https://github.com/flotilla-org/cleat/issues/71): audit activation timeline semantics. [#102](https://github.com/flotilla-org/cleat/issues/102): encoded backing, deferred decode, producer-owned shm leases and eventual remote streaming. |

Design discussion can overlap earlier work. The order above does not turn every
pair of adjacent issues into a technical dependency. #73 has explicit issue
relationships for its upgrade and continuity-design gates.

## Decisions to preserve

- **Nesting is allowed; output cycles are not.** Keep the visible containing-session
  indicator. No mandatory `--nested` flag. Cover A → A and A → B → A at the authority
  boundary; a client-side environment check alone cannot protect older clients.
  Define what can be proven across daemons/remotes and what requires upgraded
  clients. Input/escape ergonomics remain [#3](https://github.com/flotilla-org/cleat/issues/3).
- **Recording policy need not be uniform.** Ordinary recovery history, deliberately
  retained archives, nested rendering and image-heavy sessions have different
  needs. Exact defaults and policy selection remain design work; do not silently
  disable recording based on an application heuristic. Recording stays on by
  default under [ADR 0002](docs/adr/0002-recording-on-by-default.md).
- **Limits are independent of compression.** Bound sustained growth even before
  video or semantic compaction exists. Declare any loss of history and preserve
  live programs when a recording reaches a limit. Source-history references for
  nested attaches are a design candidate, not an implemented retention guarantee.
- **Preserve efficient local image transfer.** [ADR 0005](docs/adr/0005-retained-image-delivery.md)
  retains immutable generations, prefers daemon-owned files and falls back to
  socket bytes. Avoiding decoding is distinct from avoiding copies. A future shm
  extension uses immutable producer-owned backing with producer deletion and an
  acquisition deadline/lease; it must support multiple viewers without receiver
  unlink races. No zero-extra-copy claim for the current ingress path.
- **Jackstay is a later shared design concern.** Efficient remote streams per useful
  placement geometry and external recorded video should share source identity and
  continuity vocabulary. Ingress/egress extensions need separate design. Revise
  [ADR 0003](docs/adr/0003-recording-multi-track-and-image-capture.md) with #230 before
  committing to storage/codec details.
- **Capabilities belong to the VT engine.** Ghostty is currently the functional
  implementation, but future engines should declare their own identity and
  behavior. [#175](https://github.com/flotilla-org/cleat/issues/175) is complete;
  transparent child-environment policy remains [#176](https://github.com/flotilla-org/cleat/issues/176).

## Evidence and delivery status

PR [#225](https://github.com/flotilla-org/cleat/pull/225) merged engine-owned terminal
identity, Katzensteg layering/flicker fixes, reduced image-related cell work,
session/nesting indicators and a direct self-attach client guard. Manual testing
confirmed image display, cessation of blank flashes and Monkey Island mouse input.
Debug-build throughput measurements were not displayed-FPS benchmarks and do not
establish the remaining bottleneck.

On 2026-09-22, older running clients inside `whatever3` and `whatever4` attached to
their own sessions. Both collapsed to a one-row grid and repeatedly repainted it.
The casts reached 42.36 GB and 38.47 GB; sampled output was about 95% cursor/style
sequences, with growth around 1.7–1.8 GB/hour per session. This was not primarily
image payload growth. Pausing held the files open; ending the sessions required
explicit SIGKILL after their interactive shells survived SIGTERM. Removing these
and older inactive recordings deleted 100.68 GB across 589 files. A local Time
Machine snapshot initially retained the blocks. These observations motivate
#226, #227, #228 and the existing #150.

Keep [#206](https://github.com/flotilla-org/cleat/issues/206) and
[#103](https://github.com/flotilla-org/cleat/issues/103) open until their complete
acceptance/status audit is explicit. Basic retained delivery is implemented;
encoded-source retention and durable recording are not. Historical mouse/replay
and chrome issues ([#189](https://github.com/flotilla-org/cleat/issues/189),
[#50](https://github.com/flotilla-org/cleat/issues/50),
[#181](https://github.com/flotilla-org/cleat/issues/181)) need their original cases
checked before closure; newer packet functionality alone is not proof that every
old acceptance condition is satisfied.

## Cross-project validation

The retained-delivery fixtures landed in
[kitty-image-tests PR #27](https://github.com/rjwittams/kitty-image-tests/pull/27)
on 2026-09-18; they are available, not a pending prerequisite. Reuse that suite
for protocol cases and Katzensteg (including Sonic/Monkey Island) for realistic
producer workloads. Do not duplicate the protocol fixture suite in Katzensteg.

The Wheelhouse dependency/consumer work was reported complete. Re-run the agreed
in-process, daemon-live and daemon-history matrix against the actual cleat build
used for #206/#229; completion of the earlier compatibility brief does not prove
all newer retained-delivery cases. Record the loaded library and daemon revisions,
not just the source checkout. The original coordination briefs are in
`~/dev/project-map/briefs/wheelhouse-cleat-image-readiness-2026-09-17.md` and
`~/dev/project-map/briefs/kitty-image-tests-cleat-delivery-2026-09-17.md`; their
statements that daemon image bytes are missing describe the pre-implementation
baseline, not current cleat.

## Other lanes

The queue above is the current focus, not a replacement for the rest of the
backlog. Windows work continues separately through
[PR #76](https://github.com/flotilla-org/cleat/pull/76),
[#205](https://github.com/flotilla-org/cleat/issues/205),
[#223](https://github.com/flotilla-org/cleat/issues/223) and
[#169](https://github.com/flotilla-org/cleat/issues/169).
Existing-only attach responsiveness is [#224](https://github.com/flotilla-org/cleat/issues/224);
remote/socket-only operation remains [#122](https://github.com/flotilla-org/cleat/issues/122),
[#125](https://github.com/flotilla-org/cleat/issues/125),
[#126](https://github.com/flotilla-org/cleat/issues/126),
[#127](https://github.com/flotilla-org/cleat/issues/127) and
[#128](https://github.com/flotilla-org/cleat/issues/128).
Session surfaces [#216](https://github.com/flotilla-org/cleat/issues/216) and the wider
render-consumer direction [#198](https://github.com/flotilla-org/cleat/issues/198)
remain design/backlog work, not prerequisites for the current queue.

## Tracker conventions

| Label | Meaning |
| --- | --- |
| `priority:next` | Next implementation focus; currently #226. |
| `priority:soon` | Near-term queue or pending delivery; use the ordered table above. |
| `priority:later` | Explicitly deferred work or design. |
| No priority label | Not yet scheduled in this queue; not a claim that it is unnecessary. |
| `ready` | Scope is a dispatchable implementation contract; independent of priority. |
| `from-review` | Provenance from PR review, not a priority or readiness signal. |
| `bug`, `enhancement`, `documentation`, `testing` | Type of work. |
| `cli`, `protocol`, `recording`, `vt-engine`, `infrastructure`, `agent` | Area of work. |
| `vision`, `dogfood` | Long-term direction or operational provenance. |

Use native GitHub blocked-by relationships only for actual gates. Keep ordering
in this file, scope/status on the issue, and historical rationale in ADRs. Close
issues when merged evidence satisfies their contract; a local commit or partial
manual test is not completion in the tracker.
