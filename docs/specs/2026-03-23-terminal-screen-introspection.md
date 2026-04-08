# Terminal Screen Introspection and Analysis

Design spec for exposing structured terminal screen state through cleat, enabling agents, tests, and accessibility tools to understand what a terminal UI *means* rather than parsing raw text.

## Context

Cleat already maintains a full VT engine (Ghostty) in the daemon, processing every byte of PTY output into a rendered screen model. But the only way to access that screen is `capture`, which returns plain text — the same ANSI soup an agent would get from `cat /dev/pts/N`. All the structure Ghostty maintains internally (cell grid, styles, cursor, dirty tracking) is thrown away at the API boundary.

Meanwhile, agents driving TUI applications through cleat are forced to regex terminal output to determine what's on screen. A status bar is just a row of styled text. A modal dialog is just overwritten cells. Focus is just reverse video somewhere. The terminal expresses structure through visual convention, but cleat exposes only a flat character stream.

The core insight: **a terminal screen is not a grid of characters — it is a latent UI tree encoded in text, layout, style, and time.** Cleat's PTY broker position is the right place to extract that structure, because it already sees all the raw VT traffic and maintains the screen model.

## Goals

1. **Structured screen access** — Expose the cell grid with styles, cursor state, and dirty tracking through cleat's inspect API. One step above raw text: agents get "row 23 is full-width reverse video containing 'Connected'" not just the string "Connected" buried in ANSI escapes.

2. **Screen region detection** — Infer structural regions (bordered boxes, status bars, panels) from the rendered screen. Two steps above raw text: agents get "there's a box from (0,0) to (79,22) titled 'Jobs' and a status band at row 23."

3. **Foundation for semantic terminal mediation** — Design the analysis as a layered system that can grow toward semantic roles, stable region tracking, and eventually a test DSL and accessibility API — without requiring any of that upfront.

## Non-goals

- Semantic role inference (list, dialog, etc.) in the first cut — defer to follow-up once structural detection proves useful
- Stable region identity tracking across frames — requires matching heuristics that need tuning against real TUIs first
- Test DSL (`wait_region("modal").visible()`) — depends on tracking and semantics
- App hint protocol (OSC-based semantic annotations) — design-first, build later
- Accessibility / screen reader API — important long-term consumer but not the immediate driver

## Architectural principles

**Cleat is the renderer.** Ghostty's VT engine runs headless inside cleat's daemon. There is no GPU renderer, no window. Cleat is the only consumer of the terminal's rendered state. This means cleat can use the same rendering contract as Ghostty's internal renderer: call RenderState.update(), read resolved cells+styles, clear dirty flags.

**Screen model first, analysis second.** The VT engine gives us a cell grid with styles. A separate analysis layer infers structure from that grid. Keep these concerns in separate crates. The analysis crate takes a grid of cells and returns observations — it knows nothing about Ghostty, PTYs, or daemons.

**Raw truth + derived observations.** Always keep the raw cell grid alongside any inferred regions. The raw grid is authoritative. Inferred regions are best-effort observations with confidence scores (0.0–1.0, where 1.0 means certainty — e.g. a complete box-drawing rectangle — and lower values indicate heuristic inference). Consumers can filter by confidence threshold; results below 0.5 should generally be treated as speculative.

**Separate observation from interpretation.** An observation is "row 23 is full-width, reverse video, contains 'Connected'". An interpretation is "this is a status bar". A semantic claim is "status = Connected". Keep these distinct — it enables debugging when inference is wrong and lets detectors improve independently.

**Incremental updates.** Use Ghostty's per-row dirty flags to avoid re-analyzing the entire screen on every PTY read. Cleat maintains its own copy of the screen grid, updated incrementally.

## Design

### Layer 0: Ghostty Render State C API

Ghostty's upstream C API (`include/ghostty/vt/render.h`) already exposes a full render state API. This is included in cleat's pinned ghostty ref and available in `libghostty-vt`. No fork or upstream contribution is needed.

The API uses an iterator-based pattern rather than bulk cell copies:

**Lifecycle:**
- `ghostty_render_state_new()` / `ghostty_render_state_free()` — create/destroy render state
- `ghostty_render_state_update(state, terminal)` — snapshot terminal state, consumes terminal dirty flags

**Global state** (via `ghostty_render_state_get()`):
- Viewport dimensions (cols, rows)
- Dirty state (false / partial / full)
- Colors: background, foreground, cursor, full 256-color palette
- Cursor: position, visibility, blinking, visual style (bar/block/underline/hollow), password input, wide-char-tail

**Row iteration:**
- `ghostty_render_state_row_iterator_new/free()` — allocate reusable iterator
- `ghostty_render_state_row_iterator_next()` — advance to next row
- `ghostty_render_state_row_get()` — query per-row dirty flag, get cells handle

**Cell iteration** (per row):
- `ghostty_render_state_row_cells_new/free()` — allocate reusable cells container
- `ghostty_render_state_row_cells_next()` / `_select(x)` — iterate or jump to column
- `ghostty_render_state_row_cells_get()` — query cell data:
  - **Grapheme clusters**: `GRAPHEMES_LEN` (codepoint count) + `GRAPHEMES_BUF` (codepoint array) — full multi-codepoint grapheme support, not single codepoints
  - **Style**: `GhosttyStyle` sized struct with tagged-union colors (none/palette/rgb) for fg, bg, underline; bool flags for bold, italic, faint, blink, inverse, invisible, strikethrough, overline; underline enum (single/double/curly/dotted/dashed)
  - **Resolved colors**: `BG_COLOR` / `FG_COLOR` as `GhosttyColorRgb` — palette lookups already performed

**Dirty tracking:**
- Two-layer: global (false/partial/full) + per-row (bool)
- Caller is responsible for resetting both layers after reading
- `ghostty_render_state_set()` to reset global, `ghostty_render_state_row_set()` to reset per-row

**Key difference from original spec assumption:** The original spec proposed a flat `GhosttyResolvedCell` struct with bulk `get_cells()`. The actual API uses opaque iterators with per-field getters. This is more flexible (forward-compatible via sized structs) but means cleat must iterate and copy into its own types rather than memcpy rows.

A complete working example exists at `ghostty/example/c-vt-render/src/main.c` in the ghostty repo.

See also: [libghostty render state docs](https://libghostty.tip.ghostty.org/group__render.html)

### Layer 1: Screen grid (in cleat daemon)

The cleat daemon maintains a `ScreenGrid` — its own owned copy of the resolved cell state:

```rust
pub struct ScreenGrid {
    cells: Vec<ResolvedCell>,  // cols * rows, row-major
    cols: u16,
    rows: u16,
    cursor: CursorState,
    generation: u64,           // Incremented on each update
}

pub struct ResolvedCell {
    pub graphemes: Vec<u32>,   // Full grapheme cluster (multiple codepoints)
    pub fg: Rgb,               // Resolved foreground color
    pub bg: Rgb,               // Resolved background color
    pub flags: CellFlags,      // bold, italic, inverse, underline, etc.
}

pub struct CursorState {
    pub col: u16,
    pub row: u16,
    pub visible: bool,
    pub style: CursorStyle,    // bar, block, underline, hollow
    pub blinking: bool,
}
```

On each update, the daemon:
1. Calls `ghostty_render_state_update()` to snapshot terminal state
2. Checks global dirty state — skip if clean
3. Uses Ghostty's per-row dirty flags as a fast path to identify which rows to check
4. Iterates dirty rows and copies cell data into the `ScreenGrid`

The `ScreenGrid` is the stable interface that the analysis crate works against. If the Ghostty FFI changes shape, only this update code changes.

**Change tracking (future design needed):** Ghostty provides row-level dirty flags, which tell cleat which rows changed since the last update. But for region detection via temporal co-change analysis (rows that change together are likely in the same region), cleat will need cell-level change tracking — diffing the actual cell content against the previous frame, not just trusting Ghostty's row-dirty hint. Ghostty's row-dirty serves as an optimization to limit which rows to diff, but cleat owns the diff and the change history. The exact model (per-cell generation stamps, ring buffer of change sets, co-change clustering) should be designed when the analysis layer (#23) is built, not prematurely specified here.

### Layer 2: Screen analysis crate (`crates/terminal-screen`)

A new workspace crate that owns the `ScreenGrid` type and provides structural analysis. No Ghostty dependency, no daemon awareness — pure analysis of a cell grid. The daemon populates a `ScreenGrid` from the VT engine; the crate defines the type and all analysis functions.

#### Text search

The simplest and most immediately useful capability:

```rust
/// Find all occurrences of a text string on screen
pub fn find_text(grid: &ScreenGrid, needle: &str) -> Vec<TextMatch> { .. }

pub struct TextMatch {
    pub row: u16,
    pub col_start: u16,
    pub col_end: u16,
    pub text: String,
}
```

Agents can ask "is 'Connected' on screen?" and get back coordinates. No regex on ANSI soup.

#### Band detection

Detect full-width rows with distinct styling — status bars, headers, separators:

```rust
pub fn detect_bands(grid: &ScreenGrid) -> Vec<Band> { .. }

pub struct Band {
    pub row: u16,
    pub kind: BandKind, // styled, separator, blank
    pub text: String,
    pub style_summary: StyleSummary, // dominant colors, bold/inverse
    pub confidence: f32,
}

pub enum BandKind {
    Styled,    // Full-width row with distinct fg/bg (status bar, header)
    Separator, // Row of repeated characters (─, ═, -, etc.)
    Blank,     // Empty row acting as visual separator
}
```

A full-width reverse-video row containing "INSERT | UTF-8 | 12:34" is almost certainly a status bar. The detection is cheap (one pass over rows) and high-confidence.

#### Box detection

Detect rectangular regions bounded by box-drawing characters:

```rust
pub fn detect_boxes(grid: &ScreenGrid) -> Vec<DetectedBox> { .. }

pub struct DetectedBox {
    pub rect: Rect,          // x1, y1, x2, y2
    pub title: Option<String>, // Text in top border
    pub border_style: BorderStyle, // Unicode box-drawing, ASCII +--+, etc.
    pub confidence: f32,
}
```

Scan for box-drawing corners (┌┐└┘ or +), trace edges, extract interior. Most TUI frameworks (ratatui, etc.) use standard box-drawing characters, so detection is straightforward.

#### Screen snapshot

Combine everything into a single structured result:

```rust
pub fn analyze(grid: &ScreenGrid) -> ScreenAnalysis { .. }

pub struct ScreenAnalysis {
    pub cursor: CursorState,
    pub bands: Vec<Band>,
    pub boxes: Vec<DetectedBox>,
    pub generation: u64,
}
```

### Layer 3: Expose via inspect

New inspect mode in cleat's CLI:

```bash
# Full screen analysis — detected regions, text search, cursor
cleat inspect <session> --screen

# Just the text content with row numbers
cleat inspect <session> --screen --text-only

# Search for text on screen
cleat inspect <session> --screen --find "Connected"
```

Returns JSON:

```json
{
  "screen": {
    "cols": 80,
    "rows": 24,
    "cursor": { "col": 14, "row": 7, "visible": false },
    "generation": 4207
  },
  "bands": [
    {
      "row": 23,
      "kind": "styled",
      "text": "INSERT | UTF-8 | 12:34",
      "style": { "inverse": true },
      "confidence": 0.95
    }
  ],
  "boxes": [
    {
      "rect": [0, 0, 79, 22],
      "title": "Jobs",
      "border_style": "unicode",
      "confidence": 0.9
    }
  ],
  "matches": [
    { "row": 23, "col_start": 35, "col_end": 39, "text": "12:34" }
  ]
}
```

This extends the existing inspect infrastructure. The `--screen` flag triggers a render state update and analysis pass. Without `--screen`, inspect returns the same metadata it does today (session state, process info, etc.).

### VtEngine trait extension

The `VtEngine` trait gets a method to read the current screen as a grid of resolved cells:

```rust
pub trait VtEngine {
    // ... existing methods ...

    /// Snapshot the current screen as a grid of resolved cells.
    /// The engine owns the render state internally; this copies data out.
    fn screen_grid(&mut self) -> Result<ScreenGrid, String>;
}
```

The Ghostty engine calls `render_state_update()` internally, iterates cells, and populates a `ScreenGrid`. The passthrough engine returns an error. The `ScreenGrid` is an owned snapshot — callers can hold it, diff it against previous snapshots, or pass it to the analysis layer without lifetime concerns.

This is intentionally a simple "give me the screen" method. Change tracking, dirty optimization, and incremental update logic belong in the layer above (the daemon or analysis crate), not in the trait itself — different consumers will have different change-tracking needs.

## Future work

These are explicitly deferred but the architecture supports them:

### Region tracking with stable IDs

Assign persistent identities to detected regions across frames. Match regions between frames using overlap (IoU), text similarity, style similarity, and spatial proximity. Lifecycle: create → update → stable → change → delete. This enables assertions like "the sidebar stayed stable while the main pane changed."

### Semantic role inference

Assign meaning to detected regions: this box is a `list`, that band is a `status_bar`, this overlay is a `dialog`. Use heuristics: a box containing a highlighted row and many filenames is probably a file list. A small centered bordered region appearing over previous content is probably a modal.

### Perturbation-based inference

Since cleat controls input, it can infer semantics by observing *response* to controlled disturbance: press down-arrow → which region changes → that's a list. Press Tab → which region highlights → focus tracking. Resize → which regions reflow → layout boundaries.

### Semantic events

Instead of full-screen snapshots, emit discrete events: `focus_changed`, `selection_changed`, `modal_opened`, `status_changed`. These are what tests, screen readers, and agents actually need.

### App hints protocol

Let apps optionally emit semantic annotations via OSC sequences or a sidechannel: region IDs, roles, labels, focus state. Keep separate from inference. Apps that cooperate get high-confidence semantics; apps that don't still get best-effort inference.

### Test DSL

```rust
wait_region("modal").visible();
assert_region("status").contains("Connected");
assert_region("files").selected_text() == "Cargo.toml";
wait_until_stable(ignore=["spinner"]);
```

### Wait/watch mode for inspect

The initial `inspect --screen` is one-shot. Agents and tests will quickly want "wait until X appears on screen" semantics. A `--wait-for "text"` or `--watch` flag would poll the screen grid and block until a condition is met or a timeout expires. This is a stepping stone toward the full event subscription model (#9) and avoids agents implementing their own poll loops.

### Asciicast recording extensions

Record inferred regions as a parallel track alongside raw VT output. New frame types: `regions_snapshot`, `semantic_event`, `screen_summary`. Recordings become "terminal + scene graph + event log" — much richer replay and debugging.

## Phasing

1. ~~**Ghostty C API**~~ — Already exists upstream, included in cleat's pinned ref
2. **FFI bindings + `screen_grid()`** — Rust FFI for render state API, `ScreenGrid`/`ResolvedCell` types, `VtEngine::screen_grid()` method on Ghostty engine
3. **Transcoding (#29)** — First consumer. Use `screen_grid()` to render `capture --since-marker` as clean text instead of ANSI soup. Validates the FFI bindings against a real use case without needing the analysis layer.
4. **`crates/terminal-screen`** — ScreenGrid analysis: text search, band detection, box detection. Design the change-tracking model here when we understand the actual access patterns.
5. **`inspect --screen`** — Wire analysis into the CLI
6. **Iterate** — Add detectors, tune confidence, test against real TUIs
7. **Region tracking** — Stable IDs, temporal co-change analysis, matching heuristics (follow-on spec)
8. **Semantics + events** — Roles, focus, test DSL (follow-on spec)
