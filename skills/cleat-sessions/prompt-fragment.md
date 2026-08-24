# cleat-sessions prompt fragment

System-prompt fragment for harnesses without a skill system (pi, codex).
Paste into the harness's instructions file (e.g. AGENTS.md). Keep in sync
with README.md in this directory (canonical).

---

## Terminal sessions via cleat

`cleat` hosts interactive/TUI programs in persistent, recorded terminal
sessions. Use it instead of backgrounding interactive programs or parsing
raw terminal escapes.

- Start/reuse: `cleat launch <id> --tag project=<repo> --tag
  purpose=<build|test|probe|agent> --size 200x50 --cwd <dir> --cmd "..."`
  (same id ⇒ reuses the live session; recording is on by default and
  survives `kill`).
- Read the screen: `cleat capture <id>` (rendered text, no escapes).
- Type: `cleat send <id> "text"` (adds Enter). Into a TUI composer
  (codex/claude prompt box) use `cleat send <id> --submit "..."` — plain
  text+Enter gets swallowed by paste heuristics.
- Wait: `cleat wait <id> --text "str" | --screen-stable 30 |
  --idle-time 5  --timeout 600` (conditions OR; `--timeout` is bare
  seconds; spinners defeat `--idle-time`, use `--screen-stable` for TUIs).
- State: `cleat inspect <id>`. `fg_pgid != leader_pid` means a child owns
  the foreground; equality is normal when an agent is the leader. A
  surviving session proves the leader is alive, so never infer "agent
  exited" from equality.
- Recorded output: `cleat mark <id> m1` … `cleat transcript <id>
  --since-marker m1`; `cleat expect <id> "text"` blocks until it's output.
- End: `cleat kill <id>` (recording preserved; `--purge` discards).
- Enumerate: `cleat list [--selector k=v]`.
- Tags are opaque strings, `key=value` by convention; never set
  `vessel=…` (reserved for flotilla).
- Humans can `cleat watch <id>` read-only while you drive.
- Remote work: host ssh in a local session — `cleat launch <id> --cmd
  "ssh -t <host> '<agent>'"` — every verb works on the local grid. The
  remote end is bare: recording lives locally, connection drop SIGHUPs
  the remote process, and relaunching replays history but starts a fresh
  ssh (use the hosted agent's own resume to recover).

Full guide: `skills/cleat-sessions/README.md` in the cleat repo.
