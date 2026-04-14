use std::{ffi::c_void, ptr};

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GhosttyResult {
    Success = 0,
    OutOfMemory = -1,
    InvalidValue = -2,
    OutOfSpace = -3,
    NoValue = -4,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct GhosttyTerminalOptions {
    pub cols: u16,
    pub rows: u16,
    pub max_scrollback: usize,
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub enum GhosttyFormatterFormat {
    Plain = 0,
    Vt = 1,
    Html = 2,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct GhosttyFormatterScreenExtra {
    pub size: usize,
    pub cursor: bool,
    pub style: bool,
    pub hyperlink: bool,
    pub protection: bool,
    pub kitty_keyboard: bool,
    pub charsets: bool,
}

impl GhosttyFormatterScreenExtra {
    pub fn init() -> Self {
        Self {
            size: std::mem::size_of::<Self>(),
            cursor: false,
            style: false,
            hyperlink: false,
            protection: false,
            kitty_keyboard: false,
            charsets: false,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct GhosttyFormatterTerminalExtra {
    pub size: usize,
    pub palette: bool,
    pub modes: bool,
    pub scrolling_region: bool,
    pub tabstops: bool,
    pub pwd: bool,
    pub keyboard: bool,
    pub screen: GhosttyFormatterScreenExtra,
}

impl GhosttyFormatterTerminalExtra {
    pub fn init() -> Self {
        Self {
            size: std::mem::size_of::<Self>(),
            palette: false,
            modes: false,
            scrolling_region: false,
            tabstops: false,
            pwd: false,
            keyboard: false,
            screen: GhosttyFormatterScreenExtra::init(),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct GhosttyFormatterTerminalOptions {
    pub size: usize,
    pub emit: GhosttyFormatterFormat,
    pub unwrap: bool,
    pub trim: bool,
    pub extra: GhosttyFormatterTerminalExtra,
}

impl GhosttyFormatterTerminalOptions {
    pub fn init() -> Self {
        Self {
            size: std::mem::size_of::<Self>(),
            emit: GhosttyFormatterFormat::Vt,
            unwrap: false,
            trim: false,
            extra: GhosttyFormatterTerminalExtra::init(),
        }
    }
}

pub enum GhosttyTerminalOpaque {}
pub enum GhosttyFormatterOpaque {}

pub type GhosttyTerminal = *mut GhosttyTerminalOpaque;
pub type GhosttyFormatter = *mut GhosttyFormatterOpaque;

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

pub type GhosttyCell = u64;

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GhosttyCellData {
    Invalid = 0,
    Codepoint = 1,
    ContentTag = 2,
    Wide = 3,
    HasText = 4,
    HasStyling = 5,
    StyleId = 6,
    HasHyperlink = 7,
    Protected = 8,
    SemanticContent = 9,
    ColorPalette = 10,
    ColorRgb = 11,
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GhosttyCellWide {
    Narrow = 0,
    Wide = 1,
    SpacerTail = 2,
    SpacerHead = 3,
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
        // Safety: zero-init is valid for this repr(C) struct (all numeric/bool fields);
        // we then set the size field for the sized-struct ABI.
        let mut s: Self = unsafe { std::mem::zeroed() };
        s.size = std::mem::size_of::<Self>();
        s
    }
}

// Static asserts: verify Rust layouts match Ghostty's C ABI (from ghostty_type_json()).
const _: () = assert!(std::mem::size_of::<GhosttyStyleColor>() == 16);
const _: () = assert!(std::mem::size_of::<GhosttyStyle>() == 72);
const _: () = assert!(std::mem::size_of::<GhosttyColorRgb>() == 3);
const _: () = assert!(std::mem::size_of::<GhosttyRenderStateColors>() == 792);

pub enum GhosttyRenderStateOpaque {}
pub enum GhosttyRowIteratorOpaque {}
pub enum GhosttyRowCellsOpaque {}

pub type GhosttyRenderState = *mut GhosttyRenderStateOpaque;
pub type GhosttyRenderStateRowIterator = *mut GhosttyRowIteratorOpaque;
pub type GhosttyRenderStateRowCells = *mut GhosttyRowCellsOpaque;

#[link(name = "ghostty-vt")]
unsafe extern "C" {
    fn ghostty_terminal_new(allocator: *const c_void, terminal: *mut GhosttyTerminal, options: GhosttyTerminalOptions) -> GhosttyResult;
    fn ghostty_terminal_free(terminal: GhosttyTerminal);
    fn ghostty_terminal_resize(terminal: GhosttyTerminal, cols: u16, rows: u16, cell_width_px: u32, cell_height_px: u32) -> GhosttyResult;
    fn ghostty_terminal_vt_write(terminal: GhosttyTerminal, data: *const u8, len: usize);

    fn ghostty_formatter_terminal_new(
        allocator: *const c_void,
        formatter: *mut GhosttyFormatter,
        terminal: GhosttyTerminal,
        options: GhosttyFormatterTerminalOptions,
    ) -> GhosttyResult;
    fn ghostty_formatter_format_buf(formatter: GhosttyFormatter, buf: *mut u8, buf_len: usize, out_written: *mut usize) -> GhosttyResult;
    fn ghostty_formatter_free(formatter: GhosttyFormatter);

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
    fn ghostty_render_state_row_get(
        iterator: GhosttyRenderStateRowIterator,
        data: GhosttyRenderStateRowData,
        out: *mut c_void,
    ) -> GhosttyResult;
    #[allow(dead_code)]
    fn ghostty_render_state_row_set(
        iterator: GhosttyRenderStateRowIterator,
        option: GhosttyRenderStateRowOption,
        value: *const c_void,
    ) -> GhosttyResult;

    // --- Row cells ---
    fn ghostty_render_state_row_cells_new(allocator: *const c_void, out_cells: *mut GhosttyRenderStateRowCells) -> GhosttyResult;
    fn ghostty_render_state_row_cells_free(cells: GhosttyRenderStateRowCells);
    fn ghostty_render_state_row_cells_next(cells: GhosttyRenderStateRowCells) -> bool;
    #[allow(dead_code)]
    fn ghostty_render_state_row_cells_select(cells: GhosttyRenderStateRowCells, x: u16) -> GhosttyResult;
    fn ghostty_render_state_row_cells_get(
        cells: GhosttyRenderStateRowCells,
        data: GhosttyRenderStateRowCellsData,
        out: *mut c_void,
    ) -> GhosttyResult;

    // --- Cell data ---
    fn ghostty_cell_get(cell: GhosttyCell, data: GhosttyCellData, out: *mut c_void) -> GhosttyResult;
}

pub struct TerminalHandle {
    raw: GhosttyTerminal,
}

impl TerminalHandle {
    pub fn new(cols: u16, rows: u16, max_scrollback: usize) -> Result<Self, String> {
        let mut raw = ptr::null_mut();
        let result = unsafe { ghostty_terminal_new(ptr::null(), &mut raw, GhosttyTerminalOptions { cols, rows, max_scrollback }) };
        check_result(result, "ghostty_terminal_new")?;
        Ok(Self { raw })
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<(), String> {
        let result = unsafe { ghostty_terminal_resize(self.raw, cols, rows, 1, 1) };
        check_result(result, "ghostty_terminal_resize")
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        unsafe { ghostty_terminal_vt_write(self.raw, bytes.as_ptr(), bytes.len()) };
    }

    pub fn raw(&self) -> GhosttyTerminal {
        self.raw
    }
}

impl Drop for TerminalHandle {
    fn drop(&mut self) {
        unsafe { ghostty_terminal_free(self.raw) };
    }
}

pub fn format_terminal_alloc(terminal: GhosttyTerminal, options: GhosttyFormatterTerminalOptions) -> Result<Vec<u8>, String> {
    let mut formatter = ptr::null_mut();
    let result = unsafe { ghostty_formatter_terminal_new(ptr::null(), &mut formatter, terminal, options) };
    check_result(result, "ghostty_formatter_terminal_new")?;

    let bytes = format_terminal_into_owned_buffer(formatter);
    unsafe { ghostty_formatter_free(formatter) };
    bytes
}

fn format_terminal_into_owned_buffer(formatter: GhosttyFormatter) -> Result<Vec<u8>, String> {
    let mut required = 0usize;
    let result = unsafe { ghostty_formatter_format_buf(formatter, ptr::null_mut(), 0, &mut required) };
    match result {
        GhosttyResult::OutOfSpace => {}
        GhosttyResult::Success => return Ok(Vec::new()),
        other => return check_result(other, "ghostty_formatter_format_buf").map(|_| Vec::new()),
    }

    let mut bytes = vec![0u8; required];
    let mut written = 0usize;
    let result = unsafe { ghostty_formatter_format_buf(formatter, bytes.as_mut_ptr(), bytes.len(), &mut written) };
    check_result(result, "ghostty_formatter_format_buf")?;
    bytes.truncate(written);
    Ok(bytes)
}

fn check_result(result: GhosttyResult, op: &str) -> Result<(), String> {
    match result {
        GhosttyResult::Success => Ok(()),
        GhosttyResult::OutOfMemory => Err(format!("{op} failed: out of memory")),
        GhosttyResult::InvalidValue => Err(format!("{op} failed: invalid value")),
        GhosttyResult::OutOfSpace => Err(format!("{op} failed: out of space")),
        GhosttyResult::NoValue => Err(format!("{op} failed: no value")),
    }
}

// --- RAII wrappers for render state ---

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
        let result = unsafe {
            ghostty_render_state_get(self.raw, GhosttyRenderStateData::Dirty, &mut dirty as *mut GhosttyRenderStateDirty as *mut c_void)
        };
        check_result(result, "ghostty_render_state_get(Dirty)")?;
        Ok(dirty)
    }

    pub fn set_dirty(&mut self, dirty: GhosttyRenderStateDirty) -> Result<(), String> {
        let result = unsafe {
            ghostty_render_state_set(self.raw, GhosttyRenderStateOption::Dirty, &dirty as *const GhosttyRenderStateDirty as *const c_void)
        };
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
        let result =
            unsafe { ghostty_render_state_get(self.raw, GhosttyRenderStateData::CursorVisible, &mut visible as *mut bool as *mut c_void) };
        check_result(result, "ghostty_render_state_get(CursorVisible)")?;
        Ok(visible)
    }

    pub fn get_cursor_viewport_has_value(&self) -> Result<bool, String> {
        let mut has_value = false;
        let result = unsafe {
            ghostty_render_state_get(self.raw, GhosttyRenderStateData::CursorViewportHasValue, &mut has_value as *mut bool as *mut c_void)
        };
        check_result(result, "ghostty_render_state_get(CursorViewportHasValue)")?;
        Ok(has_value)
    }

    pub fn get_cursor_viewport_x(&self) -> Result<u16, String> {
        let mut x: u16 = 0;
        let result =
            unsafe { ghostty_render_state_get(self.raw, GhosttyRenderStateData::CursorViewportX, &mut x as *mut u16 as *mut c_void) };
        check_result(result, "ghostty_render_state_get(CursorViewportX)")?;
        Ok(x)
    }

    pub fn get_cursor_viewport_y(&self) -> Result<u16, String> {
        let mut y: u16 = 0;
        let result =
            unsafe { ghostty_render_state_get(self.raw, GhosttyRenderStateData::CursorViewportY, &mut y as *mut u16 as *mut c_void) };
        check_result(result, "ghostty_render_state_get(CursorViewportY)")?;
        Ok(y)
    }

    pub fn get_cursor_visual_style(&self) -> Result<GhosttyRenderStateCursorVisualStyle, String> {
        let mut style = GhosttyRenderStateCursorVisualStyle::Block;
        let result = unsafe {
            ghostty_render_state_get(
                self.raw,
                GhosttyRenderStateData::CursorVisualStyle,
                &mut style as *mut GhosttyRenderStateCursorVisualStyle as *mut c_void,
            )
        };
        check_result(result, "ghostty_render_state_get(CursorVisualStyle)")?;
        Ok(style)
    }

    pub fn get_cursor_viewport_wide_tail(&self) -> Result<bool, String> {
        let mut wide_tail = false;
        let result = unsafe {
            ghostty_render_state_get(self.raw, GhosttyRenderStateData::CursorViewportWideTail, &mut wide_tail as *mut bool as *mut c_void)
        };
        check_result(result, "ghostty_render_state_get(CursorViewportWideTail)")?;
        Ok(wide_tail)
    }

    pub fn populate_row_iterator(&self, iterator: &mut RowIteratorHandle) -> Result<(), String> {
        let result = unsafe {
            ghostty_render_state_get(
                self.raw,
                GhosttyRenderStateData::RowIterator,
                &mut iterator.raw as *mut GhosttyRenderStateRowIterator as *mut c_void,
            )
        };
        check_result(result, "ghostty_render_state_get(RowIterator)")
    }
}

impl Drop for RenderStateHandle {
    fn drop(&mut self) {
        unsafe { ghostty_render_state_free(self.raw) };
    }
}

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
        let result =
            unsafe { ghostty_render_state_row_get(self.raw, GhosttyRenderStateRowData::Dirty, &mut dirty as *mut bool as *mut c_void) };
        check_result(result, "ghostty_render_state_row_get(Dirty)")?;
        Ok(dirty)
    }

    pub fn populate_cells(&self, cells: &mut RowCellsHandle) -> Result<(), String> {
        let result = unsafe {
            ghostty_render_state_row_get(
                self.raw,
                GhosttyRenderStateRowData::Cells,
                &mut cells.raw as *mut GhosttyRenderStateRowCells as *mut c_void,
            )
        };
        check_result(result, "ghostty_render_state_row_get(Cells)")
    }

    #[allow(dead_code)]
    pub fn set_dirty(&mut self, dirty: bool) -> Result<(), String> {
        let result =
            unsafe { ghostty_render_state_row_set(self.raw, GhosttyRenderStateRowOption::Dirty, &dirty as *const bool as *const c_void) };
        check_result(result, "ghostty_render_state_row_set(Dirty)")
    }
}

impl Drop for RowIteratorHandle {
    fn drop(&mut self) {
        unsafe { ghostty_render_state_row_iterator_free(self.raw) };
    }
}

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
        let result = unsafe {
            ghostty_render_state_row_cells_get(self.raw, GhosttyRenderStateRowCellsData::GraphemesLen, &mut len as *mut u32 as *mut c_void)
        };
        check_result(result, "ghostty_render_state_row_cells_get(GraphemesLen)")?;
        Ok(len)
    }

    pub fn get_graphemes_buf(&self, buf: &mut [u32]) -> Result<(), String> {
        let result = unsafe {
            ghostty_render_state_row_cells_get(self.raw, GhosttyRenderStateRowCellsData::GraphemesBuf, buf.as_mut_ptr() as *mut c_void)
        };
        check_result(result, "ghostty_render_state_row_cells_get(GraphemesBuf)")
    }

    pub fn get_style(&self) -> Result<GhosttyStyle, String> {
        let mut style = GhosttyStyle::init();
        let result = unsafe {
            ghostty_render_state_row_cells_get(
                self.raw,
                GhosttyRenderStateRowCellsData::Style,
                &mut style as *mut GhosttyStyle as *mut c_void,
            )
        };
        check_result(result, "ghostty_render_state_row_cells_get(Style)")?;
        Ok(style)
    }

    pub fn get_bg_color(&self) -> Result<Option<GhosttyColorRgb>, String> {
        let mut color = GhosttyColorRgb::default();
        let result = unsafe {
            ghostty_render_state_row_cells_get(
                self.raw,
                GhosttyRenderStateRowCellsData::BgColor,
                &mut color as *mut GhosttyColorRgb as *mut c_void,
            )
        };
        match result {
            GhosttyResult::Success => Ok(Some(color)),
            GhosttyResult::InvalidValue => Ok(None),
            other => check_result(other, "ghostty_render_state_row_cells_get(BgColor)").map(|_| None),
        }
    }

    pub fn get_fg_color(&self) -> Result<Option<GhosttyColorRgb>, String> {
        let mut color = GhosttyColorRgb::default();
        let result = unsafe {
            ghostty_render_state_row_cells_get(
                self.raw,
                GhosttyRenderStateRowCellsData::FgColor,
                &mut color as *mut GhosttyColorRgb as *mut c_void,
            )
        };
        match result {
            GhosttyResult::Success => Ok(Some(color)),
            GhosttyResult::InvalidValue => Ok(None),
            other => check_result(other, "ghostty_render_state_row_cells_get(FgColor)").map(|_| None),
        }
    }

    pub fn get_raw_cell(&self) -> Result<GhosttyCell, String> {
        let mut cell: GhosttyCell = 0;
        let result = unsafe {
            ghostty_render_state_row_cells_get(self.raw, GhosttyRenderStateRowCellsData::Raw, &mut cell as *mut GhosttyCell as *mut c_void)
        };
        check_result(result, "ghostty_render_state_row_cells_get(Raw)")?;
        Ok(cell)
    }

    pub fn get_wide(&self) -> Result<GhosttyCellWide, String> {
        let cell = self.get_raw_cell()?;
        let mut wide = GhosttyCellWide::Narrow;
        let result = unsafe { ghostty_cell_get(cell, GhosttyCellData::Wide, &mut wide as *mut GhosttyCellWide as *mut c_void) };
        check_result(result, "ghostty_cell_get(Wide)")?;
        Ok(wide)
    }
}

impl Drop for RowCellsHandle {
    fn drop(&mut self) {
        unsafe { ghostty_render_state_row_cells_free(self.raw) };
    }
}
