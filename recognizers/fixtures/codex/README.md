# Codex recognizer fixtures

## Linux recordings (#350, Codex 0.160.0)

Recorded 2026-10-19 with **`codex-cli 0.160.0`** (`codex --version`),
cleat 0.1.0 (`d2f1f82925f0`, Ghostty VT), recording enabled by default
([ADR 0002](../../../docs/adr/0002-recording-on-by-default.md)).
These are terminal evidence for #350 / #313; recognizer rules belong to #352.

| Cast | Initial and restored terminal size | Shrunk size |
| --- | --- | --- |
| `codex-120x40.cast` | 120 columns × 40 rows | 60 × 16 |
| `codex-80x24.cast` | 80 columns × 24 rows | 60 × 16 |

### Reproduction

Use an empty scratch directory outside any checkout:

```sh
mkdir -p /tmp/codex-fixture-scratch
cleat launch fixture-codex-120 --size 120x40 \
  --cwd /tmp/codex-fixture-scratch --record \
  --cmd 'codex --no-daemon --no-alt-screen -a on-request -s read-only -c check_for_update_on_startup=false'
```

For the smaller cast, use a fresh session id and `--size 80x24`.
The nested harness uses existing provisioned credentials without copying or
printing them. Approval is explicitly `on-request`, sandbox `read-only`;
the crew's bypass mode is not inherited. No files were read or created by
the nested harness. The harmless command prints only `fixture complete`.
The startup PATH-alias warning is real and left visible. It is startup
output, not a status or composer node; #352 must exclude it.

Drive drafts with `cleat send ID --no-enter TEXT`, clear with
`cleat send-keys ID C-c`, move to the start with `Home`, and mark after
`cleat wait ID --screen-stable 1 --timeout 5`. Multi-line text is one send
containing a literal newline. Spaces are exactly three ASCII spaces.
The submitted prompt (using `cleat send ID --submit`) is:

```text
For a terminal fixture, run sleep 20; printf "fixture complete\n" using the command tool, then reply Done. Use a harmless command only.
```

The local sandbox cannot create namespaces. Codex retries the same harmless
command with an explicit approval dialog; mark it before approving once with
Enter. Wait for `Worked for` and a stable screen before marking `done`.

For resize, attach a controller in a PTY, shrink and grow its dimensions,
and signal the controller with SIGWINCH at each change. Cleat reserves one
row for its attachment bar: use outer dimensions 120×41 or 80×25, shrink to
60×17, then restore the original outer size. Wait two seconds before each
resize marker. Detach with Ctrl-] d; kill the session after all markers.
Attachment briefly reports the full outer height before reserving its bar
(120×41 or 80×25), then returns to the requested content height. No marker
is placed in that transient state. The cast's `r` events contain all actual
child dimensions.

### Named points (both casts)

| Marker | Screen at the marker / expected roles |
| --- | --- |
| `empty` | Empty composer, faint `Ask Codex to do anything` placeholder. |
| `draft` | Composer contains `Describe the throwaway file.` |
| `draft-placeholder-words` | Plain draft `Ask Codex to do anything with this draft`; the initial words are not a placeholder. |
| `draft-multiline` | Composer contains `First throwaway line`, newline, `Second throwaway line`. |
| `draft-space` | Composer contains three spaces; no placeholder. |
| `draft-cursor-left` | `Text after cursor`, cursor at the beginning of the value. |
| `working` | `Working (... • esc to interrupt)` status; empty composer with placeholder. |
| `approval` | Command approval dialog for sleep/printf. The selected `›` is a dialog option, not a composer. |
| `done` | `Done.` reply and `Worked for` footer; idle empty composer. |
| `resize-shrunk` | Settled 60×16 screen, empty composer below reflowed history. |
| `resize` | Restored original size; earlier submitted prompt returns to view above the current empty composer. |

The adjacent TOML files are **expectations stubs**, not executable rules or
a claim that the engine already supports their schema. Each `[[expect]]`
identifies a cast marker, lists expected roles, and gives dotted role
properties. `absent` lists roles that must not be inferred. Only evidenced
properties are asserted: idle points do not invent a status node. #352 can
adapt the stub layout to the engine's fixture API while preserving these
values, including spaces and the literal newline. When adapting the stubs
for #352, also assert the completed `Worked for` footer and that historical
prompt markers after resize do not produce extra composer nodes. Keep the
expectations shared across both sizes once the engine supports that, so the
identical role/value contracts cannot drift. These are adaptation requirements,
not invented role names in the current stubs.

### Measured style evidence (extends #313 revision)

The casts preserve output SGR and hardware cursor controls:

- The composer `›` uses SGR **1** (bold), followed by SGR 22 to reset it.
- The empty placeholder uses SGR **2** (faint). Draft text, including the
  draft starting with the placeholder wording, is normal intensity.
- Space-only input has a visible cursor three columns after the insertion
  start, despite a visually blank value. `draft-cursor-left` moves the
  cursor back to the insertion start with all draft text after it.
- The hardware cursor is shown (DECSET `?25h`) at settled composer markers,
  including `working`, and hidden (`?25l`) in the approval dialog. Redraws
  temporarily hide it, so intermediate escape events are not settled state.
- Approval also uses `›` for the selected option: bold marker evidence alone
  cannot establish a composer. Resize exposes historical prompt markers;
  recognition must distinguish them from the live composer.

### Privacy and validation

Every event in both published casts was decoded and read, including input,
OSC title payloads, attachment metadata and output. There are no tokens,
account names, emails, hostnames, internal URLs, or file contents. Input
events contain only the throwaway drafts, harmless command prompt and
control keys; no credential-shaped keystrokes were recorded.

Two global, equal-width text redactions were applied to every event string:
`OpenAI` → `Sample` in the banner, and the organization component of the
harness's temporary path → `fixturex`. No timings, markers, terminal sizes,
SGR, cursor controls or event ordering were changed. The public model label
`GPT-6.1-Sol` remains; it identifies a model, not an account. The original
recordings are not committed.

Validation parses every JSON event and both TOML stubs, checks exact marker
coverage and resize dimensions, and replays output through an independent
terminal emulator to inspect each marker's composer text and cursor. Style
claims were checked directly against the recorded SGR sequences. No
recognizer implementation or rules are included.

## Authenticated macOS recordings (#355)

Recorded 2026-10-19 on macOS with **codex-cli 0.162.1** (`codex --version`)
and cleat 0.1.0 (`d2f1f82925f0`, Ghostty VT). Recording is enabled by default
([ADR 0002](../../../docs/adr/0002-recording-on-by-default.md)).
At recording time, #350's PR #354 was unmerged and recorded Codex 0.160.0.
No requested state was skipped. These casts record 0.162.1; #354 records
0.160.0. The `empty` and `draft` markers anchor the turn-dependent states
to 0.162.1 for #352.

| Cast | Terminal size | Expectation stub |
| --- | --- | --- |
| `auth-120x40.cast` | 120 columns × 40 rows | `auth-120x40.toml` |
| `auth-80x24.cast` | 80 columns × 24 rows | `auth-80x24.toml` |

### Reproduction

Use an empty `mktemp -d /tmp/cleat-auth-fixtures.XXXXXX` directory outside
any checkout. The exact launch commands were:

```sh
cleat --server auth-fixtures-355 launch auth-codex-120 --size 120x40 \
  --cwd "$scratch" --tag project=cleat --tag purpose=probe \
  --cmd 'codex --no-daemon --ask-for-approval on-request --sandbox workspace-write'

cleat --server auth-fixtures-355 launch auth-codex-80 --size 80x24 \
  --cwd "$scratch" --tag project=cleat --tag purpose=probe \
  --cmd 'codex --no-daemon --ask-for-approval on-request --sandbox workspace-write -c check_for_update_on_startup=false'
```

Both recordings used the same scratch directory as the Claude Code casts.
The host's existing login is used in place; no credentials are copied,
printed or moved. Approval is explicitly `on-request` with the ordinary
`workspace-write` sandbox, overriding the crew's bypass policy. No global
configuration is edited. The 120×40 session offered an update to 0.163.0;
Escape skipped it without updating. The smaller session disables that
startup check for this invocation. The collapsed warning indicator is
left as recorded; its detail panel was never opened.

Type `Describe the throwaway file.` using `cleat send ID --no-enter TEXT`,
clear it with `cleat send-keys ID C-u`, then submit these prompts with
`cleat send ID --submit TEXT`:

```text
For a terminal fixture, run sleep 20; printf "fixture complete\n" using the command tool, then reply Done. Use only that harmless command.

For an approval-dialog fixture, request explicit approval to run the harmless command printf "approval fixture\n" outside the sandbox. Do not bypass approval. After approval, reply Done.
```

The first command runs inside the sandbox and returns to idle. The second
request explicitly asks for the ordinary unsandboxed approval dialog,
since sleep/printf inside the sandbox needs no approval. The requested
command only prints a throwaway string. No file is read or changed by
Codex. Published casts end at the `approval` marker, before its response.
Use `cleat capture ID` before `cleat mark ID NAME`. Working markers were
placed after the command and running status appeared, while sleep was
still running; spinner frames continue changing.

### Named points and expectations

Each cast contains these markers in order:

| Marker | Screen at marker |
| --- | --- |
| `empty` | Empty composer and `Ask Codex to do anything` placeholder. |
| `draft` | Unsent `Describe the throwaway file.` |
| `working` | `Working` status, background terminal running, empty composer. |
| `done` | `Done.` reply, command output and `Worked for` footer above the idle composer. |
| `approval` | `Would you like to run the following command?`, printf command and Yes/No options. |

Adjacent TOML files follow #350's local expectations-stub format:
`[[expect]]` pairs a marker with roles, dotted role values and absent
roles. These are inputs for #352, not executable recognizer rules.
The selected `› 1. Yes, proceed` in an approval dialog is not a composer.
Claude Code alone has a `question` requirement in #355.

### Measured style evidence

Both sizes preserve SGR and hardware cursor controls:

- Composer `›` is bold (SGR 1); SGR 22 resets intensity before its value.
- `Ask Codex to do anything` is faint (SGR 2); draft text is normal intensity.
- The hardware cursor is visible at `empty`, `draft`, `working` and `done`:
  zero-based column 2 for empty input, column 30 after the draft. Redraws
  temporarily hide it, but those transient frames are not the marker state.
- The approval dialog hides the cursor and reuses `›` for its selected
  option. Earlier submitted prompts remain in history, so marker glyphs
  alone cannot identify the live composer.
- No OSC 7501 declaration or query appears in either cast.

### Privacy and validation

Every event in both casts was decoded and reviewed, including input,
OSC titles and hyperlinks. No login flow, credential-shaped input,
credential file, home path, username, email, account organization or
internal URL is recorded. Public product/model names and the public
release-notes URL remain.

Equal-width redactions replace the host's four-character hostname with
`host` throughout both recordings, and replace the two-digit weekly
account-usage percentage with `--%` in the 120×40 footer (it never appears
at 80×24). Recognizers must not depend on the replacement `host` or `--%`
text. The scratch path has no account name and remains as recorded.
Recordings stop at the final marker. No event order, delta timing,
terminal size, marker, SGR or cursor control before that point is changed.
Original host recordings are not committed.

Validation parses all asciicast v3 JSON events and TOML stubs, checks exact
marker coverage and dimensions, replays every marker through an independent
terminal emulator to inspect text and cursor visibility, and checks raw SGR
for placeholder intensity. The fixtures add no recognizer implementation.
