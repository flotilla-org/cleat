# Render State FFI Bindings Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add Rust FFI bindings for Ghostty's Render State C API and expose a `screen_grid()` method on `GhosttyVtEngine` that returns cleat-native `ScreenGrid`/`ResolvedCell` types.

**Architecture:** The FFI layer in `ghostty_ffi.rs` gets raw `extern "C"` declarations and RAII wrapper types matching the existing `TerminalHandle` pattern. New cleat-native types (`ScreenGrid`, `ResolvedCell`, `CursorState`) live in `vt/mod.rs` and are populated by iterating Ghostty's render state API. The `VtEngine` trait gains a `screen_grid()` method; the passthrough engine returns an error.

**Tech Stack:** Rust, unsafe FFI (`#[link(name = "ghostty-vt")]`), `#[repr(C)]` structs matching Ghostty's ABI.

---

## File Structure

| File | Role |
|------|------|
| `crates/cleat/src/vt/mod.rs` | `ScreenGrid`, `ResolvedCell`, `CursorState`, `CellFlags`, `CursorStyle`, `Rgb` types + `screen_grid()` on `VtEngine` trait |
| `crates/cleat/src/vt/ghostty_ffi.rs` | Raw FFI declarations for render state API, `#[repr(C)]` mirror types, RAII wrappers (`RenderStateHandle`, `RowIteratorHandle`, `RowCellsHandle`) |
| `crates/cleat/src/vt/ghostty.rs` | `GhosttyVtEngine::screen_grid()` implementation using FFI wrappers |
| `crates/cleat/src/vt/passthrough.rs` | `PassthroughVtEngine::screen_grid()` returning error |
| `crates/cleat/tests/vt.rs` | Integration tests for `screen_grid()` |
| `crates/cleat/tests/vt_contracts.rs` | Contract test helpers updated for `screen_grid()` |

---

### Task 1: Add cleat-native screen types to `vt/mod.rs`

Define the types that consumers will work with. These are pure Rust — no FFI dependency.

**Files:**
- Modify: `crates/cleat/src/vt/mod.rs`

- [ ] **Step 1: Add the screen types after the existing `ColorLevel` enum**

Add these types to `crates/cleat/src/vt/mod.rs` after line 39 (after the `ColorLevel` enum), before `VtEngineKind`:

```rust
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct CellFlags: u16 {
        const BOLD          = 1 << 0;
        const ITALIC        = 1 << 1;
        const FAINT         = 1 << 2;
        const BLINK         = 1 << 3;
        const INVERSE       = 1 << 4;
        const INVISIBLE     = 1 << 5;
        const STRIKETHROUGH = 1 << 6;
        const OVERLINE      = 1 << 7;
        const UNDERLINE     = 1 << 8;
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResolvedCell {
    pub graphemes: Vec<u32>,
    pub fg: Rgb,
    pub bg: Rgb,
    pub flags: CellFlags,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CursorStyle {
    Bar,
    #[default]
    Block,
    Underline,
    BlockHollow,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CursorState {
    pub col: u16,
    pub row: u16,
    pub visible: bool,
    pub style: CursorStyle,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScreenGrid {
    pub cells: Vec<ResolvedCell>,
    pub cols: u16,
    pub rows: u16,
    pub cursor: CursorState,
}

impl ScreenGrid {
    pub fn cell(&self, col: u16, row: u16) -> Option<&ResolvedCell> {
        if col < self.cols && row < self.rows {
            self.cells.get((row as usize) * (self.cols as usize) + (col as usize))
        } else {
            None
        }
    }

    pub fn row_text(&self, row: u16) -> String {
        if row >= self.rows {
            return String::new();
        }
        let start = (row as usize) * (self.cols as usize);
        let end = start + (self.cols as usize);
        self.cells[start..end]
            .iter()
            .map(|cell| {
                if cell.graphemes.is_empty() {
                    ' '
                } else {
                    char::from_u32(cell.graphemes[0]).unwrap_or(' ')
                }
            })
            .collect()
    }
}
```

- [ ] **Step 2: Add `bitflags` dependency to Cargo.toml**

Check if `bitflags` is already a dependency. If not, add it:

Run: `grep bitflags crates/cleat/Cargo.toml`

If not present, add `bitflags = "2"` to `[dependencies]` in `crates/cleat/Cargo.toml`.

- [ ] **Step 3: Add `screen_grid()` to the `VtEngine` trait**

In `crates/cleat/src/vt/mod.rs`, add to the `VtEngine` trait (after `screen_text`):

```rust
    fn screen_grid(&mut self) -> Result<ScreenGrid, String>;
```

- [ ] **Step 4: Add `screen_grid()` stub to PassthroughVtEngine**

In `crates/cleat/src/vt/passthrough.rs`, add to the `VtEngine` impl:

```rust
    fn screen_grid(&mut self) -> Result<ScreenGrid, String> {
        Err("screen grid is unavailable because vt engine passthrough is a placeholder/test-only engine, not a functional VT engine".to_string())
    }
```

Import `ScreenGrid` in the use statement:

```rust
use super::{ClientCapabilities, ScreenGrid, VtEngine};
```

- [ ] **Step 5: Add `screen_grid()` to test VtEngine impls**

In `crates/cleat/tests/vt_contracts.rs`, add `screen_grid` to the `PlaceholderReplayVtEngine` impl:

```rust
    fn screen_grid(&mut self) -> Result<ScreenGrid, String> {
        Ok(ScreenGrid::default())
    }
```

Import `ScreenGrid` in the use statement.

Also in `crates/cleat/src/session.rs`, add `screen_grid` to `TestReplayProbeVtEngine`:

```rust
    fn screen_grid(&mut self) -> Result<ScreenGrid, String> {
        Ok(ScreenGrid::default())
    }
```

Import `ScreenGrid` in the relevant use statement.

- [ ] **Step 6: Add the public re-exports**

In `crates/cleat/src/vt/mod.rs`, add to the existing re-exports near the top:

```rust
pub use self::{CellFlags, CursorState, CursorStyle, ResolvedCell, Rgb, ScreenGrid};
```

- [ ] **Step 7: Verify compilation**

Run: `cargo build --workspace --locked`
Expected: BUILD SUCCESS

- [ ] **Step 8: Commit**

```bash
git add crates/cleat/src/vt/mod.rs crates/cleat/src/vt/passthrough.rs crates/cleat/tests/vt_contracts.rs crates/cleat/src/session.rs crates/cleat/Cargo.toml
git commit -m "vt: add ScreenGrid, ResolvedCell, and screen_grid() trait method"
```

---

### Task 2: Add render state FFI declarations and RAII wrappers

Add the raw `extern "C"` declarations and safe wrapper types to `ghostty_ffi.rs`.

**Files:**
- Modify: `crates/cleat/src/vt/ghostty_ffi.rs`

- [ ] **Step 1: Add `#[repr(C)]` mirror types for Ghostty's enums and structs**

Add these after the existing `GhosttyFormatterOpaque` line (before the `#[link]` block) in `ghostty_ffi.rs`:

```rust
// --- Render state FFI types ---

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GhosttyRenderStateDirty {
    False = 0,
    Partial = 1,
    Full = 2,
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GhosttyRenderStateCursorVisualStyle {
    Bar = 0,
    Block = 1,
    Underline = 2,
    BlockHollow = 3,
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GhosttyRenderStateData {
    Invalid = 0,
    Cols = 1,
    Rows = 2,
    Dirty = 3,
    RowIterator = 4,
    ColorBackground = 5,
    ColorForeground = 6,
    ColorCursor = 7,
    ColorCursorHasValue = 8,
    ColorPalette = 9,
    CursorVisualStyle = 10,
    CursorVisible = 11,
    CursorBlinking = 12,
    CursorPasswordInput = 13,
    CursorViewportHasValue = 14,
    CursorViewportX = 15,
    CursorViewportY = 16,
    CursorViewportWideTail = 17,
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GhosttyRenderStateOption {
    Dirty = 0,
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GhosttyRenderStateRowData {
    Invalid = 0,
    Dirty = 1,
    Raw = 2,
    Cells = 3,
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GhosttyRenderStateRowOption {
    Dirty = 0,
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GhosttyRenderStateRowCellsData {
    Invalid = 0,
    Raw = 1,
    Style = 2,
    GraphemesLen = 3,
    GraphemesBuf = 4,
    BgColor = 5,
    FgColor = 6,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct GhosttyColorRgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub enum GhosttyStyleColorTag {
    None = 0,
    Palette = 1,
    Rgb = 2,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union GhosttyStyleColorValue {
    pub palette: u8,
    pub rgb: GhosttyColorRgb,
    pub _padding: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GhosttyStyleColor {
    pub tag: GhosttyStyleColorTag,
    pub value: GhosttyStyleColorValue,
}

/// Sized struct — `size` must be set to `size_of::<Self>()` before use.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct GhosttyStyle {
    pub size: usize,
    pub fg_color: GhosttyStyleColor,
    pub bg_color: GhosttyStyleColor,
    pub underline_color: GhosttyStyleColor,
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub blink: bool,
    pub inverse: bool,
    pub invisible: bool,
    pub strikethrough: bool,
    pub overline: bool,
    pub underline: i32,
}

impl GhosttyStyle {
    pub fn init() -> Self {
        // Safety: zero-init is valid for this repr(C) struct;
        // we then set the size field for the sized-struct ABI.
        let mut s: Self = unsafe { std::mem::zeroed() };
        s.size = std::mem::size_of::<Self>();
        s
    }
}

/// Sized struct for bulk color retrieval.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct GhosttyRenderStateColors {
    pub size: usize,
    pub background: GhosttyColorRgb,
    pub foreground: GhosttyColorRgb,
    pub cursor: GhosttyColorRgb,
    pub cursor_has_value: bool,
    pub palette: [GhosttyColorRgb; 256],
}

impl GhosttyRenderStateColors {
    pub fn init() -> Self {
        let mut s: Self = unsafe { std::mem::zeroed() };
        s.size = std::mem::size_of::<Self>();
        s
    }
}

pub enum GhosttyRenderStateOpaque {}
pub enum GhosttyRowIteratorOpaque {}
pub enum GhosttyRowCellsOpaque {}

pub type GhosttyRenderState = *mut GhosttyRenderStateOpaque;
pub type GhosttyRenderStateRowIterator = *mut GhosttyRowIteratorOpaque;
pub type GhosttyRenderStateRowCells = *mut GhosttyRowCellsOpaque;
```

- [ ] **Step 2: Add `extern "C"` function declarations**

Add these inside the existing `#[link(name = "ghostty-vt")] unsafe extern "C"` block, after the formatter declarations:

```rust
    // --- Render state ---
    fn ghostty_render_state_new(allocator: *const c_void, state: *mut GhosttyRenderState) -> GhosttyResult;
    fn ghostty_render_state_free(state: GhosttyRenderState);
    fn ghostty_render_state_update(state: GhosttyRenderState, terminal: GhosttyTerminal) -> GhosttyResult;
    fn ghostty_render_state_get(state: GhosttyRenderState, data: GhosttyRenderStateData, out: *mut c_void) -> GhosttyResult;
    fn ghostty_render_state_set(state: GhosttyRenderState, option: GhosttyRenderStateOption, value: *const c_void) -> GhosttyResult;
    fn ghostty_render_state_colors_get(state: GhosttyRenderState, out_colors: *mut GhosttyRenderStateColors) -> GhosttyResult;

    // --- Row iterator ---
    fn ghostty_render_state_row_iterator_new(allocator: *const c_void, out_iterator: *mut GhosttyRenderStateRowIterator) -> GhosttyResult;
    fn ghostty_render_state_row_iterator_free(iterator: GhosttyRenderStateRowIterator);
    fn ghostty_render_state_row_iterator_next(iterator: GhosttyRenderStateRowIterator) -> bool;
    fn ghostty_render_state_row_get(iterator: GhosttyRenderStateRowIterator, data: GhosttyRenderStateRowData, out: *mut c_void) -> GhosttyResult;
    fn ghostty_render_state_row_set(iterator: GhosttyRenderStateRowIterator, option: GhosttyRenderStateRowOption, value: *const c_void) -> GhosttyResult;

    // --- Row cells ---
    fn ghostty_render_state_row_cells_new(allocator: *const c_void, out_cells: *mut GhosttyRenderStateRowCells) -> GhosttyResult;
    fn ghostty_render_state_row_cells_free(cells: GhosttyRenderStateRowCells);
    fn ghostty_render_state_row_cells_next(cells: GhosttyRenderStateRowCells) -> bool;
    fn ghostty_render_state_row_cells_select(cells: GhosttyRenderStateRowCells, x: u16) -> GhosttyResult;
    fn ghostty_render_state_row_cells_get(cells: GhosttyRenderStateRowCells, data: GhosttyRenderStateRowCellsData, out: *mut c_void) -> GhosttyResult;
```

- [ ] **Step 3: Add RAII wrapper — `RenderStateHandle`**

Add after the existing `TerminalHandle` impl in `ghostty_ffi.rs`:

```rust
pub struct RenderStateHandle {
    raw: GhosttyRenderState,
}

impl RenderStateHandle {
    pub fn new() -> Result<Self, String> {
        let mut raw = ptr::null_mut();
        let result = unsafe { ghostty_render_state_new(ptr::null(), &mut raw) };
        check_result(result, "ghostty_render_state_new")?;
        Ok(Self { raw })
    }

    pub fn update(&mut self, terminal: &TerminalHandle) -> Result<(), String> {
        let result = unsafe { ghostty_render_state_update(self.raw, terminal.raw()) };
        check_result(result, "ghostty_render_state_update")
    }

    pub fn get_cols(&self) -> Result<u16, String> {
        let mut cols: u16 = 0;
        let result = unsafe { ghostty_render_state_get(self.raw, GhosttyRenderStateData::Cols, &mut cols as *mut u16 as *mut c_void) };
        check_result(result, "ghostty_render_state_get(Cols)")?;
        Ok(cols)
    }

    pub fn get_rows(&self) -> Result<u16, String> {
        let mut rows: u16 = 0;
        let result = unsafe { ghostty_render_state_get(self.raw, GhosttyRenderStateData::Rows, &mut rows as *mut u16 as *mut c_void) };
        check_result(result, "ghostty_render_state_get(Rows)")?;
        Ok(rows)
    }

    pub fn get_dirty(&self) -> Result<GhosttyRenderStateDirty, String> {
        let mut dirty = GhosttyRenderStateDirty::False;
        let result = unsafe { ghostty_render_state_get(self.raw, GhosttyRenderStateData::Dirty, &mut dirty as *mut GhosttyRenderStateDirty as *mut c_void) };
        check_result(result, "ghostty_render_state_get(Dirty)")?;
        Ok(dirty)
    }

    pub fn set_dirty(&mut self, dirty: GhosttyRenderStateDirty) -> Result<(), String> {
        let result = unsafe { ghostty_render_state_set(self.raw, GhosttyRenderStateOption::Dirty, &dirty as *const GhosttyRenderStateDirty as *const c_void) };
        check_result(result, "ghostty_render_state_set(Dirty)")
    }

    pub fn get_colors(&self) -> Result<GhosttyRenderStateColors, String> {
        let mut colors = GhosttyRenderStateColors::init();
        let result = unsafe { ghostty_render_state_colors_get(self.raw, &mut colors) };
        check_result(result, "ghostty_render_state_colors_get")?;
        Ok(colors)
    }

    pub fn get_cursor_visible(&self) -> Result<bool, String> {
        let mut visible = false;
        let result = unsafe { ghostty_render_state_get(self.raw, GhosttyRenderStateData::CursorVisible, &mut visible as *mut bool as *mut c_void) };
        check_result(result, "ghostty_render_state_get(CursorVisible)")?;
        Ok(visible)
    }

    pub fn get_cursor_viewport_has_value(&self) -> Result<bool, String> {
        let mut has_value = false;
        let result = unsafe { ghostty_render_state_get(self.raw, GhosttyRenderStateData::CursorViewportHasValue, &mut has_value as *mut bool as *mut c_void) };
        check_result(result, "ghostty_render_state_get(CursorViewportHasValue)")?;
        Ok(has_value)
    }

    pub fn get_cursor_viewport_x(&self) -> Result<u16, String> {
        let mut x: u16 = 0;
        let result = unsafe { ghostty_render_state_get(self.raw, GhosttyRenderStateData::CursorViewportX, &mut x as *mut u16 as *mut c_void) };
        check_result(result, "ghostty_render_state_get(CursorViewportX)")?;
        Ok(x)
    }

    pub fn get_cursor_viewport_y(&self) -> Result<u16, String> {
        let mut y: u16 = 0;
        let result = unsafe { ghostty_render_state_get(self.raw, GhosttyRenderStateData::CursorViewportY, &mut y as *mut u16 as *mut c_void) };
        check_result(result, "ghostty_render_state_get(CursorViewportY)")?;
        Ok(y)
    }

    pub fn get_cursor_visual_style(&self) -> Result<GhosttyRenderStateCursorVisualStyle, String> {
        let mut style = GhosttyRenderStateCursorVisualStyle::Block;
        let result = unsafe { ghostty_render_state_get(self.raw, GhosttyRenderStateData::CursorVisualStyle, &mut style as *mut GhosttyRenderStateCursorVisualStyle as *mut c_void) };
        check_result(result, "ghostty_render_state_get(CursorVisualStyle)")?;
        Ok(style)
    }

    pub fn populate_row_iterator(&self, iterator: &mut RowIteratorHandle) -> Result<(), String> {
        let result = unsafe { ghostty_render_state_get(self.raw, GhosttyRenderStateData::RowIterator, &mut iterator.raw as *mut GhosttyRenderStateRowIterator as *mut c_void) };
        check_result(result, "ghostty_render_state_get(RowIterator)")
    }
}

impl Drop for RenderStateHandle {
    fn drop(&mut self) {
        unsafe { ghostty_render_state_free(self.raw) };
    }
}
```

- [ ] **Step 4: Add RAII wrapper — `RowIteratorHandle`**

```rust
pub struct RowIteratorHandle {
    raw: GhosttyRenderStateRowIterator,
}

impl RowIteratorHandle {
    pub fn new() -> Result<Self, String> {
        let mut raw = ptr::null_mut();
        let result = unsafe { ghostty_render_state_row_iterator_new(ptr::null(), &mut raw) };
        check_result(result, "ghostty_render_state_row_iterator_new")?;
        Ok(Self { raw })
    }

    pub fn next(&mut self) -> bool {
        unsafe { ghostty_render_state_row_iterator_next(self.raw) }
    }

    pub fn get_dirty(&self) -> Result<bool, String> {
        let mut dirty = false;
        let result = unsafe { ghostty_render_state_row_get(self.raw, GhosttyRenderStateRowData::Dirty, &mut dirty as *mut bool as *mut c_void) };
        check_result(result, "ghostty_render_state_row_get(Dirty)")?;
        Ok(dirty)
    }

    pub fn populate_cells(&self, cells: &mut RowCellsHandle) -> Result<(), String> {
        let result = unsafe { ghostty_render_state_row_get(self.raw, GhosttyRenderStateRowData::Cells, &mut cells.raw as *mut GhosttyRenderStateRowCells as *mut c_void) };
        check_result(result, "ghostty_render_state_row_get(Cells)")
    }

    pub fn set_dirty(&mut self, dirty: bool) -> Result<(), String> {
        let result = unsafe { ghostty_render_state_row_set(self.raw, GhosttyRenderStateRowOption::Dirty, &dirty as *const bool as *const c_void) };
        check_result(result, "ghostty_render_state_row_set(Dirty)")
    }
}

impl Drop for RowIteratorHandle {
    fn drop(&mut self) {
        unsafe { ghostty_render_state_row_iterator_free(self.raw) };
    }
}
```

- [ ] **Step 5: Add RAII wrapper — `RowCellsHandle`**

```rust
pub struct RowCellsHandle {
    raw: GhosttyRenderStateRowCells,
}

impl RowCellsHandle {
    pub fn new() -> Result<Self, String> {
        let mut raw = ptr::null_mut();
        let result = unsafe { ghostty_render_state_row_cells_new(ptr::null(), &mut raw) };
        check_result(result, "ghostty_render_state_row_cells_new")?;
        Ok(Self { raw })
    }

    pub fn next(&mut self) -> bool {
        unsafe { ghostty_render_state_row_cells_next(self.raw) }
    }

    pub fn get_graphemes_len(&self) -> Result<u32, String> {
        let mut len: u32 = 0;
        let result = unsafe { ghostty_render_state_row_cells_get(self.raw, GhosttyRenderStateRowCellsData::GraphemesLen, &mut len as *mut u32 as *mut c_void) };
        check_result(result, "ghostty_render_state_row_cells_get(GraphemesLen)")?;
        Ok(len)
    }

    pub fn get_graphemes_buf(&self, buf: &mut [u32]) -> Result<(), String> {
        let result = unsafe { ghostty_render_state_row_cells_get(self.raw, GhosttyRenderStateRowCellsData::GraphemesBuf, buf.as_mut_ptr() as *mut c_void) };
        check_result(result, "ghostty_render_state_row_cells_get(GraphemesBuf)")
    }

    pub fn get_style(&self) -> Result<GhosttyStyle, String> {
        let mut style = GhosttyStyle::init();
        let result = unsafe { ghostty_render_state_row_cells_get(self.raw, GhosttyRenderStateRowCellsData::Style, &mut style as *mut GhosttyStyle as *mut c_void) };
        check_result(result, "ghostty_render_state_row_cells_get(Style)")?;
        Ok(style)
    }

    pub fn get_bg_color(&self) -> Result<Option<GhosttyColorRgb>, String> {
        let mut color = GhosttyColorRgb::default();
        let result = unsafe { ghostty_render_state_row_cells_get(self.raw, GhosttyRenderStateRowCellsData::BgColor, &mut color as *mut GhosttyColorRgb as *mut c_void) };
        match result {
            GhosttyResult::Success => Ok(Some(color)),
            GhosttyResult::InvalidValue => Ok(None),
            other => check_result(other, "ghostty_render_state_row_cells_get(BgColor)").map(|_| None),
        }
    }

    pub fn get_fg_color(&self) -> Result<Option<GhosttyColorRgb>, String> {
        let mut color = GhosttyColorRgb::default();
        let result = unsafe { ghostty_render_state_row_cells_get(self.raw, GhosttyRenderStateRowCellsData::FgColor, &mut color as *mut GhosttyColorRgb as *mut c_void) };
        match result {
            GhosttyResult::Success => Ok(Some(color)),
            GhosttyResult::InvalidValue => Ok(None),
            other => check_result(other, "ghostty_render_state_row_cells_get(FgColor)").map(|_| None),
        }
    }
}

impl Drop for RowCellsHandle {
    fn drop(&mut self) {
        unsafe { ghostty_render_state_row_cells_free(self.raw) };
    }
}
```

- [ ] **Step 6: Verify compilation**

Run: `cargo build --workspace --locked --features ghostty-vt`
Expected: BUILD SUCCESS (no tests yet — just verifying FFI declarations link)

- [ ] **Step 7: Commit**

```bash
git add crates/cleat/src/vt/ghostty_ffi.rs
git commit -m "ffi: add render state API declarations and RAII wrappers"
```

---

### Task 3: Implement `GhosttyVtEngine::screen_grid()`

Wire the FFI wrappers into the engine to populate `ScreenGrid` from the render state.

**Files:**
- Modify: `crates/cleat/src/vt/ghostty.rs`

- [ ] **Step 1: Write the failing test**

Add to `crates/cleat/tests/vt.rs`:

```rust
#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_screen_grid_returns_correct_dimensions() {
    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(40, 5);

    engine.feed(b"hello grid").expect("feed bytes");

    let grid = engine.screen_grid().expect("screen grid");
    assert_eq!(grid.cols, 40);
    assert_eq!(grid.rows, 5);
    assert_eq!(grid.cells.len(), 40 * 5);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --locked --features ghostty-vt -p cleat --test vt vt_ghostty_screen_grid_returns_correct_dimensions`
Expected: FAIL — `screen_grid` not implemented yet.

- [ ] **Step 3: Implement `screen_grid()` on `GhosttyVtEngine`**

In `crates/cleat/src/vt/ghostty.rs`, update the imports and add render state field + implementation:

```rust
use super::{
    ghostty_ffi::{
        self, GhosttyFormatterFormat, GhosttyFormatterTerminalOptions, GhosttyRenderStateCursorVisualStyle,
        GhosttyRenderStateDirty, GhosttyStyle, GhosttyStyleColorTag, RenderStateHandle, RowCellsHandle,
        RowIteratorHandle, TerminalHandle,
    },
    CellFlags, ClientCapabilities, ColorLevel, CursorState, CursorStyle, ResolvedCell, Rgb, ScreenGrid, VtEngine,
};

const DEFAULT_MAX_SCROLLBACK: usize = 10_000;

pub struct GhosttyVtEngine {
    terminal: TerminalHandle,
    render_state: RenderStateHandle,
    cols: u16,
    rows: u16,
    saw_output: bool,
}

impl GhosttyVtEngine {
    pub fn new(cols: u16, rows: u16) -> Self {
        let terminal = TerminalHandle::new(cols, rows, DEFAULT_MAX_SCROLLBACK).expect("create ghostty terminal");
        let render_state = RenderStateHandle::new().expect("create ghostty render state");
        Self { terminal, render_state, cols, rows, saw_output: false }
    }
}
```

Then add `screen_grid()` to the `VtEngine` impl:

```rust
    fn screen_grid(&mut self) -> Result<ScreenGrid, String> {
        self.render_state.update(&self.terminal)?;
        let cols = self.render_state.get_cols()?;
        let rows = self.render_state.get_rows()?;
        let colors = self.render_state.get_colors()?;

        let default_fg = Rgb { r: colors.foreground.r, g: colors.foreground.g, b: colors.foreground.b };
        let default_bg = Rgb { r: colors.background.r, g: colors.background.g, b: colors.background.b };

        let mut cells = Vec::with_capacity((cols as usize) * (rows as usize));

        let mut row_iter = RowIteratorHandle::new()?;
        self.render_state.populate_row_iterator(&mut row_iter)?;

        let mut row_cells = RowCellsHandle::new()?;
        while row_iter.next() {
            row_iter.populate_cells(&mut row_cells)?;
            while row_cells.next() {
                let graphemes_len = row_cells.get_graphemes_len()?;
                let graphemes = if graphemes_len > 0 {
                    let mut buf = vec![0u32; graphemes_len as usize];
                    row_cells.get_graphemes_buf(&mut buf)?;
                    buf
                } else {
                    Vec::new()
                };

                let fg = match row_cells.get_fg_color()? {
                    Some(c) => Rgb { r: c.r, g: c.g, b: c.b },
                    None => default_fg,
                };
                let bg = match row_cells.get_bg_color()? {
                    Some(c) => Rgb { r: c.r, g: c.g, b: c.b },
                    None => default_bg,
                };

                let style = row_cells.get_style()?;
                let flags = flags_from_ghostty_style(&style);

                cells.push(ResolvedCell { graphemes, fg, bg, flags });
            }
        }

        let cursor = self.read_cursor_state()?;

        // Clear dirty state — cleat is the renderer.
        self.render_state.set_dirty(GhosttyRenderStateDirty::False)?;

        Ok(ScreenGrid { cells, cols, rows, cursor })
    }
```

Add the helper methods (outside the trait impl, as inherent methods):

```rust
impl GhosttyVtEngine {
    // ... existing new() ...

    fn read_cursor_state(&self) -> Result<CursorState, String> {
        let visible = self.render_state.get_cursor_visible()?;
        let in_viewport = self.render_state.get_cursor_viewport_has_value()?;

        if !visible || !in_viewport {
            return Ok(CursorState { visible, ..CursorState::default() });
        }

        let col = self.render_state.get_cursor_viewport_x()?;
        let row = self.render_state.get_cursor_viewport_y()?;
        let style = match self.render_state.get_cursor_visual_style()? {
            GhosttyRenderStateCursorVisualStyle::Bar => CursorStyle::Bar,
            GhosttyRenderStateCursorVisualStyle::Block => CursorStyle::Block,
            GhosttyRenderStateCursorVisualStyle::Underline => CursorStyle::Underline,
            GhosttyRenderStateCursorVisualStyle::BlockHollow => CursorStyle::BlockHollow,
        };

        Ok(CursorState { col, row, visible, style })
    }
}

fn flags_from_ghostty_style(style: &GhosttyStyle) -> CellFlags {
    let mut flags = CellFlags::empty();
    if style.bold { flags |= CellFlags::BOLD; }
    if style.italic { flags |= CellFlags::ITALIC; }
    if style.faint { flags |= CellFlags::FAINT; }
    if style.blink { flags |= CellFlags::BLINK; }
    if style.inverse { flags |= CellFlags::INVERSE; }
    if style.invisible { flags |= CellFlags::INVISIBLE; }
    if style.strikethrough { flags |= CellFlags::STRIKETHROUGH; }
    if style.overline { flags |= CellFlags::OVERLINE; }
    if style.underline != 0 { flags |= CellFlags::UNDERLINE; }
    flags
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --locked --features ghostty-vt -p cleat --test vt vt_ghostty_screen_grid_returns_correct_dimensions`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/cleat/src/vt/ghostty.rs crates/cleat/tests/vt.rs
git commit -m "vt: implement screen_grid() on GhosttyVtEngine via render state FFI"
```

---

### Task 4: Add integration tests for cell content, styles, and cursor

Verify the full round-trip: feed VT sequences, read back resolved cells.

**Files:**
- Modify: `crates/cleat/tests/vt.rs`

- [ ] **Step 1: Write test for cell text content**

```rust
#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_screen_grid_captures_cell_text() {
    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(40, 5);

    engine.feed(b"Hello").expect("feed bytes");

    let grid = engine.screen_grid().expect("screen grid");
    let text: String = (0..5)
        .map(|col| {
            let cell = grid.cell(col, 0).unwrap();
            if cell.graphemes.is_empty() {
                ' '
            } else {
                char::from_u32(cell.graphemes[0]).unwrap_or('?')
            }
        })
        .collect();
    assert_eq!(text, "Hello");
}
```

- [ ] **Step 2: Run test**

Run: `cargo test --locked --features ghostty-vt -p cleat --test vt vt_ghostty_screen_grid_captures_cell_text`
Expected: PASS

- [ ] **Step 3: Write test for bold style flag**

```rust
#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_screen_grid_captures_bold_style() {
    use cleat::vt::CellFlags;

    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(40, 5);

    engine.feed(b"\x1b[1mbold\x1b[0m plain").expect("feed bytes");

    let grid = engine.screen_grid().expect("screen grid");
    // 'b' at col 0 should be bold
    assert!(grid.cell(0, 0).unwrap().flags.contains(CellFlags::BOLD));
    // 'p' at col 5 (after "bold ") should not be bold
    assert!(!grid.cell(5, 0).unwrap().flags.contains(CellFlags::BOLD));
}
```

- [ ] **Step 4: Run test**

Run: `cargo test --locked --features ghostty-vt -p cleat --test vt vt_ghostty_screen_grid_captures_bold_style`
Expected: PASS

- [ ] **Step 5: Write test for cursor position**

```rust
#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_screen_grid_captures_cursor_position() {
    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(40, 5);

    engine.feed(b"Hello").expect("feed bytes");

    let grid = engine.screen_grid().expect("screen grid");
    assert!(grid.cursor.visible);
    assert_eq!(grid.cursor.col, 5);
    assert_eq!(grid.cursor.row, 0);
}
```

- [ ] **Step 6: Run test**

Run: `cargo test --locked --features ghostty-vt -p cleat --test vt vt_ghostty_screen_grid_captures_cursor_position`
Expected: PASS

- [ ] **Step 7: Write test for `row_text()` helper**

```rust
#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_screen_grid_row_text_returns_row_content() {
    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(10, 3);

    engine.feed(b"line one\r\nline two").expect("feed bytes");

    let grid = engine.screen_grid().expect("screen grid");
    assert_eq!(grid.row_text(0).trim_end(), "line one");
    assert_eq!(grid.row_text(1).trim_end(), "line two");
    assert_eq!(grid.row_text(2).trim_end(), "");
}
```

- [ ] **Step 8: Run test**

Run: `cargo test --locked --features ghostty-vt -p cleat --test vt vt_ghostty_screen_grid_row_text`
Expected: PASS

- [ ] **Step 9: Write test for passthrough engine error**

```rust
#[test]
fn vt_passthrough_screen_grid_returns_error() {
    let mut engine = cleat::vt::passthrough::PassthroughVtEngine::new(80, 24);
    let err = engine.screen_grid().expect_err("passthrough should fail");
    assert!(err.contains("placeholder/test-only"));
}
```

- [ ] **Step 10: Run test**

Run: `cargo test --locked -p cleat --test vt vt_passthrough_screen_grid_returns_error`
Expected: PASS

- [ ] **Step 11: Run full test suite**

Run: `cargo test --workspace --locked --features ghostty-vt`
Expected: ALL PASS

- [ ] **Step 12: Run clippy and fmt**

Run: `cargo +nightly-2026-03-12 fmt --check && cargo clippy --workspace --all-targets --locked --features ghostty-vt -- -D warnings`
Expected: CLEAN

- [ ] **Step 13: Commit**

```bash
git add crates/cleat/tests/vt.rs
git commit -m "test: add screen_grid integration tests for text, styles, cursor, and row_text"
```

---

### Task 5: Commit spec update and finalize

**Files:**
- Modified earlier: `docs/specs/2026-03-23-terminal-screen-introspection.md`

- [ ] **Step 1: Run full build + test + lint**

Run: `cargo build --workspace --locked --features ghostty-vt && cargo test --workspace --locked --features ghostty-vt && cargo +nightly-2026-03-12 fmt --check && cargo clippy --workspace --all-targets --locked --features ghostty-vt -- -D warnings`
Expected: ALL PASS

- [ ] **Step 2: Commit spec update**

```bash
git add docs/specs/2026-03-23-terminal-screen-introspection.md
git commit -m "docs: update screen introspection spec for actual Ghostty render state API"
```
