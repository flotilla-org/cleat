# Driving cleat sessions (for agents)

Harness-neutral guide to using cleat as an agent: hosting interactive
programs in persistent, recorded, inspectable terminal sessions instead of
scraping a scrollback or blocking your own shell. Thin per-harness adapters
(a Claude Code skill, a system-prompt fragment for pi/codex) install this;
the canonical content lives here.

Everything below documents behavior that exists today. If a pattern seems
to need new tooling, file an issue instead of inventing workflow.

## When to reach for cleat

- **Interactive or TUI programs** — anything that wants a real terminal
  (REPLs, debuggers, installers, TUIs). `capture` reads the rendered grid,
  so you never parse raw escape sequences.
- **Work that should survive you** — a session outlives your own process;
  another agent (or a human) can pick it up later by id.
- **Anything worth a recording** — sessions record by default (asciicast
  v3). Repros, reviews, demos come for free.
- **Driving another agent** — the proven pattern: host a codex/claude TUI
  in a session, submit prompts, watch for readiness, approve prompts, let a
  human `watch` live. Cleat's own M:N milestone was built this way.

Not for: one-shot non-interactive commands (just run them), or programs you
only need piped stdout from.

## Core loop

```sh
cleat launch build --tag project=myrepo --tag purpose=build \
    --size 200x50 --cwd ~/dev/myrepo --cmd "make menuconfig"
cleat wait build --text "Main menu" --timeout 20
cleat capture build                  # rendered screen, plain text
cleat send build --submit "..."      # type + reliably submit
cleat kill build                     # ends it; recording survives
```

- `launch <id>` **reuses** a live session with that id (no error, no
  duplicate); pick meaningful ids. Omit the id to get a generated one.
- `--size COLSxROWS` matters: TUIs *behave* differently at 80×24 (collapsed
  panels, different wrapping). Set a realistic size at launch; the grid
  resizes to a controller when one attaches (watchers never resize it).

## The verb set

| Need | Verb |
|---|---|
| Create/reuse a session | `launch <id> [--tag k=v]... [--size CxR] [--cwd DIR] --cmd "..."` |
| See the rendered screen | `capture <id>` |
| Type text | `send <id> "text"` (appends Enter), `send --no-enter`, `send-keys <id> C-c Enter ...` |
| **Submit into a TUI composer** | `send <id> --submit "prompt"` — paste-encoded + delayed Enter. Plain `send` text+Enter gets swallowed by paste heuristics (codex, claude). |
| Wait for readiness | `wait <id> --text "str" --screen-stable 30 --idle-time 5 --timeout 600` — conditions OR together |
| Structured state | `inspect <id>` — size, leader pid, **foreground pgid** (`fg_pgid != leader_pid` ⇒ a command is running), attachments, recording |
| Recorded output since a point | `mark <id> name` … `transcript <id> --since-marker name` ; `expect <id> "text"` blocks on recorded output |
| Interactive use (human) | `attach <id>` (controller), `watch <id>` (read-only) |
| Signals | `interrupt <id>` (byte Ctrl-C) vs `signal <id> INT` (real signal, `--target foreground|leader|tree`) |
| End it | `kill <id>` — **recording is preserved**; `kill --purge` discards |
| Enumerate | `list [--selector k=v]... [--watch] [--all]` |

### Choosing a wait condition

- `--text "..."` — a known string will appear on the grid. Most precise.
- `--screen-stable <dur>` — the grid stopped changing (small spinner/timer
  redraws are tolerated). Right for "the TUI is waiting for input".
- `--idle-time <dur>` — no PTY output at all. **Defeated by TUI
  spinners/timers** (they redraw every second); use for plain CLIs only.
- `--timeout` is bare **seconds** (`600`, not `10m`).
- OR-combine for agent supervision: `--text "Press enter to confirm"
  --screen-stable 180` catches both approval prompts and quiet completion.

## Daemons and addressing

A daemon is a named process hosting many sessions. You almost never think
about this: the default daemon (`default`) **auto-starts on first use** and
lingers 120 s after its last session ends (`CLEAT_DAEMON_LINGER`
overrides). "Have I got a usable daemon?" is answered by naming one.

- A session address is `(daemon, id)`; unqualified ids mean `default`.
- `--server NAME` (global flag) selects another daemon — use for hard
  isolation (test scratch space, a separate agent fleet), **not** for
  grouping. Grouping is what tags are for.

## Tag conventions

Tags are flat opaque strings; cleat never interprets them. `key=value` is
convention. Selectors are **AND-only exact matches** on whole strings.

Portfolio conventions (decision 2026-07-06):

- `project=<repo>` — which repo/effort the session belongs to
- `purpose=<build|test|probe|agent>` — what it's for
- `vessel=<id>` — **reserved**; do not set. Flotilla will use it when
  adopting sessions.

Retag live sessions with `cleat tag <id> +new-tag -old-tag`.

## Recordings as artifacts

Recording is **on by default**, asciicast v3, at
`$runtime_root/<daemon>/sessions/<id>/session.cast` (runtime root:
`$CLEAT_RUNTIME_DIR`, else `$XDG_RUNTIME_DIR/cleat`, else
`$TMPDIR/cleat-<uid>`). It survives `kill` and daemon exit, and is the
basis for recreation (relaunching an id replays prior history as
scrollback). `mark <id> <name>` before risky steps so `transcript
--since-marker` can slice exactly the interesting part. `replay` plays a
cast (or slice) back at controlled speed.

## Driving another agent in a session (proven pattern)

1. `launch codex-task --tag project=X --tag purpose=agent --size 200x50
   --cwd <worktree> --cmd codex`
2. `wait codex-task --text "OpenAI Codex" --timeout 30` (banner up)
3. `send codex-task --submit "<the task brief>"`
4. Supervise: `wait codex-task --text "Press enter to confirm"
   --screen-stable 180 --timeout 3600`, then `capture` to see *why* it
   stopped: approval prompt → `send-keys codex-task Enter` (or the option
   key); question → answer with `send --submit`; done → collect results.
5. A human can `watch codex-task` live the whole time; the recording is
   the audit trail.

Approval-churn tip: when the hosted agent offers "don't ask again for
commands starting with …", prefer prefixes that will recur; ask the agent
to put one-off probe scripts in a file so a single prefix covers them.

## Gotchas

- **Detached capability synthesis**: with no attached client, the child's
  terminal queries (DA/DSR/modes) are answered by cleat's VT engine, not a
  real terminal. Programs that branch on capability detection can behave
  differently detached vs attached; recordings faithfully capture whichever
  actually happened.
- **Controller vs watcher**: one controller (input + resize) at a time;
  watchers are read-only and never affect the session. A watcher smaller
  than the session grid sees wrap artifacts (byte-tee limitation).
- **The passthrough engine is test-only.** A functional binary is built
  with `--features ghostty-vt`; `capture`/`wait --text` error on
  passthrough builds.
- **`--timeout` is seconds** (float), unlike the humantime durations most
  other flags take.
- `send` appends Enter by default — use `--no-enter` when composing
  multi-part input manually.
