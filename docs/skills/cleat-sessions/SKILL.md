---
name: cleat-sessions
description: Host interactive/TUI programs in persistent, recorded cleat terminal sessions - launch, drive, wait on, capture, and retire them; includes driving another agent (codex/claude TUI) inside a session, locally or over ssh. Use when running interactive or TUI programs (REPLs, debuggers, installers, menuconfig), when work should survive the current session or be watchable/recorded, or when asked to drive, supervise, or babysit another agent in a terminal.
---

# cleat sessions

Cleat hosts programs in persistent, recorded terminal sessions with a
structured control plane. Prefer it over backgrounding interactive
programs or parsing raw escapes. Canonical guide (conventions, gotchas,
full verb set): `docs/skills/cleat-sessions/README.md` in the cleat repo,
if checked out locally.

**Prerequisite probe** (version strings can't distinguish builds yet): the
installed binary must support the modern surface — check once with
`cleat launch --help | grep -q -- --tag`. If that fails, the binary
predates this skill; do not proceed with these commands — report the stale
binary instead.

## Quick start

```sh
cleat launch build --tag project=myrepo --tag purpose=build \
    --size 200x50 --cwd ~/dev/myrepo --cmd "make menuconfig"
cleat wait build --text "Main menu" --timeout 20
cleat capture build                # rendered screen, plain text
cleat send build --submit "..."    # type + reliably submit into a TUI
cleat kill build                   # recording survives (--purge discards)
```

`launch <id>` reuses a live session with that id. Set a realistic `--size`
— TUIs behave differently at 80×24. Recording is on by default.

## Key verbs

- `capture <id>` — rendered grid as text (never parse escapes)
- `send <id> "text"` (adds Enter) · `send --submit` for TUI composers
  (plain text+Enter gets swallowed by paste heuristics) · `send-keys <id>
  C-c Enter Escape ...`
- `wait <id> --text "str" --screen-stable 30 --idle-time 5 --timeout 600`
  — conditions OR together; **`--timeout` is bare seconds**; spinners
  defeat `--idle-time`, use `--screen-stable` for TUIs
- `inspect <id>` — `fg_pgid != leader_pid` ⇒ a command is running
- `mark <id> m1` … `transcript <id> --since-marker m1` — recorded output
  slices; `expect <id> "text"` blocks until text is output
- `interrupt <id>` (Ctrl-C byte) vs `signal <id> INT --target foreground`
- `list [--selector k=v]... [--watch]` · `tag <id> +t -t`
- `kill <id>` preserves the recording; `--purge` discards

## Tags (portfolio conventions)

Opaque strings, `key=value` by convention, AND-only exact-match selectors.
Set `project=<repo>` and `purpose=<build|test|probe|agent>`. Never set
`vessel=…` (reserved for flotilla adoption).

## Driving another agent (proven pattern)

```sh
cleat launch codex-task --tag purpose=agent --size 200x50 --cwd <worktree> --cmd codex
cleat wait codex-task --text "OpenAI Codex" --timeout 30
cleat send codex-task --submit "<task brief>"
# supervise loop:
cleat wait codex-task --text "Press enter to confirm" --screen-stable 180 --timeout 3600
cleat capture codex-task   # see WHY it stopped: approval → send-keys Enter; question → send --submit
```

Humans can `cleat watch <id>` read-only throughout; the recording is the
audit trail.

## Remote work today (ssh inside a session)

Zero-install remote: a local session whose command is
`ssh -t <host> '<agent>'` (`-t` forces a real PTY). All verbs work
unchanged on the local grid; ssh is just bytes in the middle.

```sh
cleat launch codex-paneer --tag purpose=agent --size 200x50 \
    --cmd "ssh -t paneer 'cd ~/dev/porthole && exec codex'"
```

Know the limits: the remote end is bare (recording and all verbs live
locally); local daemon death or a dropped connection SIGHUPs the remote
agent; recreation replays history but runs a **fresh** ssh — the hosted
agent's own resume is the recovery path; `signal` hits the local ssh
client only (and `--target tree` is not yet implemented) — `interrupt`
travels as bytes and reaches the remote program. Full caveat list: README
§"Remote work today".

## Gotchas

- Detached sessions get capability answers from the VT engine, not a real
  terminal — capability-sensitive programs may act differently detached.
- One controller at a time; watchers are read-only and never resize.
- Daemons are invisible plumbing: auto-start, 120 s linger. Use
  `--server NAME` only for hard isolation; grouping is tags' job.
