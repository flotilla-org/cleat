# Claude Code recognizer fixtures

## Authenticated macOS recordings (#355)

Recorded 2026-10-19 on macOS with **Claude Code 2.1.303**
(`claude --version`) and cleat 0.1.0 (`d2f1f82925f0`, Ghostty VT).
Recording is enabled by default ([ADR 0002](../../../docs/adr/0002-recording-on-by-default.md)).
These casts complement #351's no-login fixtures at 2.1.287. Recognizer rules
and verified version ranges belong to #353; these recordings provide the
same-version `empty` and `draft` anchors needed for that work.

| Cast | Terminal size | Expectation stub |
| --- | --- | --- |
| `auth-120x40.cast` | 120 columns × 40 rows | `auth-120x40.toml` |
| `auth-80x24.cast` | 80 columns × 24 rows | `auth-80x24.toml` |

### Reproduction

Work outside a checkout in a throwaway directory:

```sh
scratch=$(mktemp -d /tmp/cleat-auth-fixtures.XXXXXX)
cleat --server auth-fixtures-355 launch auth-claude-120 --size 120x40 \
  --cwd "$scratch" --tag project=cleat --tag purpose=probe \
  --cmd 'env -u CLAUDECODE claude --permission-mode manual --setting-sources ""'
```

Use a fresh session ID and `--size 80x24` for the smaller recording. Both
published casts used the same scratch directory. `CLAUDECODE` is the only
environment marker unset. The harness reads the host's existing login in
place; no credential file is copied, printed or moved. `manual` is this
version's ordinary permission-prompt mode. `--setting-sources ""` excludes
user/project/local settings for this invocation so existing allow rules or
auto mode cannot suppress the dialog; it does not edit global configuration.
The initial workspace trust prompt at 120×40 was accepted. The second
session reused that scratch directory's trust.

Type `Describe the throwaway file.` with `cleat send ID --no-enter TEXT`,
then clear it with `cleat send-keys ID C-u`. Submit each prompt with
`cleat send ID --submit TEXT`:

```text
For a terminal fixture, run sleep 20; printf "fixture complete\n" using Bash, then reply Done. Use only that harmless command.

For a permission-dialog fixture, use Bash to run touch approval-fixture.txt in this scratch directory. Ask for permission if required, and do not change permission settings. Then reply Done.

For a question-dialog fixture, call the AskUserQuestion tool now. Ask which throwaway color to use, with options Blue and Green. Do not inspect files or run commands.
```

The 80×24 permission prompt substitutes `approval-small-fixture.txt` for the
filename. Sleep/printf ran without approval. The subsequent `touch` request
produced the recorded permission dialog; approve only once with Enter,
leaving the mode unchanged. The question prompt invokes `AskUserQuestion`
and produces an actual question dialog at both sizes. The smaller session
queued this prompt briefly while the preceding reply finished.

Use `cleat capture ID` to check the rendered screen before
`cleat mark ID NAME`. Working markers were placed while sleep was running,
after its command/status area had appeared; spinner frames continue changing.
The casts end at their final `question` markers, before shutdown.

### Named points and expectations

Each cast contains these markers in order:

| Marker | Screen at marker |
| --- | --- |
| `empty` | Empty composer with `Try "refactor <filepath>"` at 120×40 or `Try "fix lint errors"` at 80×24. |
| `draft` | Unsent `Describe the throwaway file.` |
| `working` | Bash sleep/printf in progress, running status, empty composer. |
| `done` | `Done.` reply and command output above the idle empty composer. |
| `approval` | Bash permission dialog for `touch`; selected `❯ 1. Yes` is a dialog option. |
| `question` | `Which throwaway color should I use?`, with Blue, Green and free-text options. |

Adjacent TOML files use the local `[[expect]]` stub format from #350:
marker, expected roles, dotted role values, and `absent` roles. They are
inputs for #353, not executable recognizer rules. Dialogs must not be
mistaken for composers even though both use `❯`.

### Style and OSC 7501 evidence

At both sizes, the composer lies between two `─` rules. The placeholder
and draft have no faint SGR styling; `❯` is not bold. Empty versus draft
therefore needs the placeholder text and surrounding composer structure,
not intensity alone. At the settled `empty`, `draft`, `working` and `done`
markers, DECSET `?25h` leaves the hardware cursor visible. It starts at
zero-based column 2 for an empty value and column 30 after the draft.
Both dialogs hide the hardware cursor with `?25l`. This differs from the
2.1.287 evidence described in #351; do not extend that version's cursor
assumption to 2.1.303.

Both casts emit OSC 7501. The latest declaration at each marker is:

| Marker | OSC 7501 state | Kind |
| --- | --- | --- |
| `empty`, `draft` | `idle` | absent |
| `working` | `working` | absent |
| `done` | `done` | absent |
| `approval` | `blocked` | `permission` |
| `question` | `blocked` | `question` |

Every declaration names `app=claude-code`. Blocked messages are base64
encoded: the permission message describes the throwaway `touch` command;
the question message contains only the color question/options. These are
status payloads, not credentials. Live `cleat inspect` also reported
`working` during the first command. Preserve agreement with these status
signals when implementing #353.

### Privacy and validation

Every cast event was decoded and reviewed, including input, OSC titles,
hyperlinks and the base64 OSC 7501 messages. The only file changes are empty
throwaway files inside the `mktemp -d` directory. No login flow, credential
input, credential file, home path, username, email, account organization,
hostname or internal URL is included.

One equal-width redaction per cast replaces the banner's account plan
with `Sample XXX`, preserving the intervening cursor-position escape.
The model name and product name remain. Output after the final marker was
omitted, including shutdown's local session-resume identifier. Event order,
delta timings, terminal size, markers, SGR and cursor controls up to that
marker are preserved. Original host recordings are not committed.

Validation parses every asciicast v3 JSON event and TOML stub, checks exact
marker coverage and dimensions, and replays each marker through an
independent terminal emulator to inspect composer text, reply/dialog text
and cursor visibility. Raw SGR and OSC payloads were checked separately;
unsupported terminal capability queries in that emulator are not treated
as visible style evidence. No recognizer behavior is claimed by the stubs.
