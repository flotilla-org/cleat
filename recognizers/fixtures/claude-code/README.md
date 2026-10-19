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

## No-login Linux recordings (#351, partial)

Recorded fixtures of **Claude Code** for the recognizer work (#313 ruling 2, issue #351). Recognizer rules are out of scope here. **This is a partial recording of #351**: see [Not recorded](#not-recorded).

| | |
|---|---|
| Harness | Claude Code `2.1.287` (`claude --version` → `2.1.287 (Claude Code)`) |
| Terminal sizes | `120x40` and `80x24` (cleat/Ghostty engine, `TERM_PROGRAM=ghostty`) |
| Permission mode | default (`⏸ manual mode on` in the footer) |
| Auth state | **not logged in** (see below) |

### Files

| Cast | Expectations stub |
|---|---|
| `claude-code-120x40.cast` | `claude-code-120x40.toml` |
| `claude-code-80x24.cast` | `claude-code-80x24.toml` |

Casts are asciicast v3, as written by `cleat` (ADR 0002). Each `cleat mark` is an `"m"` event named after the marker. The `"i"` events are the keys sent; all are the throwaway text listed below.

### Launch command

```sh
cd /tmp/scratch351   # scratch dir outside any checkout, with two throwaway files
cleat launch cc351-120x40 --tag project=cleat --tag purpose=fixture \
    --size 120x40 --cwd /tmp/scratch351 \
    --env CLAUDE_CONFIG_DIR=<crew config dir> \
    --cmd "claude --permission-mode default"
```

The same command with `80x24` made the other cast. No Claude env markers were unset (`CLAUDECODE` did not stop the nested launch). The only environment passed to the child was `CLAUDE_CONFIG_DIR`, so onboarding and the folder-trust prompt were already answered; the trust dialog is not in the casts.

Driving: `cleat wait <id> --screen-stable N`, then `cleat mark <id> <name>` once the screen was stable. Text went in with `cleat send --no-enter`, a newline in a draft with `\` then `Enter`, the cursor with `send-keys Left`, and a draft was cleared with `C-u` (repeated until the placeholder returned). Resize used `cleat attach` in a pseudo-terminal whose window size was changed (the `"r"` events); the `"r"` events carry a transient `+1` row (e.g. `60x21` then `60x20`; `120x41` then `120x40`) from the attach status line. Assert against the **final** size of each pair: `60x20` and `120x40` (80x24 session: `50x15` and `80x24`).

### Markers

"Composer" is the area between the two `─` rules. Row numbers are 1-indexed.

| Marker | What is on screen (120x40 rows; 80x24 is the same layout 16 rows up) |
|---|---|
| `empty` | Banner (logo, `Claude Code v2.1.287`, model/plan, cwd, a tip). Composer row 38: `❯ ` then faint placeholder `Try "refactor <filepath>"`. Footer row 40: `⏸ manual mode on · ? for shortcuts · ← for agents`. |
| `draft` | Composer row 38: `❯ list the files here`. Footer is just `⏸ manual mode on`. A red `Not logged in · Run /login` hint sits right-aligned above the top rule. |
| `draft-placeholder-words` | Composer row 38: the current placeholder wording followed by ` in this file`, i.e. `❯ Try "refactor <filepath>" in this file` (80x24: `Try "create a util logging.py that..." in this file`). Same footer as `draft`. |
| `draft-multiline` | Composer rows 37–38: `❯ line one` and, indented two columns with no `❯`, `  line two`. The rules move to rows 36 and 39. |
| `draft-space` | Composer row 38: `❯` and nothing visible (three spaces typed). Footer is `⏸ manual mode on`; no placeholder. |
| `draft-cursor-left` | Composer row 38: `❯ abc def`, cursor moved three cells left so it sits before `def`. |
| `resize-shrunk` | Window shrunk to 60x20 (80x24 session: 50x15) with `abc def` still in the composer; rules re-drawn at the new width, banner above. |
| `resize` | Window grown back to 120x40 (80x24) with the composer redrawn at full width at its original rows. |

The placeholder wording is chosen **at random per launch** (`Try "refactor <filepath>"`, `Try "fix lint errors"`, `Try "write a test for <filepath>"`, … were all seen). Recognizers must not hard-code it; the two casts use different wording on purpose.

#### Not recorded

`working`, `approval`, `question` and `done` are **not recorded**. The nested instance has no inheritable auth: with the crew's `CLAUDE_CONFIG_DIR`, and with a direct `claude -p`, every turn fails with `Not logged in · Please run /login`. Making credentials available to a nested instance is an operator decision, so none were copied or injected. The authenticated recordings above (#355, version 2.1.303) cover these states, but not at 2.1.287; #351 stays open for a 2.1.287 logged-in recording.

Consequences for the recorded states: the `Not logged in · Run /login` hint appears after the first keystroke (not in `empty`), and a logged-in instance may differ in banner, hints and style (the 2.1.303 casts do). See the style notes below.

### Measured style evidence

Extends the #313 revision (2.1.287: composer between two `─` rules, hardware cursor hidden, no faint text, `❯` and the draft share one span). Measured on these casts (unauthenticated, default permission mode):

- **Layout** holds: the composer is between two `─` rules (SGR `38;2;136;136;136`), footer below. `❯` is followed by a **U+00A0**, not a space. `❯` and the draft are one span: `ESC[39m❯␣<draft>` with no SGR between them.
- **Empty vs draft signal**: the placeholder **is faint** here, written as `ESC[2m` … `ESC[22m`, directly after `❯␣`. A draft is written with no `ESC[2m`. So *faint text in the composer row ⇒ placeholder ⇒ empty*, and a draft that begins with the placeholder's exact words (`draft-placeholder-words`) is distinguished from it **only by the absence of faint**, not by its text. Secondary signal: the footer loses `? for shortcuts · ← for agents` as soon as the composer holds anything (including only spaces). The #313 note "no faint text" does not hold for this unauthenticated run; the #312 casts show an empty composer as `❯␣` plus a reverse-video cell (`ESC[7m ␣ESC[27m`) instead. Whether login state explains the difference is untested at 2.1.287: the authenticated 2.1.303 casts above show no faint placeholder, so version or login state may account for it.
- **Hardware cursor**: shown at rest and placed in the composer. Every frame is wrapped in a synchronized update, hides the cursor (`?25l`), repaints, parks it at `CUP row;col` and shows it (`?25h`). The `.toml` files give the cursor cell per marker. At `empty` it sits at col 3 (the start of the placeholder). This also differs from the #313 "hardware cursor hidden" note.
- **Spaces**: a space-only draft draws no cells (`ESC[K` only). It is visible only through the cursor column (3 + number of spaces) and the missing placeholder/footer hint.
- **Cursor left**: text after the cursor stays on screen, so the value is the whole row after `❯␣`, not text up to the cursor.
- **Multi-line**: continuation rows are indented two columns without `❯`; the rules move to enclose them.
- **Resize**: rules and footer are redrawn at the new width; the composer and its text survive (shrink and grow both repaint the whole screen with `ESC[2J`).

### OSC 7501 and other program reports

Neither cast contains **any** OSC 7501 (not even a `?` support query from the program), so `cleat inspect --json` reported `program_status: []` (with `program_status_available: true`) at **every** marker, in both sizes. `title` is `✳ Claude Code` (set once with OSC 0 at start). No OSC 133 was emitted either. So 2.1.287 in this state reports no program status and a recognizer has nothing from OSC 7501 to agree or disagree with: `working`/`blocked` waits cannot be driven by it. Working/approval markers were not recorded, so this is **unmeasured for those states**.

### Privacy pass

Both casts were read in full (every output and input event, decoded) before commit:

- No tokens, account names, emails or organization names; no hostnames or internal URLs; the only path is the throwaway `/tmp/scratch351`. The only file names typed are throwaway text. No keystroke dump of anything credential-shaped (the keys sent are the `i` events above: short throwaway strings and `Ctrl-U`).
- **Redaction**: the banner's billing-plan text was replaced with `[plan redacted]` (same width) in both casts with `sed`. The model name (`Opus 5.5`), version and permission-mode footer are kept.
- Repeatable check for later re-records (should print nothing):
  `grep -a -o -i -E 'sk-[A-Za-z0-9_-]+|[a-z0-9._-]+@[a-z0-9.-]+|https?://[^"\\ ]+|/home/[^"\\ ]*' *.cast`
- The unauthenticated state means no account identity is shown at all.
