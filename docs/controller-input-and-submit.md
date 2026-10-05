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
`SendPreconditions`, and only then records the optional marker and writes. The
precondition container and its evaluation step are the extension seam for a
future selector `--if`; this change does not implement selectors.

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
unguarded send keeps its existing keys request. Both guarded send and submission
work with the no-VT build; paste uses that engine's existing byte encoder.

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
