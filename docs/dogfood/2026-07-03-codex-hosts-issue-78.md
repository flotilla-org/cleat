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

## Issues spawned

*(label: `dogfood`)*

- [#79](https://github.com/flotilla-org/cleat/issues/79) — `launch --size` (prep 1)
- [#80](https://github.com/flotilla-org/cleat/issues/80) — `cleat watch` byte-tee MVP (prep 2)
