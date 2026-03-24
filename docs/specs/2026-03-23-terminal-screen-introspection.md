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

**Raw truth + derived observations.** Always keep the raw cell grid alongside any inferred regions. The raw grid is authoritative. Inferred regions are best-effort observations with confidence scores. Consumers can use either layer.

**Separate observation from interpretation.** An observation is "row 23 is full-width, reverse video, contains 'Connected'". An interpretation is "this is a status bar". A semantic claim is "status = Connected". Keep these distinct — it enables debugging when inference is wrong and lets detectors improve independently.

**Incremental updates.** Use Ghostty's per-row dirty flags to avoid re-analyzing the entire screen on every PTY read. Cleat maintains its own copy of the screen grid, updated incrementally.

## Design

### Layer 0: Ghostty C API additions

Ghostty's internal renderer accesses the terminal via `RenderState` — a module in `src/terminal/render.zig` that copies dirty rows from pages, resolves style IDs into concrete styles (colors, attributes), and presents flat arrays of cells+styles. This is terminal-module functionality, not renderer-module — it lives alongside the screen and page code.

Cleat needs the same access. New C API functions wrapping RenderState:

```c
// Opaque handle to a render state (one per terminal)
typedef struct GhosttyRenderState* GhosttyRenderState;

// Cell data with resolved style — no indirection through style IDs
typedef struct {
    uint32_t codepoint;       // Unicode codepoint (21 bits used)
    uint8_t wide;             // 0=narrow, 1=wide, 2=spacer_head, 3=spacer_tail
    uint8_t fg_r, fg_g, fg_b; // Resolved foreground color
    uint8_t bg_r, bg_g, bg_b; // Resolved background color
    uint16_t flags;           // bold, italic, inverse, underline, etc.
    // Exact layout TBD — this is the shape, not the ABI
} GhosttyResolvedCell;

// Create a render state for a terminal
GhosttyResult ghostty_render_state_new(
    const GhosttyAllocator* allocator,
    GhosttyRenderState* state,
    GhosttyTerminal terminal);

// Update render state — copies dirty rows, resolves styles
// Returns which rows are dirty via out_dirty_flags (bool per row)
GhosttyResult ghostty_render_state_update(
    GhosttyRenderState state,
    bool* out_dirty_flags,
    uint16_t num_rows);

// Read resolved cells for a range of rows
// Cells are written contiguously: row0[0..cols], row1[0..cols], ...
GhosttyResult ghostty_render_state_get_cells(
    GhosttyRenderState state,
    uint16_t start_row,
    uint16_t num_rows,
    GhosttyResolvedCell* out_cells,
    size_t out_cells_len);

// Get cursor state
GhosttyResult ghostty_render_state_get_cursor(
    GhosttyRenderState state,
    uint16_t* out_col,
    uint16_t* out_row,
    bool* out_visible,
    uint8_t* out_style);  // block, bar, underline

// Free render state
void ghostty_render_state_free(GhosttyRenderState state);
```

This mirrors the internal rendering contract: update() detects dirty rows and copies/resolves cells, get_cells() reads the resolved data, and dirty flags are cleared as part of update(). Cleat calls update() on each PTY read cycle (or lazily on inspect).

**Upstream strategy:** Maintain as a fork initially. Submit PR to Ghostty with working cleat usage as motivation. The API shape follows their internal patterns (RenderState is already how the Zig renderer works), so the ask is "expose what you already have" not "build something new."

### Layer 1: Screen grid (in cleat daemon)

The cleat daemon maintains a `ScreenGrid` — its own owned copy of the resolved cell state:

```rust
pub struct ScreenGrid {
    cells: Vec<ResolvedCell>,  // cols * rows, row-major
    cols: u16,
    rows: u16,
    cursor_col: u16,
    cursor_row: u16,
    cursor_visible: bool,
    generation: u64,           // Incremented on each update
    row_generations: Vec<u64>, // Per-row generation for change tracking
}
```

On each PTY read cycle (or lazily on first inspect request), the daemon:
1. Calls `ghostty_render_state_update()` to get dirty flags
2. Calls `ghostty_render_state_get_cells()` for dirty rows only
3. Updates its `ScreenGrid`, bumping row generations

The `ScreenGrid` is the stable interface that the analysis crate works against. If the Ghostty FFI changes shape, only this update code changes.

### Layer 2: Screen analysis crate (`crates/terminal-screen`)

A new workspace crate that takes a `ScreenGrid` reference and returns structural observations. No Ghostty dependency, no daemon awareness — pure analysis of a cell grid.

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

The `VtEngine` trait gets new methods for screen grid access:

```rust
pub trait VtEngine {
    // ... existing methods ...

    /// Get the current screen grid with resolved styles
    fn screen_grid(&mut self) -> Result<ScreenGrid, String>;

    /// Get which rows changed since last call
    fn dirty_rows(&mut self) -> Result<Vec<bool>, String>;
}
```

The passthrough engine returns empty/default results. The Ghostty engine delegates to the new C API. This keeps the VT engine abstraction clean.

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

### Asciicast recording extensions

Record inferred regions as a parallel track alongside raw VT output. New frame types: `regions_snapshot`, `semantic_event`, `screen_summary`. Recordings become "terminal + scene graph + event log" — much richer replay and debugging.

## Phasing

1. **Ghostty C API** — RenderState wrapper with resolved cells, dirty flags, cursor
2. **`crates/terminal-screen`** — ScreenGrid type, text search, band detection, box detection
3. **Wire into cleat** — VtEngine trait extension, `inspect --screen`
4. **Iterate** — Add detectors, tune confidence, test against real TUIs
5. **Region tracking** — Stable IDs, matching heuristics (follow-on spec)
6. **Semantics + events** — Roles, focus, test DSL (follow-on spec)
