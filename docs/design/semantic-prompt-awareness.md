# Semantic prompt awareness for send and wait

Research slice of [cleat#306](https://github.com/flotilla-org/cleat/issues/306), 2026-10-05. No CLI flags or runtime features are implemented by this change.

The structural selector and recognizer direction is recorded in
[Selectors and recognizers](../specs/2026-03-23-terminal-screen-introspection.md#selectors-and-recognizers).

## Recommendation

Do not use OSC 133 alone to authorize delivery into today's agent composers. Claude Code screen-reader mode emits turn-navigation marks, but our probe found no input-start B. Codex emitted no semantic marks in either tested mode. First ship a daemon-owned controller-activity fence and atomic submit transaction as a heuristic mitigation for [flotilla#2614](https://github.com/flotilla-org/flotilla/issues/2614). Exact draft safety additionally requires a verified producer/editor contract; quiet time cannot detect an abandoned draft. The flotilla classifier in [flotilla#2602](https://github.com/flotilla-org/flotilla/issues/2602) remains outside this work.

## Q1: measured producers

Six detached, recorded cleat/Ghostty sessions used 120×40 grids. Versions: Claude Code 2.1.287 and Codex CLI 0.160.0. Each agent received `Reply exactly OK. Do not use tools.`, completed with `OK`, and the normal/AX Claude and both Codex sessions then held an unsubmitted `draft probe`. Claude tools were disabled with `--tools ""`; Codex used `--no-daemon` to isolate the probe. Initial Claude onboarding-only recordings were excluded; a disposable config with completed onboarding and injected OAuth credentials enabled real turns. No credentials are in the recordings.

| Producer / mode | OSC 133 payloads observed | Interpretation |
|---|---|---|
| Claude, normal | none | No marked live composer in this probe |
| Claude, `--ax-screen-reader` | `A;redraw=0`, `C`, `D` | Turn navigation; **no B**, no fresh A/B around the post-turn draft |
| Claude, AX with `TERM_PROGRAM=WezTerm`, `TERM=xterm-256color` | none | Confirms documented terminal-dependent suppression |
| Codex, normal | none | No semantic input evidence |
| Codex, `--no-alt-screen` | none | Inline mode does not enable semantic marks |
| Bash 5.2, explicit integration | C, C, D;0, A, B | Positive control: scanner observes both prompt and input marks |

Cleat sets engine-owned `TERM_PROGRAM=ghostty`, truecolor, and `TERM=xterm-ghostty` when terminfo is available (otherwise xterm-256color): [identity implementation](https://github.com/flotilla-org/cleat/blob/00c072b207dc943f6c93fe3b6b09abaa257695a6/crates/cleat/src/terminal_identity.rs#L19). These probes used the installed functional Ghostty engine; the WezTerm row explicitly overrode identity. Attached-terminal behavior and other versions are not established by this experiment.

AX's A occurred immediately before the submitted `you:` transcript. C and D arrived together near completion. Thus neither the documentation's “turn boundaries” nor the observed marks promise B→C live input tracking. The operator's [partial answer](https://github.com/flotilla-org/cleat/issues/306#issuecomment-5985539236) correctly identifies accessibility mode as relevant. The [vendor accessibility documentation](https://code.claude.com/docs/en/accessibility) describes `--ax-screen-reader`, `CLAUDE_AX_SCREEN_READER=1`, and `axScreenReader: true`, caret-preserving rendering, turn markers, and WezTerm suppression. We tested the flag rather than all equivalent configuration forms. Reduced motion or bells are useful activity signals but do not establish an empty draft.

Codex's [official configuration reference](https://developers.openai.com/codex/config-reference/) did not establish a supported OSC133 enable switch. At release [rust-v0.160.0 / a956835d](https://github.com/openai/codex/tree/a956835d020762cb2b570053af06f643a11c0ecc), `rg -n -i 'osc.?133|semantic.?prompt|133;'` over normal source excluding lockfiles returned no matches. Its [inline flag](https://github.com/openai/codex/blob/a956835d020762cb2b570053af06f643a11c0ecc/codex-rs/tui/src/cli.rs#L77) changes screen buffering; [screen-reader detection](https://github.com/openai/codex/blob/a956835d020762cb2b570053af06f643a11c0ecc/codex-rs/tui/src/screen_reader.rs#L1) affects animations, with no screen-reader CLI flag found. This bounded source search corroborates runtime absence, not every future release or dependency. [Codex#23652](https://github.com/openai/codex/issues/23652) mentions OSC133 click extensions as a request, not shipped composer lifecycle output.

Historical Claude requests [#1465](https://github.com/anthropics/claude-code/issues/1465) and [#26235](https://github.com/anthropics/claude-code/issues/26235) were closed by inactivity housekeeping. [#32635's author](https://github.com/anthropics/claude-code/issues/32635#issuecomment-4027545111) closed it as a duplicate, so its `completed` state is not evidence of implementation. Request live composer A/B and execution C/D upstream, together with redraw/continuation guarantees; do not infer support from issue closure.

Shell integrations emit these marks when installed/enabled; [Ghostty's shell integration](https://ghostty.org/docs/features/shell-integration) supports bash, zsh and fish. Wrapping an unmarked TUI in a marked shell reports the shell's running command, not the nested editor's draft.

### Reproduction and evidence

Launch with `cleat launch ID --tag project=cleat --tag purpose=probe --size 120x40 --cwd SCRATCH --cmd PROGRAM`; record by default. Capture readiness, submit the harmless prompt with `cleat send ID --submit ...`, wait for actual `OK`, then `cleat send ID --no-enter 'draft probe'`. Preserve the cast before killing the session. For Claude pass injected credentials to the child environment without logging them; a pre-existing daemon may not inherit the caller's credential environment.

For a Bash positive control, set `PS1=$'\e]133;A\a$ \e]133;B\a'`, `trap 'printf "\033]133;C\007"' DEBUG`, and `PROMPT_COMMAND='printf "\033]133;D;0\007"'`. This is deliberately a synthetic integration, not production shell integration: DEBUG also marks PROMPT_COMMAND and D uses a constant status. It demonstrates mark transport and an unsubmitted input, not shell exit-status correctness. The echoed command contains `D;\r;0`: the line editor inserts a carriage return at the terminal wrap in its display echo. That is distinct from the executed OSC output, which the scanner records as `D;0`. The decoded cast preserves both without normalising away the display control byte.

[runtime-results.json](semantic-prompt-evidence/runtime-results.json) records SHA-256, output sizes, marks and escaped local context. The [scanner](semantic-prompt-evidence/scan-casts.py) and all six full `.cast` files are also committed beside that JSON, so the evidence can be rechecked after artifact expiry. Recording bundle: `artifact/artifact-d196bf72de313a78cf7ba1ae5d9d2889670ce4e68e29e3cc2a44045d3c351c1a` (six casts and scanner). The scanner concatenates decoded output events before matching `ESC ] 133 ; payload BEL/ST`, so marks split across PTY reads are counted. Absence is meaningful only for the tested turn/draft window. Recordings are retained in cleat after kill as well; artifact storage has shorter retention than this note.

## Q2: pinned Ghostty surface

### Existing surface

- `ghostty_terminal_get(terminal, GHOSTTY_TERMINAL_DATA_CURSOR_AT_PROMPT /*39*/, bool*)` already exists. It returns false on the alternate screen and otherwise uses the cursor row's prompt flag first, then cursor semantic mode. [Header](https://github.com/rjwittams/ghostty/blob/c361de9691f006f65c400be73896d1e48a8ec56c/include/ghostty/vt/terminal.h#L1963-L1972), [implementation](https://github.com/rjwittams/ghostty/blob/c361de9691f006f65c400be73896d1e48a8ec56c/src/terminal/c/terminal.zig#L1790), [predicate](https://github.com/rjwittams/ghostty/blob/c361de9691f006f65c400be73896d1e48a8ec56c/src/terminal/Terminal.zig#L2234-L2254).
- `ghostty_row_get(..., GHOSTTY_ROW_DATA_SEMANTIC_PROMPT, ...)` distinguishes None, Prompt, PromptContinuation. [Header](https://github.com/rjwittams/ghostty/blob/c361de9691f006f65c400be73896d1e48a8ec56c/include/ghostty/vt/screen.h#L231-L249).
- `ghostty_cell_get(..., GHOSTTY_CELL_DATA_SEMANTIC_CONTENT /*9*/, ...)` distinguishes Output=0, Input=1, Prompt=2. Thus the C API **does distinguish input cells**, contrary to the issue snapshot's row-only premise. [Header](https://github.com/rjwittams/ghostty/blob/c361de9691f006f65c400be73896d1e48a8ec56c/include/ghostty/vt/screen.h#L118-L135), [getter](https://github.com/rjwittams/ghostty/blob/c361de9691f006f65c400be73896d1e48a8ec56c/src/terminal/c/cell.zig#L166).
- Cleat already reads the cell semantic value, maps Input to `1`, and carries it in `TerminalCell.semantic` and `TerminalRenderStyle.semantic`. It carries row semantics too. Its Rust `GhosttyTerminalData` does **not** bind `CursorAtPrompt` yet. Pinned references: [vt/ghostty_ffi.rs:404](https://github.com/flotilla-org/cleat/blob/00c072b207dc943f6c93fe3b6b09abaa257695a6/crates/cleat/src/vt/ghostty_ffi.rs#L404), [vt/ghostty_ffi.rs:2183](https://github.com/flotilla-org/cleat/blob/00c072b207dc943f6c93fe3b6b09abaa257695a6/crates/cleat/src/vt/ghostty_ffi.rs#L2183), [vt/ghostty_ffi.rs:2244](https://github.com/flotilla-org/cleat/blob/00c072b207dc943f6c93fe3b6b09abaa257695a6/crates/cleat/src/vt/ghostty_ffi.rs#L2244), [vt/ghostty.rs:1031](https://github.com/flotilla-org/cleat/blob/00c072b207dc943f6c93fe3b6b09abaa257695a6/crates/cleat/src/vt/ghostty.rs#L1031), [vt/ghostty.rs:1111](https://github.com/flotilla-org/cleat/blob/00c072b207dc943f6c93fe3b6b09abaa257695a6/crates/cleat/src/vt/ghostty.rs#L1111), [provider.rs:112](https://github.com/flotilla-org/cleat/blob/00c072b207dc943f6c93fe3b6b09abaa257695a6/crates/cleat/src/provider.rs#L112), [provider.rs:467](https://github.com/flotilla-org/cleat/blob/00c072b207dc943f6c93fe3b6b09abaa257695a6/crates/cleat/src/provider.rs#L467). Bind the existing terminal data selector for the basic wait proof; a fork bump is unnecessary for that limited surface.

### What internal state means

OSC A sets cursor semantic mode to prompt and marks the row. B changes cursor mode to input; C changes it to output; D also changes it to output. The parser can parse D's exit status, but `Terminal.semanticPrompt` does not store it or track a running command. No B/C cursor positions or active-input span are retained in `Screen.SemanticPrompt`; it holds `seen` and click behavior. `seen` is set on a prompt-kind transition, not on every possible OSC133 action. [Transition handling](https://github.com/rjwittams/ghostty/blob/c361de9691f006f65c400be73896d1e48a8ec56c/src/terminal/Terminal.zig#L2140-L2200), [screen state](https://github.com/rjwittams/ghostty/blob/c361de9691f006f65c400be73896d1e48a8ec56c/src/terminal/Screen.zig#L99-L123), [cursor accounting](https://github.com/rjwittams/ghostty/blob/c361de9691f006f65c400be73896d1e48a8ec56c/src/terminal/Screen.zig#L2833-L2872).

### Limitations for exact draft safety

1. Existing false conflates absent integration, command output, and alternate-screen suspension. Cleat should separately expose unsupported/unknown rather than interpreting false as an empty composer.
2. Existing true is a navigation-oriented heuristic, **not** proof that no command is running. A/C on the same row with C at a nonzero column leaves the row prompt flag set, so `cursorIsAtPrompt()` can stay true after C. D does not clear that row flag either. A future semantic wait requiring no command running needs an explicit phase or a predicate based on it; don't silently promise stronger semantics for selector 39.
3. Input-tagged cells permit a conservative display-text proof, but not an exact current editable buffer. Historical input cells can remain after C/D; an empty input has no printed cells; editing may leave stale blank/input tags; cursor position can be in the middle of a draft; right and continuation prompts interrupt an input display. Tracking B alone does not prove where a full draft ends.
4. OSC133 reports semantic display boundaries, not a program's private editor buffer. Passwords, hidden text, tab expansion, multiline rendering and text outside retained scrollback cannot be reconstructed exactly from cells. Any generic `input_text` is observed display text, with an explicit completeness indicator; stash needs a harness/editor contract beyond this FFI.
5. Terminal inspection and PTY typing are different operations. A stable semantic snapshot does not atomically exclude a controller's input; a cleat delivery transaction must serialize accepted controller writes and automated send and recheck the generation immediately before writing.


### Evidence and existing seams

- [FinalTerm semantics in iTerm2 documentation](https://iterm2.com/3.6/documentation-one-page.html#shell-integrationfinalterm): A precedes prompt, B follows prompt and precedes user command, C precedes execution, D ends execution. [VS Code's supported 133 protocol](https://code.visualstudio.com/docs/terminal/shell-integration#supported-escape-sequences) confirms these meanings and notes reduced information compared with 633. Neither these marks nor terminal cells expose an application's editable buffer or atomic draft editing API. Inferring editable text from rendered cells is a derived capability, not guaranteed by OSC 133.
- Existing row API has numeric `semantic_prompt`, wrapping, cells and graphemes ([provider.rs lines 80–94](https://github.com/flotilla-org/cleat/blob/00c072b207dc943f6c93fe3b6b09abaa257695a6/crates/cleat/src/provider.rs#L80)). A row flag alone does not identify B's column or full logical input extent.
- Existing inspect describes session/terminal/process/attachments/recording and activity; it has no screen argument or draft fields ([protocol.rs lines 99–138](https://github.com/flotilla-org/cleat/blob/00c072b207dc943f6c93fe3b6b09abaa257695a6/crates/cleat/src/protocol.rs#L99), [cli.rs line 425](https://github.com/flotilla-org/cleat/blob/00c072b207dc943f6c93fe3b6b09abaa257695a6/crates/cleat/src/cli.rs#L425)).
- Wait conditions are explicitly OR, with ready=0, timeout=1, error/session exit=2; existing matching loops likewise return on any condition ([cli.rs line 489](https://github.com/flotilla-org/cleat/blob/00c072b207dc943f6c93fe3b6b09abaa257695a6/crates/cleat/src/cli.rs#L489), [session.rs line 2986](https://github.com/flotilla-org/cleat/blob/00c072b207dc943f6c93fe3b6b09abaa257695a6/crates/cleat/src/session.rs#L2986)).
- `send --submit` sends Paste, sleeps, then sends Enter separately ([cli.rs line 1205](https://github.com/flotilla-org/cleat/blob/00c072b207dc943f6c93fe3b6b09abaa257695a6/crates/cleat/src/cli.rs#L1205)). HTTP input accepts Text/Paste/Key/RawBytes ([session.rs line 3778](https://github.com/flotilla-org/cleat/blob/00c072b207dc943f6c93fe3b6b09abaa257695a6/crates/cleat/src/session.rs#L3778)).
- Actor commands serialize writes/key/paste operations, providing the natural guard seam ([host/actor.rs line 1456](https://github.com/flotilla-org/cleat/blob/00c072b207dc943f6c93fe3b6b09abaa257695a6/crates/cleat/src/host/actor.rs#L1456)). Raw stream input enters at [session.rs:2901](https://github.com/flotilla-org/cleat/blob/00c072b207dc943f6c93fe3b6b09abaa257695a6/crates/cleat/src/session.rs#L2901); packet controller keyboard/text/paste/raw events at [session.rs:4685](https://github.com/flotilla-org/cleat/blob/00c072b207dc943f6c93fe3b6b09abaa257695a6/crates/cleat/src/session.rs#L4685). Input has source identities in packet paths; HTTP automation is distinguishable by route. No last-controller-input timestamp appears in these inspected command paths or inspect fields.

## Q3: query and wait proposal

First establish a producer contract: A/C/D used only to annotate submitted turns must remain unknown for composer readiness. A/B/C/D display phases alone are insufficient if an emitter does not delimit its live editor.

Expose an optional `screen.prompt` object through `inspect --screen --json`:
```json
{"capability":"osc133","status":"input","at_prompt":true,"input_text":"","input_empty":true,"text_complete":true,"generation":42}
```
Capability values: `unsupported` (engine cannot observe semantics), `unobserved` (engine supports them but no valid live marks), `osc133`. Status: unknown/prompt/input/running. Use nullable booleans/text for unknown information. At prompt is true in the current A→B prompt or B→C input phase, with a live cursor in that region and no command running; it says nothing about emptiness. D→A is between commands, not a prompt. During A→B, input has not been delimited: input_empty stays null. Active alternate-screen entry must invalidate shell context unless fresh marks establish application-local context. Screen reset, lost anchors/history, inconsistent sequences, and unknown foreground context must invalidate stale positive assertions.

Extract complete visible logical input from B to its complete current extent, not B-to-cursor: moving cursor left must not hide suffix text. Soft wraps join without newline; genuine multiline input requires continuation-aware reconstruction and prompt exclusion. Preserve whitespace: a space is input. Graphemes, wide cells and combining text must survive. If clipping/truncation/ambiguous continuation prevents complete reconstruction, text_complete=false and input_empty=null. OSC 133 cannot prove hidden/password buffers or application's actual editing state; describe input_text as reconstructed display text.

`wait --at-prompt` is level-triggered, immediately succeeds if established true, and retains existing OR semantics alongside other conditions. Absence of marks waits to timeout; unsupported no-VT engine returns error=2 immediately for an explicitly requested semantic condition. This preserves optional Ghostty and honestly reports unavailable capability. A wait result is advisory, never authorization for subsequent unguarded send; state can change immediately afterward.

## Q4: guarded send, races and controller activity

The first controller-activity/transaction slice is described in [Controller input and daemon submission](../controller-input-and-submit.md), including conservative echo fencing, bounded replay and refusal behavior.

Propose `send --if-input-empty` as immediate refusal by default. Success requires established live input phase, complete input reconstruction, input_empty=true and no running command. Busy/nonempty returns a documented refusal exit status; unknown/unsupported returns error with structured reason. Optional waiting belongs in explicit `--wait-input-empty --timeout ...`, with predicate and write still checked in the daemon at the eventual write point. Preserve unguarded legacy send for callers omitting these options.

Add one actor-owned guarded submission request, including paste and delayed Enter. Serialize predicate check and initial input emission with controller events, and retain transaction ownership through Enter. CLI inspect→send cannot close the race, and two independent guarded requests are insufficient because the pasted content makes input nonempty before Enter. Define handling of controller input during transaction explicitly (prefer abort before initial write; once begun, bounded queue with lossless replay after submission, or explicit busy rejection rather than silent discard). Pump pending output before deciding, track controller-input generation and unacknowledged controller input: controller keystrokes may have reached PTY but not yet appeared in terminal echo. A controller event accepted before the check must conservatively inhibit a send until observed input catches up. Terminal output from uncontrolled external writers means actor atomicity is against cleat-mediated input only.

Propose `last_controller_input_at` and monotonic `controller_input_generation` separate from automation. Count accepted controller text, paste, key presses and raw bytes, not watch events or resize. Raw terminal responses may share input channel, so document possible false positives. `--controller-idle N` can optionally require quiet time in the same transaction. It is a heuristic: long-abandoned drafts remain nonempty; attached identity is not proof of humanity. Keep PTY output-idle unchanged.

### Stash mode

Defer generic `--stash-input`: B/C delimit displayed regions, not a writable application buffer. Ctrl-U/Home/End mean different things in readline, vi modes and TUI editors; hidden text, suffix, cursor, selection, multiline and history state may be lost. Safe stash requires an explicit application editing adapter/API, complete draft+cursor snapshot, clear confirmation, delivery acknowledgement, a fresh ready input generation, and restoration conditional on no new controller edits. Submission may switch buffers or never return. Persist original draft and transaction state on failure and never overwrite new user edits; cancel/timeout/exit must offer recoverable original text. Do not claim safe stash from OSC 133 alone.

## Q5: fallback and first unblock

No-option sends retain current behavior. Semantic query returns unknown/unobserved or unsupported; semantic guards fail closed and never silently become pixel heuristics. Existing flotilla classifier remains its independent fallback. Shell OSC133 wrappers around a TUI only describe the parent shell's C→D running command and cannot identify its nested composer. Therefore shell-only semantic support cannot fix flotilla#2614 for an unmarked agent composer.


## Validation

### Executed small proof

The standalone [C proof](semantic-prompt-evidence/ghostty-proof.c) reads existing C cell semantics and cursor-at-prompt after synthetic A/B, draft, same-row C/D, and alternate-screen entry. Initially run against an installed library, it was rerun after building the exact clean scratch checkout `c361de9691f006f65c400be73896d1e48a8ec56c` using Zig 0.16.0 and the flags from `tools/ghostty-toolchain.toml`: `zig build -Demit-lib-vt=true -Dsimd=true -Doptimize=ReleaseSafe --prefix /tmp/osc133-ghostty-install`. Both runs agree with [observed output](semantic-prompt-evidence/ghostty-proof-output.txt):

```text
initial at_prompt=0 cells=000000
empty-B at_prompt=1 cells=220000
draft at_prompt=1 cells=221111
C-same-row at_prompt=1 cells=221111
D-same-row at_prompt=1 cells=221111
alternate at_prompt=0 cells=000000
```

Cells use Output=0, Input=1, Prompt=2. This proves why a row-first getter cannot implement the stronger wait semantics unmodified. It is a C API experiment, not a new Rust feature or a test of an actual shell editor. Reproduce against that prepared prefix with `cc -I "$OSC133_PREFIX/include" ghostty-proof.c "$OSC133_PREFIX/lib/libghostty-vt.a" -lm -lpthread -ldl -o /tmp/ghostty-proof` then `/tmp/ghostty-proof`. The helper’s pin/version/build flags were used directly in scratch to avoid an embedded repository in the vessel checkout.

`cargo check --workspace --locked --no-default-features` passed. Today's Cargo default actually enables ghostty-vt; the brief's “Rust-only default” means the supported no-VT build, which remains unchanged. A plain `cargo check --workspace --locked` failed before checking cleat because the repo-local Ghostty install prefix was not prepared; the pinned Ghostty build and proof were subsequently run in scratch as described above. `CLEAT_GHOSTTY_PREFIX=/tmp/osc133-ghostty-install cargo check --workspace --locked` also passed after preparation. The no-VT path must report semantic query/guard capability as unsupported, never guessed empty. No Rust source, dependencies, feature defaults or fork pins changed.

## Fork handoff

Reliable phase/position queries need additions beyond the existing navigation getter. Parked proposal: `artifact/artifact-6fe767a107399f3c7bb0af512e0391e7f29acd8fa32e0b3c31d5774a3d7dceb5`, targeting `rjwittams/ghostty@c361de9691f006f65c400be73896d1e48a8ec56c`. It specifies an additive per-screen phase/epoch query and tracked B input-start coordinate, invalidation and ABI validation. No push to that out-of-scope fork was attempted. The additions still cannot expose hidden editor buffers or make generic stash safe.

### Existing functionality to preserve

Keep `GHOSTTY_TERMINAL_DATA_CURSOR_AT_PROMPT=39` and its existing navigation heuristic unchanged for compatibility. Cleat can bind it without a fork change, but must not treat it as a command-running or editable-input guarantee. Existing cell Input/Prompt/Output and row prompt flags stay intact. [Existing header](https://github.com/rjwittams/ghostty/blob/c361de9691f006f65c400be73896d1e48a8ec56c/include/ghostty/vt/terminal.h#L1963-L1972), [predicate](https://github.com/rjwittams/ghostty/blob/c361de9691f006f65c400be73896d1e48a8ec56c/src/terminal/Terminal.zig#L2234-L2254).

### Patch A: explicit phase, independently useful

Add an additive query to `include/ghostty/vt/terminal.h` and `src/terminal/c/terminal.zig`, with the next unused selector after 40 (confirm the destination head before assigning):

```c
typedef enum {
  GHOSTTY_SEMANTIC_PHASE_UNKNOWN = 0,
  GHOSTTY_SEMANTIC_PHASE_PROMPT = 1,
  GHOSTTY_SEMANTIC_PHASE_INPUT = 2,
  GHOSTTY_SEMANTIC_PHASE_COMMAND = 3,
  GHOSTTY_SEMANTIC_PHASE_OUTPUT = 4,
} GhosttySemanticPhase;

typedef struct {
  uint64_t epoch;
  bool markers_seen;
  bool alternate_screen;
  GhosttySemanticPhase phase;
  bool input_start_valid;
} GhosttySemanticPromptState;
/* ghostty_terminal_get(..., GHOSTTY_TERMINAL_DATA_SEMANTIC_PROMPT_STATE, &state) */
```

Maintain per-screen state in `Screen.SemanticPrompt` (currently only seen/click). Hook `Terminal.semanticPrompt` transitions: A/new primary prompt starts a new epoch and Prompt; B sets Input; C sets Command; D sets Output and may retain an optional exit code in a later patch. `markers_seen` must be set for every recognized semantic action, independent of existing `seen`, which is an optimization for prompt tagging. Keep alternate-screen state independent and report it explicitly; normal-screen markers cannot authorize an alternate-screen composer. Reset clears state; resize/reflow preserves phase; screen switching reports the newly active screen. Expose Unknown where sequence history cannot establish a phase. P/right/continuation prompt actions must not spuriously clear an active command or reset the primary epoch; implement and test them explicitly rather than relying solely on cursor tag changes. [Transitions to patch](https://github.com/rjwittams/ghostty/blob/c361de9691f006f65c400be73896d1e48a8ec56c/src/terminal/Terminal.zig#L2084-L2200), [state to extend](https://github.com/rjwittams/ghostty/blob/c361de9691f006f65c400be73896d1e48a8ec56c/src/terminal/Screen.zig#L99-L123).

A phase query proves observed protocol state only. It does not prove that an arbitrary emitter uses these marks for an editable composer. The producer contract remains part of cleat's capability assessment.

### Patch B: tracked input start

On B and corresponding supported input-start action, retain a tracked page-list pin at the cursor. Replace/release it on a new input start, A/new primary prompt, C, D, reset and teardown. A/right-side or continuation prompt can briefly interrupt input rendering without invalidating the primary input start. Return the input-start location in requested coordinate space through an additive function:

```c
GhosttyResult ghostty_terminal_semantic_input_start(
    GhosttyTerminal terminal,
    GhosttyPointTag coordinate_space,
    GhosttyPointCoordinate *out);
```

Return `GHOSTTY_NO_VALUE` for Unknown/Prompt/Command/Output phases, unsupported screen, discarded anchor, or coordinates outside the requested space. Never return a borrowed tracked-ref handle owned by the terminal. Read the coordinate in the same synchronous inspection transaction as the phase; callers serialize terminal mutations. Reuse the fork's tracked-reference/page-pin accounting and invalidation behavior for reflow/pruning rather than storing raw row numbers. [Existing tracking API](https://github.com/rjwittams/ghostty/blob/c361de9691f006f65c400be73896d1e48a8ec56c/include/ghostty/vt/terminal.h#L2388-L2417), [tracked-reference guarantees](https://github.com/rjwittams/ghostty/blob/c361de9691f006f65c400be73896d1e48a8ec56c/include/ghostty/vt/grid_ref.h#L49-L90).

Do not claim a B anchor is a full input region: the cursor may be inside the draft, and semantic display tags may include stale or hidden cells. A later display-span/text query can return `complete=false` when there is no reliable end boundary, hard multiline separators, offscreen/discarded content, masked input, or unsupported redraw behavior. Empty must mean known zero-length editable display, not blank after whitespace trimming. A literal-space draft is nonempty. No generic lossless stash API belongs in this patch.

### Required behavioral validation for owning-project implementation

- A/B/C/D phase transitions, BEL and ST terminators, fragmented writes, C and D on the same prompt row.
- Empty B without printed cells; draft with literal spaces; cursor moved left into draft; continuation/right prompts; shell redraw replacing a draft.
- No markers, incomplete/misordered markers, P/input-start variants, reset, normal/alternate transitions.
- Anchors follow wraps, scrolling and resize/reflow, invalidate on pruning/reset, and release without leaks after repeated prompt cycles.
- Preserve selector 39 behavior and existing C enum values; C ABI compile test and existing libghostty-vt tests.

Cleat follow-up: feature-gated Rust binding for Patch A/B, typed provider capability/state, conservative query fields; daemon serializes controller writes and conditional send. The no-VT build reports unsupported and retains current unconditional send behavior. Stash needs a separate harness-specific editing protocol and failure-recovery design.

## Dispatchable implementation slices

1. [#307 — Controller activity and atomic submit](https://github.com/flotilla-org/cleat/issues/307): **first practical mitigation for flotilla#2614** without agent marks; heuristic only, with consumer adoption separate.
2. [#308 — Verified semantic provider phase/origin](https://github.com/flotilla-org/cleat/issues/308): coordinate fork handoff, invalidate stale context, preserve optional VT behavior.
3. [#309 — Query and wait surface](https://github.com/flotilla-org/cleat/issues/309): depends on #308; nullable fields, conservative display reconstruction, retained OR waits.
4. [#310 — Atomic empty-input guard](https://github.com/flotilla-org/cleat/issues/310): depends on #307–#309; first exact safeguard for a verified marked composer, **not an exact fix for today's measured agent TUIs**.
5. [#311 — Editing-adapter stash research](https://github.com/flotilla-org/cleat/issues/311): require an editor contract and recoverable draft before exposing stash.

Upstream live-composer support is an external dependency for exact agent draft detection. #307 can reduce immediate collisions but cannot guarantee protection for old drafts; no cleat-only OSC133 slice demonstrated here fully resolves flotilla#2614 across both current agents.
