# Recognizer engine

Recognition is opt-in: `cleat launch --recognizer synthetic --cmd ...` is
identical to adding `--tag recognizer:synthetic` at launch. Exactly one id may
be selected. Conflicting `--recognizer` and `--tag recognizer:` ids are rejected
by the CLI before daemon startup. Cleat never infers an id from the command.
Selection persists in session metadata, including recreation and host transfer.
Changing tags with `cleat tag` does not replace the active engine; use
`cleat recognizer set` (below).

## Live attach and replace

```sh
cleat recognizer set SESSION codex
```

loads the id exactly as `--recognizer` does at launch (an add-on file, else the
bundled rules) and makes it the session's only recognizer, replacing any running
one. The `recognizer:<id>` tag replaces the previous selection tag, so
recreation and host transfer keep the choice like a launch-time one. The new
engine evaluates the current frame at once. An id or file that fails to load
changes nothing (exit 1, HTTP 422). Re-run the command after editing an add-on
file to reload it. The HTTP equivalent is `POST /sessions/SESSION/recognizer`
with `{"id":"codex"}`, answered with `{"tags":[...]}`. One recognizer serves
every attachment; per-attachment recognizers are not provided.

`cleat inspect ID --explain` displays the active id, revision, error, and each
rule's match, named captures, and region provenance. `inspect --json` includes
this information under `recognizer` without needing `--explain`. A role that
has not been established is absent; it is never an empty placeholder. Use
`--json` as the structured contract for automation; human output and explain
formatting are diagnostic and may change.

## Files and installation

Bundled sources live in `recognizers/<id>.toml`. They are embedded in the binary
so installation does not depend on the build checkout. The shipped `synthetic`
recognizer is an engine demonstration. `claude-code` covers the recorded
Claude Code 2.1.287 composer scenarios and 2.1.303 composer, working, completed,
approval and question scenarios. It also covers 2.1.287 under flotilla crew
settings, whose bypass-permissions footer has no `? for shortcuts` hint: an
empty composer there is the marker alone with the cursor at the value start.
Select it explicitly with `--recognizer claude-code`.

`codex` is verified against the recorded 0.160.0 and 0.162.1 harnesses;
see its fixture README for evidence and limits.

An existing add-on overrides the bundled file with the same id. Malformed or
unreadable overrides, including broken symlinks, fail loading; they do not fall back to bundled rules.

Add-ons live in the directory named by `CLEAT_RECOGNIZERS_DIR`,
or otherwise `$XDG_CONFIG_HOME/cleat/recognizers` (default
`$HOME/.config/cleat/recognizers`) on Unix and `%APPDATA%\cleat\recognizers` on
Windows. If no platform configuration base is available, add-ons are disabled;
bundled rules remain available. Configured paths must be absolute. Install
`<id>.toml` there. Files are read when a session starts, on host adoption and
on `recognizer set`. A missing or invalid file during
host adoption leaves the PTY running and exposes an inspect error with the
selected id and revision 0 (the revision could not be loaded).
Invalid files reject launch before the child starts; diagnostics include the
filename, line, and rule. All rules are validated and selectors compiled before
the file is installed. Unrecognized keys are errors. Rule diagnostics use the
parsed rule span; syntax errors identify the file and exact parser line.

```toml
id = "example"
revision = 1
min_engine = "0.1.0"
verified = ["example-harness >=1.0, <2.0"]

[[rule]]
select = 'row:matches(/^Draft: (?P<draft>\S+)/)'
role = "composer"
value = "{draft}"

[[rule]]
select = 'title:has-text("Working")'
state = "working"

[[rule]]
select = 'row:has-text("Permission required")'
role = "dialog"
state = "blocked"
kind = "permission"
```

`revision` is an unsigned integer. `min_engine` is a semantic version, checked
against engine version `0.1.0`. `verified` is a required list of harness names
and semver requirements; it records verification claims, not command detection.
`rule` may be empty. A rule requires `select` and at least one of `role` or
`state`. `select` uses terminal-screen's existing selector grammar, including
`:matches(/regex/)` and `:has-text("literal")`. `value` substitutes named
captures with `{name}`; literal text around captures is preserved. Unknown
capture names fail loading; absent optional captures cause an evaluation error. An evaluation error discards the entire observation for that frame, including otherwise successful rules, and is reported in inspect.
Without `value`, a role retains the matched node's text, including spaces.

A capture can also be extracted line by line:

```toml
value = { capture = "input", skip = 2, trim_padding = true }
```

`skip` drops Unicode scalar values from the start of each captured line (for
example, a prompt marker and its following space, or continuation indentation).
`trim_padding` requires the capture to span the whole matched node. It removes
trailing ASCII screen spaces, retaining spaces before a visible hardware cursor. Display width, including wide and combining characters,
is used when padding to the cursor. Unicode width tables can differ from the
VT for ambiguous-width characters and emoji variation selectors. Text after a
cursor moved left is retained.
Spaces after the cursor cannot be distinguished from screen padding; this mode
does not claim to recover them. Missing captures fail evaluation as templates do.

`role` is an ASCII identifier (letters, digits, `_`, `-`). Each match produces a
node of that name in the same screen tree, carrying its role, value, state,
kind and provenance attributes, and the matched rectangle. States use OSC 7501's
`idle`, `working`, `done`, `blocked`, `error`. `kind` requires `blocked` and is
`approval`, `permission`, `question` or `auth`. `approval` is a dialog UI kind;
its program status uses OSC 7501's `permission` kind. Rules evaluate against the
input tree, not each other's output. When a rule matches declared and inferred regions, only
declared matches are used. Rules run in file order and matches in document
order; the last matching state declaration wins. `title` is a synthetic node
whose text is the latest observed terminal title, including a known empty title.
State matched directly on it has `source = title`; screen state uses
`source = recognizer`. Inferred state uses the root record id (empty string).
OSC reports remain in the store alongside the latest inferred root record;
`ProgramStatusTracker::effective_records` applies ADR 0007 rule 6: after the
first OSC report, inferred fallback is suppressed until program end / OSC 133 A.
The unfiltered subscription and inspect store allow consumers to arbitrate.

## Evaluation and publication

Screen queries attach the latest completed recognition observation to their
fresh structural tree. Annotation is read-only: a query does not force another
evaluation or advance debounce confirmations. A just-changed screen can briefly
precede the next recognition pass, so role attributes describe the latest
completed observation rather than guaranteeing synchronous recognition of every
cell in the query snapshot.

Selectors run only on a changed screen generation or changed title. Timer
confirmations use cached results. A working-to-idle title transition requires
three confirmations spaced at least 100 ms apart, with a 700 ms deadline.
A visibly idle screen or blocked state publishes immediately. A return to
working cancels pending idle. No match or evaluation failure clears the inferred
record through the subscription path. The last confirmed state is retained
internally across such observation gaps so a subsequent title-idle still
requires working-to-idle debounce. Recognition preserves render damage and runs
outside input admission: errors and Rust unwinding panics appear in inspect
and cannot refuse unguarded sends. A new generation retries failed evaluation.
Repository profiles retain Rust's default `panic = "unwind"`. Embedders must
also use unwinding for panic isolation: with `panic = "abort"`, a panic ends the
process before `catch_unwind` can run.

`SessionService::connect_recognizer` subscribes to initial values and changes
through `ActivityEvent::RolesChanged` and `ProgramStatusChanged`, including role
removal as an empty list. HTTP `/connect` clients opt in with
`{"recognizer_roles":true}` (protocol 15). Existing status-only subscribers
retain protocol 14 and the unchanged status payload. Legacy activity/render
subscriptions retain their existing protocol and payloads.

For applications that need Rust, use `Rules::into_recognizer` when loading a
stylesheet (this installs Codex's style/cursor adapter), or implement `recognizer::Recognizer` and create
an `Engine`. Return an `Observation` with roles, state, source, and explanations.
Implement `supported_roles()` with every role the recognizer can produce,
independently of the current frame. Atomic `send --if` guards use this declaration
to distinguish unsupported sources from temporarily absent roles. Its default
is an empty set; TOML rules and the Codex adapter declare their roles automatically.
The engine installs the same nodes and contains errors/panics. The input tree
is immutable to the implementation. A functional VT backend is required for
live sessions and cast fixtures.

## Fixtures and performance

Fixtures live in `recognizers/fixtures/<id>/`: an asciicast v3 `.cast` and a TOML
file with the same stem. `recognizer::check_fixture` is the unpaced library
replay entry point. It feeds output and resize events to the supplied VT,
evaluates observations, and checks named `m` markers:

```toml
[[marker]]
name = "draft"
state = "working"
source = "title"
role = [{ role = "composer", value = "hello" }]

[[marker]]
name = "cleared"
role = []
```

Role entries optionally include `kind`, which is compared exactly (omitting it
expects no kind). `agree_osc = true` on a marker also requires a program report
whose `app` matches the recognizer id and compares its state and kind with the
recognizer's own result. The report never supplies the inferred answer.
Role lists are exact, ordered observations; omitted `role` means empty. Optional
state and source constrain those fields. Optional role `state` constrains that
attribute; marker `contains` requires recorded screen text such as the completed
`Worked for` footer. State assertions compare the last
confirmed, debounced engine state rather than the current candidate. Duplicate
expectations, unexpected or missing markers, evaluation errors and mismatches
fail the harness. See the synthetic
fixture and `synthetic_cast_fixture` test. The harness never forwards recorded
terminal replies or clipboard effects.

Run the explicit release budget on #316's adversarial 200×50 plus screen:

```
cargo test --release -p cleat recognizer_adversarial_plus_budget -- --ignored --nocapture
```

It reports recognition time including frame cloning and checks the 5 ms budget.
The segmentation budget remains independently measured by terminal-screen's
`adversarial_plus_budget` test. The Claude rules are original rules derived
from the recorded casts. Codex translations carry attribution in
`recognizers/fixtures/codex/README.md`; future translations must also carry notices.
