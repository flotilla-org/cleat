# Dogfood run: cleat hosts codex working on #78

Cleat hosts the agent that fixes cleat's staleness bug
([#78](https://github.com/flotilla-org/cleat/issues/78)). The driver (human or
Claude) supervises through the control plane; a human terminal watches the TUI
live once MVP watch lands.

## Shape

- One session per work item: `cleat launch codex-78 --record --size <cols>x<rows> --cmd "codex ..."`
- Supervision without attaching: `capture`, `wait --idle-time`, `expect`,
  `inspect` (foreground_pgid != leader_pgid = "codex is running something"),
  `send` / `send-keys` for prompts.
- Human check-ins: `cleat watch` (byte-tee MVP) from any terminal, incl. ssh.

## Prep (blocking, in order)

1. **`launch --size`** — initial grid geometry at launch. Blocking because at the
   default 80×24 a modern TUI doesn't just look cramped, it *behaves* differently
   (collapsed panels, different wrap points), which contaminates `expect` matching
   and the recording. Distinct from *pinned* geometry (freeze), which is not
   needed to start: nothing attaches as controller while driving via the control
   plane.
2. **Byte-tee MVP watch** — read-only attachments: replay payload on attach + the
   same output chunks the controller gets; input frames dropped. Known wart,
   accepted: a watcher terminal smaller than the session grid sees wrap artifacts
   (no client-side letterbox until the Update Packet transport). Scaffolding;
   will be replaced by the packet-based watcher.
3. Smoke test: codex under the ghostty engine, detached — launch, wait, capture,
   see a sane prompt.

## Pre-registered predictions

Check off as confirmed/refuted; surprises go in the friction log.

- [ ] **#78 staleness** felt constantly on attach/watch with a chatty codex TUI
  (macOS).
- [ ] **Second-attach 409** hit the first time someone peeks while attached
  elsewhere — measures the real value of watch/take-control. (Partially
  pre-empted by prep item 2.)
- [ ] **Dropped `CSI ? u` (kitty keyboard) queries while detached** cause codex to
  behave differently detached vs attached — the README's known asymmetry biting a
  real workload.
- [ ] **Geometry churn**: reflow on each differently-sized attach is the first
  annoyance felt; argues pinned geometry earlier than planned.
- [ ] **`cleat list` across many codex sessions** is where the flat
  one-daemon-per-session model hurts first — the concrete pull toward tags +
  directory (ADR 0004 world).

## Friction log

*(append during the run: timestamp, what happened, what it implies)*

- **2026-07-04** Prep 1 and 2 merged (#81 → `78cba35`, #82 → `ae135c6`, both
  codex-authored). Reviewed: `--size` plumbs coherently to PTY winsize, VT engine
  creation, ConPTY, and both provider FFI paths; watch matches the issue spec
  (replay without resize, watcher input/resize ignored, drop-on-backlog,
  inspect roles, list status counts controllers only). Verified live:
  `launch --size 120x40` → `inspect` reports `terminal 120x40`; watcher receives
  replay payload; `attachments: watcher` in inspect.
- **2026-07-04** `wait --timeout 5s` rejected — timeout is a bare float (seconds),
  while transcript's `--until-idle` accepts humane durations. Agents *will* type
  `5s`. Small CLI inconsistency worth unifying.
- **2026-07-04** `recreation_seeds_scrollback_from_prior_recording` is flaky under
  parallel test load (times out spawning a real PTY; passes 3/3 in isolation).
  Pre-existing, not a #81/#82 regression.
- **2026-07-04** Byte-tee watch nits (acceptable scaffolding, all subsumed by the
  progress/servicing split): watchers are not in the poll fd set, so disconnect
  and backlog detection ride the ≤100 ms tick; the watch upgrade handler can
  attempt to write an HTTP 500 onto an already-upgraded stream on enqueue error;
  `--size` on an already-running session id is silently ignored (reuse path).

- **2026-07-04 (run live)** Codex TUI renders cleanly detached under the ghostty
  engine at 200×50; recording active; human watching via `cleat watch` while the
  driver supervises through the control plane. The multi-client shape works.
- **2026-07-04 (run live)** `cleat send codex-78 "<task>"` left the text sitting
  in codex's composer unsubmitted — the trailing Enter was swallowed (likely
  codex's paste heuristic treating fast text+newline as one paste). A separate
  `send-keys codex-78 Enter` submitted it. Driving modern TUI composers needs
  either a delay between text and Enter or a paste-encoded send followed by a
  distinct Enter; consider a `send --submit` that does text-as-paste + delayed
  Enter.

- **2026-07-04 (run live, watcher report)** Watch initially "over drew" the
  watcher's terminal: the watch handler sends the replay payload with no
  clear+home first (the reattach path has `REATTACH_CLEAR_SEQUENCE`; watch skips
  it), and the payload assumes a 200×50 canvas the watcher may not have. Cheap
  scaffolding fix: always prepend clear+home for watchers. The real fix is the
  packet-based watcher — a byte-tee paints someone else's escape stream into the
  watcher's current screen state and can never own cursor/alt-screen/viewport
  reconciliation. First concrete evidence for the Update Packet destination.
- **2026-07-04 (run live, watcher report)** Prediction 1 counter-evidence: watch
  is *not* perceptibly laggy while codex streams. The #78 staleness may be
  conditional (controller-attach path? specific load shapes?) rather than
  constant — matches the decision to instrument before fixing.

- **2026-07-04 (run live)** `wait --idle-time` is defeated by TUI spinners: codex
  redraws its "Working (Nm Ns)" timer every second, so PTY output never goes
  quiet and a 45 s idle monitor sat blind for 40 minutes *through an approval
  prompt*. Also `inspect`'s `foreground_pgid != leader_pgid` signal is useless
  for TUI agents (the TUI is always the foreground process). Workaround today:
  `wait --text "Press enter to confirm" --idle-time 60` (OR semantics) to catch
  approval prompts by text. Real need: a "screen stable" / semantic-prompt wait
  condition -- the VT engine already has row-level dirty tracking and ghostty
  exposes semantic prompt state per row, so both are buildable.

- **2026-07-04 (run complete)** Codex finished #78: instrumented first as asked,
  **could not reproduce a Darwin poll() miss** (burst, paced, and
  watcher-attached workloads) — prediction/hypothesis 1 unconfirmed. Landed the
  kqueue/epoll readiness primitive anyway (the split needs it) as PR #88, honest
  commit message, all four validation gates green on macOS. #78 stays open; the
  staleness may live in the attach path or a load shape not reproduced.
- **2026-07-04 (run complete)** Driving-codex friction summary: composer swallows
  fast text+Enter (use send --no-enter, pause, send-keys Enter); TUI spinner
  defeats wait --idle-time (use wait --text on the approval-prompt string, OR'd
  with idle); worktrees don't share .tools so agents try to rebuild ghostty
  (point CLEAT_GHOSTTY_PREFIX at the main checkout); codex sandbox blocks UDS
  binds in tests (needs an unsandboxed approval). All workable today; the first
  two argue for a send --submit flag and a screen-stable/semantic-prompt wait
  condition.

- **2026-07-04 (shepherding phase)** Codex shepherded its own PR #88 from inside
  the cleat session ($pr-shepherd): investigated Linux CI failures it could not
  reproduce locally and fixed **three real epoll bugs** (EINTR from SIGCHLD
  interrupting epoll_wait, HUP/ERR misreported as PTY read readiness, client
  writability semantics), de-flaked its own regression test, rode out an
  unrelated Ghostty VT flake with a rerun, and reported merge-readiness without
  merging. The review-loop pattern (bot review -> fix -> re-verify) also worked
  on the docs PR #87: three real CONTEXT.md ambiguities found and resolved, with
  one reviewer inference correctly pushed back on (sticky primary). Strongest
  datapoint of the run: the full author->CI->review->merge-ready loop is
  workable with the agent hosted in a cleat session.

- **2026-07-04 (retirement)** `cleat kill codex-78` **deleted the active
  recording** — the daemon preserved the session dir (recorder present) but
  `SessionService::kill` unconditionally `remove_session`s it afterwards. The
  full record of the run is gone; filed as #92. Also found ~30 empty
  `session-<uuid>` husks and a crashed session dir (stale socket, no pid file)
  that `list` reports as an error forever. Painful but exactly the kind of
  lifecycle hole dogfooding exists to find: the run's most valuable artifact was
  destroyed by the run's final command.

## Issues spawned

*(label: `dogfood`)*

- [#79](https://github.com/flotilla-org/cleat/issues/79) — `launch --size` (prep 1)
- [#80](https://github.com/flotilla-org/cleat/issues/80) — `cleat watch` byte-tee MVP (prep 2)
- [#88](https://github.com/flotilla-org/cleat/pull/88) — kqueue/epoll readiness (the run's output; #78 stays open)
- [#89](https://github.com/flotilla-org/cleat/issues/89) — `send --submit` (composer swallows fast text+Enter)
- [#90](https://github.com/flotilla-org/cleat/issues/90) — `wait --screen-stable` / `--at-prompt` (spinners defeat idle)
- [#92](https://github.com/flotilla-org/cleat/issues/92) — `kill` deletes active recordings (found at retirement; the run's recording is lost)
