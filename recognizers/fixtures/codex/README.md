# Codex recognizer fixtures

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
