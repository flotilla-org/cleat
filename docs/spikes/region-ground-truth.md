# Spike: region ground truth from TUI frameworks (#319)

Branch `spike/region-ground-truth`. Spike code lives in
`spikes/region-ground-truth/`. The cast parser is the example
`crates/cleat/examples/region_cast.rs`.

## Answer

Instrumenting a framework so that each frame carries exact widget rectangles
costs very little in the two frameworks that matter:

- **ratatui:** about 20 changed lines in `ratatui-core` and one line in
  `ratatui-widgets`, applied through `[patch.crates-io]`. The demo app is
  unchanged ratatui code.
- **Ink:** about 50 lines of JavaScript that wrap the Ink instance's
  log-update function and walk the Yoga tree. Ink itself is not modified.
- **Claude Code** (closed and Bun-compiled): a `BUN_OPTIONS="--preload …"`
  script of about 90 lines gets at the forked Ink tree without touching the
  binary. `NODE_OPTIONS` has no effect on it.

All three emit a draft `OSC 7701` snapshot inside the synchronized-update
bracket. Ordinary `cleat launch --record` casts then hold labelled frames, and
`region_cast` turns a cast into JSON lines of `(rows, regions)`.

| Recording | Frames | Regions per frame | Checked against cells |
|---|---|---|---|
| `ratatui-demo.cast` (alt screen) | 12 | 9–12 | every rectangle matches its border or text |
| `ink-demo.cast` (inline, main screen) | 12 | 18–20 | every rectangle matches, including the absolute-positioned dialog |
| `claude-onboarding.cast` (Claude Code 2.1.289, first-run theme picker) | 2 | 20, 117 | every emitted rectangle matches; some visible rows have no rectangle (see below) |

The hard parts are not the hooks:

- **Naming:** frameworks know the type of a widget, not its role.
- **Nesting** is lost wherever the framework flattens it.
- **Stability:** Claude Code's internals can change in any release.

## 1. Draft OSC

```text
ESC ] 7701 ; B ; f=<frame> ; o=a|c ST   begin a snapshot
ESC ] 7701 ; R ; id=<id> ; parent=<id> ; kind=<kind> ; name=<name> ; x=<col> ; y=<row> ; w=<cols> ; h=<rows> ST
ESC ] 7701 ; E ST                        commit: replaces the whole previous set
ESC ] 7701 ; X ST                        clear all declared regions
```

`ST` is `ESC \`. BEL is accepted.

**Snapshot, not deltas.** Every `B…E` block lists the complete region set.
A consumer that misses a record (a stream cut, an attach in mid-stream, a
dropped chunk) recovers at the next frame. Deltas would need the
start/end bookkeeping that OSC 3008 has, and a missed `end` corrupts the state
for good. A snapshot costs about 60–90 bytes per region per frame. Emitters
may skip the snapshot when the tree is unchanged: the Ink emitter keeps its
records deterministic, so log-update's unchanged-frame skip still works.

**Frame boundary.** The snapshot belongs to the frame drawn in the same
synchronized update (`?2026h … ?2026l`), wherever `E` falls inside it. It is
paired with the grid when that update ends, or straight away if there is no
bracket. ratatui and Claude Code put the snapshot just before ESU. Ink puts it
before the frame text. The parser handles both cases with one rule: wait for
`synchronized_output_active()` to become false.

**Coordinates.** All values are zero-based cells.

- `o=a`: viewport-absolute.
- `o=c`: rows are relative to the cursor row when `B` is parsed. Inline
  renderers need this. Ink writes its frame below whatever scrollback is
  already on screen and never learns its absolute row, but its cursor is at
  the frame's top-left right after log-update erases the previous frame.
- `y` and `h` may extend past the viewport. Claude Code's 37-row frame on a
  30-row screen is an example. A consumer intersects rectangles with the
  viewport itself, as pi's `bounds` and `clip` do.

**Nesting and z-order.** An explicit `parent=` records nesting. Record order is
paint order, so a later record is on top: ratatui records `Clear` and then the
popup, and Ink records the absolute dialog last. When the emitter cannot know
the parent, the consumer infers containment from geometry.
`region_cast` fills `contained_in` with the smallest earlier record that
encloses the region.

**Identity.** `id` is an opaque, emitter-chosen string (1–64 printable ASCII
characters, as in OSC 3008). It should stay stable across redraws for the same
logical widget. The spike uses structural paths (`files/Block.0`) or app labels
(`composer`). Structural ids shift when a sibling appears before them, so
labels are the dependable form. `kind` is the framework type or ARIA-like role
(`Block`, `List`, `listitem`, `dialog`, `textbox`). `name` is the app's label.

**Escaping.** Values escape `;`, `=`, `\`, control characters and non-ASCII
characters as `\xHH`. OSC 3008 escapes the same way.

**Invalidation.** Not designed beyond the obvious. A terminal should drop the
declared set on alternate-screen switch, RIS, resize until the next frame, and
`X`. In main-screen mode, rows that scroll into history keep no regions. A
region tree for scrollback (Codex `insert_history`, Claude Code transcript)
needs OSC 133-style per-line marks instead. That is an open question.

**Number.** 7701 is not in Ghostty's OSC table (0–22, 52, 55, 66, 72, 77, 99,
104, 110–119, 133, 552, 777, 1337, 3008, 5522), and a web search found no other
use. That is evidence it is free, not proof. Ghostty and xterm drop unknown
OSCs silently, so programs can emit it unconditionally.

### Prior art (nothing declares nested 2D rectangles)

- **OSC 133 / 633 / WezTerm semantic zones** mark prompt, input and output by
  row and cursor mode. Their only identity is the command id (`aid`).
- **iTerm2 OSC 1337.**
  - `AddAnnotation=msg|len|x|y` covers a 1D run of cells.
  - `Block=id=..;attr=start|end` defines foldable row blocks with ids. These
    are the closest thing to ids for regions.
- **OSC 3008** (UAPI context: systemd, shells, containers). It has `start=`
  and `end=` with ids. Nesting comes implicitly from a stack. Re-sending
  `start` with the same id resets its fields and ends its subcontexts. It has
  no geometry. The draft borrows its id character set and escaping.
- **bubblezone** (Bubble Tea) wraps content in private `CSI <n> z` markers,
  which lipgloss's width measurement ignores. `Scan()` strips the markers and
  records each zone's start and end cells. It is in-band marking within a
  single process.
- **Ink's `aria-role` / `aria-state`** and Claude Code's per-node
  `accessibility` field already carry roles for screen-reader mode. Those are
  ready-made `kind` values.
- **pi's terminal surfaces** (`katzensteg-terminal-surface`,
  `packages/tui/src/surface.ts`) give each component `bounds` (allocated rect),
  `clip` (visible part), `occlusions` (overlays above it) and `contentOffset`.
  It is the only source I found that records occlusion explicitly.
  `TUI.renderFrame` (`packages/tui/src/tui.ts:927-975`) computes these
  geometries inside its own `?2026h … ?2026l` bracket, just before ESU. That is
  the exact place to emit the OSC. I estimate (without trying) that pi needs
  about 20 lines to become an emitter. The draft should probably gain an optional `clip=`, or let consumers
  derive clipping from z-order as above.

Not verified: Contour, foot, VTE accessibility work and terminal-wg proposals
(gitlab.freedesktop.org blocked the fetch).

## 2. ratatui

**Hook.** `Frame::render_widget` and `Frame::render_stateful_widget` in
`ratatui-core` 0.1.0 (ratatui 0.30) are the only funnel from app code to
widgets.

- **Custom `Backend`: ruled out.** It only sees cell diffs, never widgets.
- **App-side wrapper (`render_region(frame, id, widget, area)`): ruled out.**
  It means editing every call site.
- **Patched crate: chosen** (`spikes/region-ground-truth/ratatui-regions.patch`):
  - `regions.rs` keeps a thread-local stack and list. `enter(type_name, area)`
    returns a guard. `label("composer")` names the next region.
  - `Frame::render_widget` and `render_stateful_widget` call `enter` with
    `type_name::<W>()`.
  - `Terminal::try_draw`, when `RATATUI_REGIONS=1`, writes BSU before
    flushing the cells and the snapshot plus ESU after.
  - `impl Widget for &Block` also calls `enter`. Blocks drawn inside other
    widgets (`List::block`, `Paragraph::block`) then become child regions.
    A Block over the same area as its parent Block is deduplicated.

**Demo** (`ratatui-demo/src/main.rs`, plain ratatui): a bordered app with
side-by-side Files (a `List` with a highlight) and Preview panels. Preview
contains a nested Details block, there is an inset Input box, and frames 5–9
show a `Clear` plus a Confirm popup. Output of
`region_cast ratatui-demo.cast --frame 6` (trimmed):

```text
 13 |││                                 ┌ Confirm ───────────────────┐                                │││|
 25 |│    ┌ Input ─────────────────────────────────────────────────────────────────────────────────┐    │|
app            Block      x=0  y=0  w=100 h=30 in=-        top="┌ cleat region demo ───…"
files          List       x=1  y=1  w=39  h=23 in=app      top="┌ Files ───…┐"
files/Block.0  Block      x=1  y=1  w=39  h=23 in=files
details        Block      x=41 y=4  w=57  h=19 in=preview  top="┌ Details ───…┐"
Paragraph.1    Paragraph  x=42 y=5  w=55  h=17 in=details  top="frame 5"
composer       Paragraph  x=5  y=25 w=90  h=3  in=app      top="┌ Input ───…"
Clear.0        Clear      x=35 y=13 w=30  h=5  in=app
confirm        Paragraph  x=35 y=13 w=30  h=5  in=app      top="┌ Confirm ───…┐"
```

**What it does not give:**

- **Nesting between sibling `render_widget` calls.** ratatui apps lay out with
  `Layout` and call the frame once per leaf, so `files` and `preview` are
  roots. Their `app` parent above comes only from geometric containment.
- **The selected list row.** It is `ListState`, not a widget. Patching
  `List::render` to emit a `listitem selected` child would take a few lines.
- **Names.** Without `label()` every kind is just `Paragraph`.
- **Custom widgets** that write to `Buffer` directly. Those exist in Codex.
- The stdout write assumes the backend is `CrosstermBackend<Stdout>`. It
  shares std's stdout buffer, so ordering holds. A backend on stderr or on a
  file would need the bytes routed through the backend instead.

### Codex

Codex (`openai/codex`, `codex-rs`, ratatui 0.30.2 unpatched) does not use
ratatui's `Terminal`:

- **Rendering path.**
  - `codex-rs/tui/src/custom_terminal.rs` is a fork with its own `Frame`.
  - `App` renders `chat_widget.render(area, frame.buffer)` (`app.rs:1211`)
    through Codex's own `Renderable` trait
    (`render/renderable.rs:16`, about 71 impls).
  - That trait takes a `&mut Buffer`, not a `Frame`, so patching ratatui
    would see almost nothing.
- **Hook points.**
  - Instrument the layout containers in `renderable.rs`: `ColumnRenderable`
    (190–205), `FlexRenderable` (369) and `InsetRenderable` (412).
  - Hand-label ChatWidget, BottomPane, the composer
    (`chat_composer.rs:4463`), the active cell and dialog overlays.
  - Emit inside the existing `stdout().sync_update(...)` in `Tui::draw`
    (`tui.rs:1286`), after `draw_with_size` (`tui.rs:1350`).
- **Effort.** About 100–200 lines in the `codex-tui` crate, with no dependency
  forks. Building Codex from source with the patch is realistic.
- **Caveats.**
  - The viewport is inline: regions need absolute rows (the viewport's
    `area.y` moves) or `o=c`.
  - Finished transcript cells go to scrollback through `insert_history_lines`
    (DECSTBM plus direct writes), not widgets, so history gets no
    rectangles.

I did not build Codex.

## 3. Ink and Claude Code

### Ink 8 (`ink-demo/`)

- **Hook.**
  - The Ink instance lives in a `WeakMap` keyed by stdout
    (`ink/build/instances.js`). It is not exported, so `ink-regions.mjs`
    imports the file by path, which resolves to the same module instance.
  - The emitter replaces `ink.log` (log-update) with a wrapper that prefixes
    each frame's text with the snapshot.
  - The throttled writer calls `this.log(output)` between its BSU and ESU, so
    the records land inside the bracket and before the cells, at the frame's
    top-left: `o=c`.
  - OSCs are zero-width and contain no newline, so Ink's line accounting is
    unaffected.
- **Tree.**
  - Walk `rootNode.childNodes` and sum each node's
    `yogaNode.getComputedLeft()` and `getComputedTop()` to get absolute
    positions.
  - `kind` is `internal_accessibility.role` (from `aria-role`), falling back
    to `box` or `text`.
  - `name` comes from an ad-hoc `regionName` prop, which Ink passes through
    into `style`.
- **Result.**
  - Inline rendering below a line of scrollback, with nesting four levels
    deep: `app/box.0/files/listitem.2/text.0`.
  - Every rectangle matches the cells, including the `position:"absolute"`
    dialog, which Ink draws without clearing what is underneath (the
    rectangle is still correct).
  - The first frame is rendered synchronously inside `render()`, before the
    hook is installed, so it has no regions. Installing the hook before the
    first frame would need a custom stdout or a patched Ink.

### Claude Code 2.1.289

- **Packaging.** Claude Code is a 230 MB Bun single-file executable
  (`~/.local/share/claude/versions/2.1.289`, Mach-O), not an npm `cli.js`.
  - `NODE_OPTIONS=--require` is ignored.
  - `BUN_OPTIONS="--preload x.cjs"` runs the preload before the bundle
    (checked with `claude --version`).
- **What the bundle contains.**
  - A fork of Ink on React 19.2 with a pure-JS Yoga port
    (`getComputedLeft(){return this.layout.left}`).
  - DOM nodes are plain objects with unminified property names: `nodeName`,
    `childNodes`, `parentNode`, `yogaNode`, `cachedLayout`, `accessibility`,
    `debugOwnerChain`, `scrollTop`, and others.
  - The painter assigns `node.cachedLayout = {x, y, width, height, top, clip}`
    in screen coordinates for every node it paints.
  - It writes `?2026h/l`.
- **Getting the tree** (`claude-preload/cc-regions.cjs`).
  - **Ruled out:** the React DevTools hook, because
    `reconciler.injectIntoDevTools` is never called. A prototype setter for
    `cachedLayout`, because the node literal defines it as an own property.
  - **Used:** a temporary `Array.prototype.push` trap catches the first
    `parent.childNodes.push(child)` (Claude Code's `appendChild`), walks
    `parentNode` up to `ink-root`, and restores `push`.
  - `process.stdout.write` is wrapped to insert the snapshot before each ESU.
  - Coordinates come from Yoga sums or from `cachedLayout`
    (`CC_REGIONS_SOURCE=cached`). The two agreed on the recorded frames.
- **Running it.** `claude-preload/run.sh` launches Claude with an empty
  `CLAUDE_CONFIG_DIR`. It shows first-run onboarding and never reads
  credentials. No login was attempted.
- **Result.**
  - Two frames: 20 and 117 rectangles.
  - The logo rows, the heading, the first two theme options and the 7-row
    diff preview (rows 22–28) all match the cells exactly.
  - The frame is 37 rows high on a 30-row screen, and the bottom rows lie
    off-screen.
- **What didn't fully work.**
  - The theme options on visible rows 16 and 18 have no text rectangle;
    row 16 lies outside every rectangle. Rows 17, 19 and 20 have text
    rectangles. I did not find out why. Candidates are nodes
    without a Yoga node (`ink-virtual-text`), or content that a select
    component paints outside its node.
  - The tree is very deep (about 20 levels of anonymous `box`) and has no
    roles on this screen. Without component names, `kind` is useless for
    labelling. `debugOwnerChain` may hold component names in some debug
    mode; I did not try.
  - This rests on undocumented internals, so any release can break it.
    Treat it as a research tool, not something to ship.

### Claude Code routes compared

| Route | What it sees | Exactness | Stability | Ships? |
|---|---|---|---|---|
| **Preload** (`BUN_OPTIONS`) | Claude Code's whole tree: composer, transcript, dialogs | exact geometry, poor names | breaks on internal renames; fork-specific | dataset collection only |
| **(a) Function-hooks plugin + wrapper** | only the plugin's own render sites (`ui.render` `AbovePrompt`/`Pane`, `prompt.section`), with site-local size props (`bodyColumns`, `maxRows`, `scroll.bodyRows`) and pointer positions in the site's own cells | exact inside a site; origin unknown | public API (early access, 2.1.260+) | yes, for plugin-owned regions |
| **(b) Wrapper infers regions** | cells only | heuristic (the #316/#319 segmentation producers) | robust | yes, but not ground truth |

For **(a)**, the katzensteg plugin (`~/dev/katzensteg/tools/claude-code-plugin`)
shows what the API exposes:

- It knows each site's body width, the row budget and where it placed each
  panel in site cells.
- The site's absolute screen origin is not exposed.
- The plugin cannot write the OSC itself: from 2.1.287 Claude Code rejects
  control characters in plugin text.

So the plugin reports site-local rectangles out of band. The katzensteg
host already offers HTTP via `KATZENSTEG_WM_HOST`.

The wrapper (`katzensteg-wm --wrap`) would then inject `OSC 7701` at Claude
Code's ESU, where it already finds safe insertion points between complete
escape sequences. The missing piece is the site origin:

- **Sentinel in the output stream (the stand-in trick).** The plugin draws a
  marker such as the U+10EEED stand-in at a site corner, and the wrapper finds
  it in the stream. The relay deliberately keeps no screen model, so it cannot
  turn a byte position into a cell. That needs a VT model, which cleat has
  (Ghostty) and katzensteg-wm does not.
- **Wrapper-side anchoring.** The wrapper anchors the plugin's site to
  something found in the stream.

The same wrapper is a plausible runtime path for declared regions on Claude
Code before any upstream adoption:

- Plugin-declared regions go out via (a).
- Claude Code's own composer and status regions come from the preload, or
  later from an upstream emitter.
- Unlabelled content falls back to (b).

The wrapper can also set `BUN_OPTIONS` for the child. cleat itself is such a
wrapper with a screen model, so it could resolve sentinel positions directly.

## 4. Other frameworks (feasibility only, not built)

- **Bubble Tea / lipgloss.**
  - `View()` returns a string with no rectangles.
  - **bubblezone:** make `Scan()` emit `OSC 7701` from the zones it strips.
    Zones carry ids but no nesting, so nesting comes from containment.
  - **lipgloss v2:** `Layer` and `Compositor` (`layer.go`) form a real
    rectangle tree with ids and z-order, so apps built on it can emit exact
    trees.
- **Textual.**
  - `screen._compositor.full_map` already maps each `Widget` to a
    `MapGeometry` (region, clip, order). Nesting comes from `widget.parent`,
    kind from the class name, and name from `id`.
  - Emit in `App._display` between `_begin_update` and `_end_update`, which
    already bracket the frame with sync. A subclass or a monkeypatch is
    enough.
  - This is the easiest of all: probably 30 lines.
- **ncurses via `LD_PRELOAD`.**
  - Interpose `newwin`, `derwin`, `subwin`, `newpad`, `mvwin`, `wresize` and
    `delwin`, plus libpanel for z-order, and emit at `doupdate`.
  - Many apps draw everything on `stdscr`, so the windows are not widgets.
    Pads need `prefresh` tracking. Static linking defeats the interposition.
  - On macOS, `DYLD_INSERT_LIBRARIES` is stripped for SIP-protected and
    hardened-runtime binaries.
  - It works on Linux for apps that really use windows. Expect coarse ground
    truth.

## 5. Parser (`crates/cleat/examples/region_cast.rs`)

```sh
cargo run -p cleat --example region_cast -- CAST              # JSON lines {t, frame, rows[], regions[]}
cargo run -p cleat --example region_cast -- CAST --frame N    # screen plus region table, with each region's top row cropped from the cells
```

- **Scanning.** It scans the raw cast output for `OSC 7701`, because Ghostty
  drops unknown OSCs. Records split across PTY reads are reassembled, and
  other OSCs pass through. Everything else is fed to a Ghostty engine.
- **Committing.** `E` commits once the synchronized update has ended, then the
  parser reads `screen_grid()`.
- **`o=c`.** The cursor row is read with a CPR query, because
  `ScreenGrid.cursor` is zeroed while the cursor is hidden, as it always is in
  TUIs.
- **Containment.** It fills `contained_in` for regions that have no explicit
  parent.
- **Tests.** Four unit tests cover the scanner (every split point), OSC
  pass-through, escaping and containment:
  `cargo test -p cleat --example region_cast`.

It does not handle the alternate-screen or resize invalidation rules, does not
clip to the viewport, and does not stream large casts by frame range.

## Reproduce

```sh
cd spikes/region-ground-truth
./prepare.sh "$SCRATCH/vendor"     # vendors and patches ratatui-core/widgets outside the repo
(cd ratatui-demo && cargo build)
(cd ink-demo && npm install)       # the spike symlinked node_modules to scratch instead
# record.sh RUNTIME_DIR OUT SECONDS CMD [ENV=VAL]: own daemon under a short runtime dir
./record.sh /tmp/cgt ratatui-demo.cast 5 "$PWD/ratatui-demo/target/debug/ratatui-region-demo" RATATUI_REGIONS=1
./record.sh /tmp/cgt ink-demo.cast 5 "node $PWD/ink-demo/app.mjs"
./record.sh /tmp/cgt claude-onboarding.cast 8 "sh $PWD/claude-preload/run.sh /tmp/cc-empty-config /tmp/cc.log"
./parse-all.sh /tmp/region-jsonl
```

`record.sh` starts an isolated daemon. Stop it afterwards by its exact PID
(`$RUNTIME_DIR/default*/daemon.pid`). `CLEAT_RUNTIME_DIR` must be short: the
scratch path exceeded `SUN_LEN`.

## Next steps

1. Make pi the first real emitter: about 20 lines in `TUI.renderFrame`, with
   `bounds`, `clip` and `occlusions` already computed.
2. Record a labelled corpus from the patched ratatui demo set, the Ink demos
   and Textual. Use it to score the #316 graph and temporal producers.
3. Decide on scrollback: per-line marks or regions anchored to history.
4. If Claude Code ground truth matters, find the missing rows, then try
   `debugOwnerChain` for component names.
