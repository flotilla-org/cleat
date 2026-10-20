# Controller input and daemon submission

`cleat send SESSION --submit --controller-idle 500ms 'message'` optionally
refuses delivery unless accepted controller input has been quiet for the
requested duration. Plain seconds (including fractions) and humantime durations
are accepted, rounded up to milliseconds. With no controller history, the
quiet predicate is satisfied. A refusal returns CLI error status 1, HTTP 409,
and writes neither text nor a requested `--mark-before` marker. This is an
immediate predicate, not a waiting command.

`inspect --json` exposes top-level `controller_input_generation` and
`last_controller_input_at` (Unix milliseconds, initially null). Generation starts
at zero for a new child activation, advances on accepted nonempty raw/text/paste
input and keyboard presses/repeats, and survives live host transfers. Queued
controller input counts at admission. Key release, resize, focus, mouse, watchers
and HTTP automation do not advance it. Acceptance is a daemon event, not the time
a byte first reached the client's socket. These fields are independent of PTY
output-idle and screen activity.

Raw attachments carry both typing and terminal responses on their input channel.
A terminal response therefore can produce a controller-activity false positive.
A controller role is not proof of a human operator.

## Write transaction

The actor pumps pending PTY output, fences unobserved controller input, evaluates
`SendPreconditions`, and only then records the optional marker and writes.
Selector guards use this same precondition step (see below).

For `--submit`, the actor encodes paste using the active engine, then holds
transaction ownership for the existing 100ms delay and writes Enter. The HTTP
response is deferred until Enter and replay finish. The daemon keeps servicing
attachments and the actor keeps pumping output during that delay. Concurrent
submission and automation writes receive a busy refusal. Controller raw, text,
paste and key events are queued in admission order and replayed after Enter,
without normalizing bytes. Key-source cleanup and focus changes use the same
queue. The queue admits at most 256 events and 64KiB of payload/variable key
metadata (plus bounded event overhead). An event that would exceed either bound
is explicitly refused, does not advance generation, and gets a raw `Frame::Error`
or packet `ControlError`. Previously admitted events still replay. Mouse/wheel
PTY input is explicitly refused while submission owns the actor. A PTY write or
session-exit failure is reported as an error; successful submission requires
Enter and all admitted replay writes to succeed. An error can occur after paste
and Enter were delivered (for example, during controller replay), so it does not
prove that submission was absent: callers must reconcile delivery before retrying
to avoid double submission. Even if Enter fails, the actor attempts every admitted
replay event to preserve controller input; the resulting draft state is uncertain.

Without the new guard, existing sends retain their byte behavior. `--submit`
uses one daemon transaction instead of CLI paste/sleep/Enter requests. Ordinary
unguarded send keeps its existing keys request. Quiet-time guards and submission work with the no-VT build; selector guards
require a functional VT engine. Paste uses the active engine's byte encoder.

## Conservative echo fence and limits

For guarded sends, controller writes remain fenced until their expected text
appears in subsequent PTY output, followed by a 100ms settling interval. Echo
matching handles fragmented output, preserves matching progress across live
host transfers, and ignores unrelated output. Printable key events use their
generated text or unmodified Unicode scalar; an encoded key with no known text
has unverifiable echo. Pending echo evidence is bounded to 64KiB. Unknown,
transformed, masked, non-echoed or oversized input remains fenced rather than
being cleared by an arbitrary timeout. Further matching input cannot clear an
already unverifiable fence. This may prevent guarded sends for the remainder of
a child activation; unguarded sends remain available. Raw terminal responses
can cause this conservative false refusal too.

Echo matching is observational evidence, not an application acknowledgement:
coincidentally identical output can match, and application redraw/encoding may
prevent a match. Quiet time does not establish an empty editor and cannot detect
an abandoned draft. The transaction excludes interleaving cleat-mediated input;
it cannot control external PTY writers or an application's private buffer.

Flotilla's separate adoption for flotilla#2614 would call
`cleat send SESSION --submit --controller-idle DURATION TEXT` (or the equivalent
HTTP `input` request below), and handle refusal/retry according to its own policy.
No flotilla classifier or consumer behavior changes here.

```json
{"kind":"send","text":"message","submit":true,"no_enter":false,"controller_idle_ms":500,"marker_name":null}
```

POST this to `/sessions/SESSION/input`. Successful HTTP 200 returns
`{"marker_offset":null}` or a recording offset when a marker was requested.
`submit` and `no_enter` together are HTTP 400. Existing input variants and marker
endpoints retain their response shapes. Legacy automation endpoints also refuse
writes during a submission; their existing error handling reports that refusal.

## Atomic selector guard

```sh
cleat send crew-1 --submit --if "composer[value='']" --controller-idle 3s 'Review wake: ...'
```

`--if SELECTOR` is an immediate predicate evaluated by the daemon in the same
actor transaction as the write. It uses the screen-query selector grammar,
including unions and nested predicates. The daemon first validates syntax and
source capabilities, then pumps pending output, requires the controller echo
fence to be clear, evaluates the selector on the current structural tree with
recognizer roles, and only then records the optional marker and writes. Ownership
continues through delayed Enter and controller replay. A separate CLI query
followed by a send cannot provide this guarantee. `--wait-if` is not implemented.

| Outcome | CLI exit | HTTP | JSON `reason` |
| --- | --- | --- | --- |
| Sent | 0 | 200 | — |
| Wrong value or no structural match | 1 | 409 | `not-matched` |
| Supported role absent from this frame | 1 | 409 | `role-absent` |
| Controller fence, quiet-time check, or submission busy | 1 | 409 | `controller-active` |
| No VT, or role without an active source | 2 | 422 | `unsupported` |
| Malformed selector | 2 | 400 | `error` |
| Recognition, transport, PTY or session error | 2 | 500 | `error` |

Failures return `{"reason":"not-matched","error":"selector did not match the current frame"}`
with the appropriate reason and diagnostic. CLI refusals and unsupported
diagnostics include the reason; generic errors retain their plain diagnostic. The exit-code table applies to
`--if`; CLI transactions without it retain their existing error handling.
Refusals and pre-write errors produce neither text nor a `--mark-before` marker.
A later PTY/Enter/replay error can follow a write, so reconcile delivery before
retrying such errors as described above.

Role support is determined from the parsed selector (also inside `:not`, `:has`
and `:nth-child`) and the selected recognizer's declared output roles, before
reading screen state. Structural elements and region kinds need no recognizer.
Unknown roles, a missing recognizer for `composer`, and `prompt` without an OSC
133 source are unsupported. OSC 133 prompt roles are not yet provided. A running
recognizer that currently produces no composer instead returns `role-absent`.
For a selector that does not match, `role-absent` means at least one referenced
role is absent anywhere in the frame. With unions, nested negation, or mixed
roles, this is a diagnostic hint rather than proof that the absent role alone
caused the nonmatch; both `role-absent` and `not-matched` are retryable refusals.
Recognition errors fail closed. Omitting `--if` preserves existing input behavior;
`--controller-idle` alone still works without VT.

Composer checks preserve literal spaces, multiline content, placeholder-prefixed
drafts and visible suffixes after a cursor moved left. They detect abandoned
drafts even after a long quiet period. Both bundled recognizers have one known
observation limit: spaces after the cursor are indistinguishable from terminal
padding. A draft consisting only of spaces with the cursor moved to its start
can read as empty. This limitation does not permit removal of draft content.

```json
{"kind":"guarded_send","text":"message","submit":true,"no_enter":false,"if":"composer[value='']","controller_idle_ms":3000,"marker_name":null}
```

Selector-guarded clients use the distinct `guarded_send` request kind. Older
daemons reject that unknown kind before writing; adding an `if` field to their
ordinary `send` request would otherwise be silently ignored. Upgrade/restart an
older daemon before using guarded sends. Unguarded transactions retain `send`.

## Controller fence diagnostics

`inspect --json` and `GET /sessions/{id}` include `controller_fence`:

```json
{"state":"settling","since":1760918400000,"settles_at":1760918400100}
```

`state` is `clear` (no unobserved input or settling interval), `pending`
(written input is awaiting its expected PTY echo), `unverifiable` (echo cannot
be verified), or `settling` (echo matched, within the 100 ms settling interval).
`since` is Unix milliseconds when that state began. For `clear` after echo, it
is derived from the echo-match timestamp plus the 100 ms settling interval,
even if no inspect request occurred at that instant. It is null for an initial
`clear` fence. `settles_at` appears only while settling. An unverifiable fence
instead includes `cause`: `no-known-text` for input without known echo text,
or `oversized` when pending echo evidence exceeds 64 KiB. These diagnostics
survive live host transfer and reset to initial `clear` on a new child activation.
The existing controller generation and last-input timestamp remain unchanged.

A `controller-active` refusal includes an additive `controller` object:

```json
{"reason":"controller-active","controller":{"cause":"fence-unverifiable"},"error":"controller input has not yet produced a verifiable PTY echo"}
```

The causes are `fence-pending`, `fence-unverifiable`, `fence-settling`,
`quiet-time` (the requested quiet interval has not elapsed), and `busy`
(another submission owns the PTY). CLI diagnostics include `controller.cause`.
The reason, HTTP 409 status and CLI exit 1 are unchanged. Callers should not
keep retrying `fence-unverifiable`: it stays closed until the child activation
ends, so fall back and alert the operator. Pending masked input and transformed
echo may also remain pending indefinitely; elapsed quiet time does not open them.

Diagnostics expose no typed bytes, expected echo, partial-match progress or byte
counts. This avoids disclosing even the length of masked input. The conservative
guard and unguarded-send behavior are unchanged.
