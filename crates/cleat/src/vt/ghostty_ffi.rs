use std::{collections::BTreeMap, ffi::c_void, io::Cursor, ptr, slice, sync::OnceLock};

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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GhosttyTerminalScrollViewportTag {
    Top = 0,
    Bottom = 1,
    Delta = 2,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union GhosttyTerminalScrollViewportValue {
    pub delta: isize,
    pub _padding: [u64; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GhosttyTerminalScrollViewport {
    pub tag: GhosttyTerminalScrollViewportTag,
    pub value: GhosttyTerminalScrollViewportValue,
}

impl GhosttyTerminalScrollViewport {
    pub fn top() -> Self {
        Self { tag: GhosttyTerminalScrollViewportTag::Top, value: GhosttyTerminalScrollViewportValue { _padding: [0; 2] } }
    }

    pub fn bottom() -> Self {
        Self { tag: GhosttyTerminalScrollViewportTag::Bottom, value: GhosttyTerminalScrollViewportValue { _padding: [0; 2] } }
    }

    pub fn delta(delta: isize) -> Self {
        Self { tag: GhosttyTerminalScrollViewportTag::Delta, value: GhosttyTerminalScrollViewportValue { delta } }
    }
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GhosttyTerminalScreen {
    Primary = 0,
    Alternate = 1,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GhosttyTerminalScrollbar {
    pub total: u64,
    pub offset: u64,
    pub len: u64,
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GhosttyTerminalData {
    Invalid = 0,
    Cols = 1,
    Rows = 2,
    CursorX = 3,
    CursorY = 4,
    CursorPendingWrap = 5,
    ActiveScreen = 6,
    CursorVisible = 7,
    KittyKeyboardFlags = 8,
    Scrollbar = 9,
    CursorStyle = 10,
    MouseTracking = 11,
    Title = 12,
    Pwd = 13,
    TotalRows = 14,
    ScrollbackRows = 15,
    WidthPx = 16,
    HeightPx = 17,
    ColorForeground = 18,
    ColorBackground = 19,
    ColorCursor = 20,
    ColorPalette = 21,
    ColorForegroundDefault = 22,
    ColorBackgroundDefault = 23,
    ColorCursorDefault = 24,
    ColorPaletteDefault = 25,
    KittyImageStorageLimit = 26,
    KittyImageMediumFile = 27,
    KittyImageMediumTempFile = 28,
    KittyImageMediumSharedMem = 29,
    KittyGraphics = 30,
}

pub type GhosttyMode = u16;

pub const GHOSTTY_MODE_DECCKM: GhosttyMode = 1;
pub const GHOSTTY_MODE_MOUSE_X10: GhosttyMode = 9;
pub const GHOSTTY_MODE_MOUSE_NORMAL: GhosttyMode = 1000;
pub const GHOSTTY_MODE_MOUSE_BUTTON: GhosttyMode = 1002;
pub const GHOSTTY_MODE_MOUSE_ANY: GhosttyMode = 1003;
pub const GHOSTTY_MODE_SGR_MOUSE: GhosttyMode = 1006;
pub const GHOSTTY_MODE_ALT_SCROLL: GhosttyMode = 1007;
pub const GHOSTTY_MODE_SGR_PIXELS_MOUSE: GhosttyMode = 1016;
pub const GHOSTTY_MODE_BRACKETED_PASTE: GhosttyMode = 2004;

// Paste encoding (libghostty `ghostty/vt/paste.h`). Wraps paste data in
// bracketed-paste markers when the program enabled mode 2004, strips unsafe
// bytes, and normalizes newlines, so Cleat never hand-rolls this.
unsafe extern "C" {
    fn ghostty_paste_encode(
        data: *mut u8,
        data_len: usize,
        bracketed: bool,
        out_buf: *mut u8,
        buf_len: usize,
        out_written: *mut usize,
    ) -> GhosttyResult;
}

/// Encode paste `text` for the PTY, bracketing it when `bracketed` is set.
pub fn paste_encode(text: &[u8], bracketed: bool) -> Vec<u8> {
    let mut input = text.to_vec();
    unsafe {
        let mut needed: usize = 0;
        // Query the required length (out buffer null/0 => out_of_space + size).
        // The Ghostty encoder may sanitize the input buffer while sizing, so
        // always pass owned mutable bytes.
        match ghostty_paste_encode(input.as_mut_ptr(), input.len(), bracketed, ptr::null_mut(), 0, &mut needed) {
            GhosttyResult::Success if needed == 0 => return Vec::new(),
            GhosttyResult::Success | GhosttyResult::OutOfSpace => {}
            _ => return text.to_vec(),
        }
        let mut out = vec![0u8; needed];
        let mut written: usize = 0;
        match ghostty_paste_encode(input.as_mut_ptr(), input.len(), bracketed, out.as_mut_ptr(), out.len(), &mut written) {
            GhosttyResult::Success => {
                out.truncate(written);
                out
            }
            // Defensive: preserve the paste without reintroducing bytes that
            // the sizing call already sanitized.
            _ => input,
        }
    }
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
    pub selection: *const c_void,
}

impl GhosttyFormatterTerminalOptions {
    pub fn init() -> Self {
        Self {
            size: std::mem::size_of::<Self>(),
            emit: GhosttyFormatterFormat::Vt,
            unwrap: false,
            trim: false,
            extra: GhosttyFormatterTerminalExtra::init(),
            selection: ptr::null(),
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
    Selected = 7,
    HasStyling = 8,
    GraphemesUtf8 = 9,
}

pub type GhosttyCell = u64;
pub type GhosttyRow = u64;

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GhosttyCellContentTag {
    Codepoint = 0,
    CodepointGrapheme = 1,
    BgColorPalette = 2,
    BgColorRgb = 3,
}

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

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GhosttyCellSemanticContent {
    Output = 0,
    Input = 1,
    Prompt = 2,
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GhosttyRowSemanticPrompt {
    None = 0,
    Prompt = 1,
    PromptContinuation = 2,
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GhosttyRowData {
    Invalid = 0,
    Wrap = 1,
    WrapContinuation = 2,
    Grapheme = 3,
    Styled = 4,
    Hyperlink = 5,
    SemanticPrompt = 6,
    KittyVirtualPlaceholder = 7,
    Dirty = 8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct GhosttyColorRgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct GhosttyBuffer {
    pub ptr: *mut u8,
    pub cap: usize,
    pub len: usize,
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

/// Selector for `ghostty_terminal_set`. C defines more variants (PWD and color options)
/// that cleat does not currently configure.
#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GhosttyTerminalOption {
    Userdata = 0,
    WritePty = 1,
    Bell = 2,
    Enquiry = 3,
    Xtversion = 4,
    TitleChanged = 5,
    Size = 6,
    ColorScheme = 7,
    DeviceAttributes = 8,
    Title = 9,
    Pwd = 10,
    ColorForeground = 11,
    ColorBackground = 12,
    ColorCursor = 13,
    ColorPalette = 14,
    KittyImageStorageLimit = 15,
    KittyImageMediumFile = 16,
    KittyImageMediumTempFile = 17,
    KittyImageMediumSharedMem = 18,
}

/// Callback fired synchronously from `ghostty_terminal_vt_write` when the
/// terminal wants to send reply bytes back to the pty (DSR, DECRQM, DA, ...).
pub type GhosttyTerminalWritePtyFn = unsafe extern "C" fn(terminal: GhosttyTerminal, userdata: *mut c_void, data: *const u8, len: usize);

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GhosttySizeReportSize {
    pub rows: u16,
    pub columns: u16,
    pub cell_width: u32,
    pub cell_height: u32,
}

pub type GhosttyTerminalSizeFn =
    unsafe extern "C" fn(terminal: GhosttyTerminal, userdata: *mut c_void, out_size: *mut GhosttySizeReportSize) -> bool;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct GhosttyDeviceAttributesPrimary {
    pub conformance_level: u16,
    pub features: [u16; 64],
    pub num_features: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct GhosttyDeviceAttributesSecondary {
    pub device_type: u16,
    pub firmware_version: u16,
    pub rom_cartridge: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct GhosttyDeviceAttributesTertiary {
    pub unit_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct GhosttyDeviceAttributes {
    pub primary: GhosttyDeviceAttributesPrimary,
    pub secondary: GhosttyDeviceAttributesSecondary,
    pub tertiary: GhosttyDeviceAttributesTertiary,
}

/// Callback fired when ghostty receives a DA1/DA2/DA3 query. The app fills
/// `*out_attrs` with the response shape it wants to advertise. Return true
/// to emit, false to silently drop.
pub type GhosttyTerminalDeviceAttributesFn =
    unsafe extern "C" fn(terminal: GhosttyTerminal, userdata: *mut c_void, out_attrs: *mut GhosttyDeviceAttributes) -> bool;

// Computed for 64-bit targets: 2 (u16) + 128 ([u16; 64]) + 6 bytes trailing padding
// to re-align to usize (8) + 8 (usize num_features) = 144. 32-bit targets would be 136.
// This assert targets 64-bit (cleat's supported build targets).
const _: () = assert!(std::mem::size_of::<GhosttyDeviceAttributesPrimary>() == 144);
const _: () = assert!(std::mem::size_of::<GhosttyDeviceAttributesSecondary>() == 6);
const _: () = assert!(std::mem::size_of::<GhosttyDeviceAttributesTertiary>() == 4);
const _: () = assert!(std::mem::size_of::<GhosttyDeviceAttributes>() == 160);

// Static asserts: verify Rust layouts match Ghostty's C ABI (from ghostty_type_json()).
const _: () = assert!(std::mem::size_of::<GhosttyStyleColor>() == 16);
const _: () = assert!(std::mem::size_of::<GhosttyStyle>() == 72);
const _: () = assert!(std::mem::size_of::<GhosttyColorRgb>() == 3);
const _: () = assert!(std::mem::size_of::<GhosttyBuffer>() == 24);
const _: () = assert!(std::mem::size_of::<GhosttyRenderStateColors>() == 792);
const _: () = assert!(std::mem::size_of::<GhosttyTerminalScrollViewport>() == 24);
const _: () = assert!(std::mem::size_of::<GhosttyTerminalScrollbar>() == 24);

pub enum GhosttyRenderStateOpaque {}
pub enum GhosttyRowIteratorOpaque {}
pub enum GhosttyRowCellsOpaque {}
#[allow(dead_code)]
pub enum GhosttyKittyGraphicsOpaque {}
#[allow(dead_code)]
pub enum GhosttyKittyGraphicsImageOpaque {}
#[allow(dead_code)]
pub enum GhosttyKittyGraphicsPlacementIteratorOpaque {}
#[allow(dead_code)]
pub enum GhosttyKittyGraphicsVirtualPlacementIteratorOpaque {}

pub type GhosttyRenderState = *mut GhosttyRenderStateOpaque;
pub type GhosttyRenderStateRowIterator = *mut GhosttyRowIteratorOpaque;
pub type GhosttyRenderStateRowCells = *mut GhosttyRowCellsOpaque;
#[allow(dead_code)]
pub type GhosttyKittyGraphics = *mut GhosttyKittyGraphicsOpaque;
#[allow(dead_code)]
pub type GhosttyKittyGraphicsImage = *const GhosttyKittyGraphicsImageOpaque;
#[allow(dead_code)]
pub type GhosttyKittyGraphicsPlacementIterator = *mut GhosttyKittyGraphicsPlacementIteratorOpaque;
#[allow(dead_code)]
pub type GhosttyKittyGraphicsVirtualPlacementIterator = *mut GhosttyKittyGraphicsVirtualPlacementIteratorOpaque;

pub enum GhosttyAllocator {}

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GhosttySysOption {
    Userdata = 0,
    DecodePng = 1,
    Log = 2,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GhosttySysImage {
    pub width: u32,
    pub height: u32,
    pub data: *mut u8,
    pub data_len: usize,
}

pub type GhosttySysDecodePngFn = unsafe extern "C" fn(
    userdata: *mut c_void,
    allocator: *const GhosttyAllocator,
    data: *const u8,
    data_len: usize,
    out: *mut GhosttySysImage,
) -> bool;

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GhosttyKittyGraphicsData {
    Invalid = 0,
    PlacementIterator = 1,
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GhosttyKittyGraphicsPlacementData {
    Invalid = 0,
    ImageId = 1,
    PlacementId = 2,
    IsVirtual = 3,
    XOffset = 4,
    YOffset = 5,
    SourceX = 6,
    SourceY = 7,
    SourceWidth = 8,
    SourceHeight = 9,
    Columns = 10,
    Rows = 11,
    Z = 12,
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GhosttyKittyGraphicsImageData {
    Invalid = 0,
    Id = 1,
    Number = 2,
    Width = 3,
    Height = 4,
    Format = 5,
    Compression = 6,
    DataPtr = 7,
    DataLen = 8,
    Generation = 9,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GhosttyKittyGraphicsPlacementRenderInfo {
    pub size: usize,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub grid_cols: u32,
    pub grid_rows: u32,
    pub viewport_col: i32,
    pub viewport_row: i32,
    pub viewport_visible: bool,
    pub source_x: u32,
    pub source_y: u32,
    pub source_width: u32,
    pub source_height: u32,
}

impl GhosttyKittyGraphicsPlacementRenderInfo {
    fn init() -> Self {
        Self { size: std::mem::size_of::<Self>(), ..Self::default() }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GhosttyKittyGraphicsVirtualPlacementInfo {
    pub size: usize,
    pub image_id: u32,
    pub placement_id: u32,
    pub z: i32,
    pub viewport_col: i32,
    pub viewport_row: i32,
    pub grid_cols: u32,
    pub grid_rows: u32,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub source_x: u32,
    pub source_y: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub x_offset: u32,
    pub y_offset: u32,
}

impl GhosttyKittyGraphicsVirtualPlacementInfo {
    fn init() -> Self {
        Self { size: std::mem::size_of::<Self>(), ..Self::default() }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KittyImageResourceInfo {
    pub image_id: u32,
    pub generation: u64,
    pub width_px: u32,
    pub height_px: u32,
    pub format: u32,
    pub compression: u32,
    pub data_len: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KittyImagePlacementInfo {
    pub image_id: u32,
    pub generation: u64,
    pub placement_id: u32,
    pub is_virtual: bool,
    pub z: i32,
    pub viewport_col: i32,
    pub viewport_row: i32,
    pub grid_cols: u32,
    pub grid_rows: u32,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub source_x: u32,
    pub source_y: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub x_offset_px: u32,
    pub y_offset_px: u32,
}

unsafe extern "C" {
    fn ghostty_alloc(allocator: *const GhosttyAllocator, len: usize) -> *mut u8;
    fn ghostty_sys_set(option: GhosttySysOption, value: *const c_void) -> GhosttyResult;
    fn ghostty_terminal_new(allocator: *const c_void, terminal: *mut GhosttyTerminal, options: GhosttyTerminalOptions) -> GhosttyResult;
    fn ghostty_terminal_free(terminal: GhosttyTerminal);
    fn ghostty_terminal_resize(terminal: GhosttyTerminal, cols: u16, rows: u16, cell_width_px: u32, cell_height_px: u32) -> GhosttyResult;
    fn ghostty_terminal_vt_write(terminal: GhosttyTerminal, data: *const u8, len: usize);
    fn ghostty_terminal_scroll_viewport(terminal: GhosttyTerminal, behavior: GhosttyTerminalScrollViewport);
    fn ghostty_terminal_get(terminal: GhosttyTerminal, data: GhosttyTerminalData, out: *mut c_void) -> GhosttyResult;
    fn ghostty_terminal_mode_get(terminal: GhosttyTerminal, mode: GhosttyMode, out_value: *mut bool) -> GhosttyResult;
    fn ghostty_terminal_set(terminal: GhosttyTerminal, option: GhosttyTerminalOption, value: *const c_void) -> GhosttyResult;
    #[allow(dead_code)]
    fn ghostty_kitty_graphics_get(graphics: GhosttyKittyGraphics, data: GhosttyKittyGraphicsData, out: *mut c_void) -> GhosttyResult;
    #[allow(dead_code)]
    fn ghostty_kitty_graphics_image(graphics: GhosttyKittyGraphics, image_id: u32) -> GhosttyKittyGraphicsImage;
    #[allow(dead_code)]
    fn ghostty_kitty_graphics_image_get(
        image: GhosttyKittyGraphicsImage,
        data: GhosttyKittyGraphicsImageData,
        out: *mut c_void,
    ) -> GhosttyResult;
    #[allow(dead_code)]
    fn ghostty_kitty_graphics_placement_iterator_new(
        allocator: *const c_void,
        out_iterator: *mut GhosttyKittyGraphicsPlacementIterator,
    ) -> GhosttyResult;
    #[allow(dead_code)]
    fn ghostty_kitty_graphics_placement_iterator_free(iterator: GhosttyKittyGraphicsPlacementIterator);
    #[allow(dead_code)]
    fn ghostty_kitty_graphics_placement_next(iterator: GhosttyKittyGraphicsPlacementIterator) -> bool;
    #[allow(dead_code)]
    fn ghostty_kitty_graphics_placement_get(
        iterator: GhosttyKittyGraphicsPlacementIterator,
        data: GhosttyKittyGraphicsPlacementData,
        out: *mut c_void,
    ) -> GhosttyResult;
    #[allow(dead_code)]
    fn ghostty_kitty_graphics_placement_render_info(
        iterator: GhosttyKittyGraphicsPlacementIterator,
        image: GhosttyKittyGraphicsImage,
        terminal: GhosttyTerminal,
        out_info: *mut GhosttyKittyGraphicsPlacementRenderInfo,
    ) -> GhosttyResult;
    #[allow(dead_code)]
    fn ghostty_kitty_graphics_virtual_placement_iterator_new(
        allocator: *const c_void,
        out_iterator: *mut GhosttyKittyGraphicsVirtualPlacementIterator,
    ) -> GhosttyResult;
    #[allow(dead_code)]
    fn ghostty_kitty_graphics_virtual_placement_iterator_free(iterator: GhosttyKittyGraphicsVirtualPlacementIterator);
    #[allow(dead_code)]
    fn ghostty_kitty_graphics_virtual_placement_iterator_reset(
        iterator: GhosttyKittyGraphicsVirtualPlacementIterator,
        terminal: GhosttyTerminal,
    ) -> GhosttyResult;
    #[allow(dead_code)]
    fn ghostty_kitty_graphics_virtual_placement_next(
        iterator: GhosttyKittyGraphicsVirtualPlacementIterator,
        out_info: *mut GhosttyKittyGraphicsVirtualPlacementInfo,
    ) -> GhosttyResult;

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
    fn ghostty_render_state_row_cells_get_multi(
        cells: GhosttyRenderStateRowCells,
        count: usize,
        keys: *const GhosttyRenderStateRowCellsData,
        values: *mut *mut c_void,
        out_written: *mut usize,
    ) -> GhosttyResult;

    // --- Cell data ---
    fn ghostty_cell_get_multi(
        cell: GhosttyCell,
        count: usize,
        keys: *const GhosttyCellData,
        values: *mut *mut c_void,
        out_written: *mut usize,
    ) -> GhosttyResult;
    fn ghostty_row_get(row: GhosttyRow, data: GhosttyRowData, out: *mut c_void) -> GhosttyResult;
}

// ---------------------------------------------------------------------------
// Mouse encoding (libghostty `ghostty/vt/mouse.h`).
//
// The encoder owns the wire protocol: it gates events against the terminal's
// live tracking mode, dedupes motion, picks the output format (SGR / SGR-pixels
// / X10 / urxvt), and converts surface-space pixel positions into cell or pixel
// coordinates from the renderer size. Cleat just feeds it events + the size.
// ---------------------------------------------------------------------------

pub enum GhosttyMouseEncoderOpaque {}
pub type GhosttyMouseEncoder = *mut GhosttyMouseEncoderOpaque;
pub enum GhosttyMouseEventOpaque {}
pub type GhosttyMouseEvent = *mut GhosttyMouseEventOpaque;

/// Keyboard modifier bitmask (`GhosttyMods` = uint16). Only shift/ctrl/alt
/// affect mouse encoding. See `ghostty/vt/key/event.h`.
pub type GhosttyMods = u16;
pub const GHOSTTY_MODS_SHIFT: GhosttyMods = 1 << 0;
pub const GHOSTTY_MODS_CTRL: GhosttyMods = 1 << 1;
pub const GHOSTTY_MODS_ALT: GhosttyMods = 1 << 2;

#[repr(C)]
#[derive(Clone, Copy)]
pub enum GhosttyMouseAction {
    Press = 0,
    Release = 1,
    Motion = 2,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub enum GhosttyMouseButton {
    Left = 1,
    Right = 2,
    Middle = 3,
    Four = 4,
    Five = 5,
    Six = 6,
    Seven = 7,
    Eight = 8,
    Nine = 9,
}

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub enum GhosttyMouseEncoderOption {
    Event = 0,
    Format = 1,
    Size = 2,
    AnyButtonPressed = 3,
    TrackLastCell = 4,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GhosttyMousePosition {
    pub x: f32,
    pub y: f32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GhosttyMouseEncoderSize {
    pub size: usize,
    pub screen_width: u32,
    pub screen_height: u32,
    pub cell_width: u32,
    pub cell_height: u32,
    pub padding_top: u32,
    pub padding_bottom: u32,
    pub padding_right: u32,
    pub padding_left: u32,
}

pub struct MouseEncodeEvent {
    pub action: GhosttyMouseAction,
    pub button: Option<GhosttyMouseButton>,
    pub any_button_pressed: bool,
    pub mods: GhosttyMods,
    pub x_px: f32,
    pub y_px: f32,
}

unsafe extern "C" {
    fn ghostty_mouse_encoder_new(allocator: *const c_void, encoder: *mut GhosttyMouseEncoder) -> GhosttyResult;
    fn ghostty_mouse_encoder_free(encoder: GhosttyMouseEncoder);
    fn ghostty_mouse_encoder_setopt(encoder: GhosttyMouseEncoder, option: GhosttyMouseEncoderOption, value: *const c_void);
    fn ghostty_mouse_encoder_setopt_from_terminal(encoder: GhosttyMouseEncoder, terminal: GhosttyTerminal);
    fn ghostty_mouse_encoder_encode(
        encoder: GhosttyMouseEncoder,
        event: GhosttyMouseEvent,
        out_buf: *mut u8,
        out_buf_size: usize,
        out_len: *mut usize,
    ) -> GhosttyResult;
    fn ghostty_mouse_event_new(allocator: *const c_void, event: *mut GhosttyMouseEvent) -> GhosttyResult;
    fn ghostty_mouse_event_free(event: GhosttyMouseEvent);
    fn ghostty_mouse_event_set_action(event: GhosttyMouseEvent, action: GhosttyMouseAction);
    fn ghostty_mouse_event_set_button(event: GhosttyMouseEvent, button: GhosttyMouseButton);
    fn ghostty_mouse_event_clear_button(event: GhosttyMouseEvent);
    fn ghostty_mouse_event_set_mods(event: GhosttyMouseEvent, mods: GhosttyMods);
    fn ghostty_mouse_event_set_position(event: GhosttyMouseEvent, position: GhosttyMousePosition);
}

/// Owns a libghostty mouse encoder + a reusable event handle.
pub struct MouseEncoder {
    encoder: GhosttyMouseEncoder,
    event: GhosttyMouseEvent,
}

// SAFETY: the raw handles are owned solely by this struct and only touched
// while `&mut self` is held (the in-process actor thread owns the engine).
unsafe impl Send for MouseEncoder {}

impl MouseEncoder {
    pub fn new() -> Result<Self, String> {
        let mut encoder: GhosttyMouseEncoder = ptr::null_mut();
        check_result(unsafe { ghostty_mouse_encoder_new(ptr::null(), &mut encoder) }, "ghostty_mouse_encoder_new")?;
        let mut event: GhosttyMouseEvent = ptr::null_mut();
        if let Err(err) = check_result(unsafe { ghostty_mouse_event_new(ptr::null(), &mut event) }, "ghostty_mouse_event_new") {
            unsafe { ghostty_mouse_encoder_free(encoder) };
            return Err(err);
        }
        // Dedupe motion: suppress motion reports until the target cell changes
        // (the encoder exempts sgr-pixels internally).
        let track = true;
        unsafe { ghostty_mouse_encoder_setopt(encoder, GhosttyMouseEncoderOption::TrackLastCell, (&track as *const bool).cast()) };
        Ok(Self { encoder, event })
    }

    /// Push the renderer geometry used to map surface pixels to cells / pixels.
    pub fn set_size(&mut self, screen_width: u32, screen_height: u32, cell_width: u32, cell_height: u32) {
        let size = GhosttyMouseEncoderSize {
            size: std::mem::size_of::<GhosttyMouseEncoderSize>(),
            screen_width,
            screen_height,
            cell_width: cell_width.max(1),
            cell_height: cell_height.max(1),
            padding_top: 0,
            padding_bottom: 0,
            padding_right: 0,
            padding_left: 0,
        };
        unsafe {
            ghostty_mouse_encoder_setopt(self.encoder, GhosttyMouseEncoderOption::Size, (&size as *const GhosttyMouseEncoderSize).cast())
        };
    }

    /// Encode one event, syncing tracking mode + format from `terminal`.
    /// Returns the report bytes (empty when the encoder gates the event out).
    pub fn encode(&mut self, terminal: GhosttyTerminal, event: MouseEncodeEvent) -> Vec<u8> {
        unsafe {
            ghostty_mouse_encoder_setopt_from_terminal(self.encoder, terminal);
            let any = event.any_button_pressed;
            ghostty_mouse_encoder_setopt(self.encoder, GhosttyMouseEncoderOption::AnyButtonPressed, (&any as *const bool).cast());
            ghostty_mouse_event_set_action(self.event, event.action);
            match event.button {
                Some(button) => ghostty_mouse_event_set_button(self.event, button),
                None => ghostty_mouse_event_clear_button(self.event),
            }
            ghostty_mouse_event_set_mods(self.event, event.mods);
            ghostty_mouse_event_set_position(self.event, GhosttyMousePosition { x: event.x_px, y: event.y_px });

            let mut buf = [0u8; 64];
            let mut written: usize = 0;
            match ghostty_mouse_encoder_encode(self.encoder, self.event, buf.as_mut_ptr(), buf.len(), &mut written) {
                GhosttyResult::Success => buf[..written.min(buf.len())].to_vec(),
                GhosttyResult::OutOfSpace => {
                    // Mouse reports never exceed ~20 bytes, but honor the contract.
                    let mut big = vec![0u8; written];
                    let mut w2: usize = 0;
                    if ghostty_mouse_encoder_encode(self.encoder, self.event, big.as_mut_ptr(), big.len(), &mut w2)
                        == GhosttyResult::Success
                    {
                        big.truncate(w2);
                        big
                    } else {
                        Vec::new()
                    }
                }
                _ => Vec::new(),
            }
        }
    }
}

impl Drop for MouseEncoder {
    fn drop(&mut self) {
        unsafe {
            ghostty_mouse_event_free(self.event);
            ghostty_mouse_encoder_free(self.encoder);
        }
    }
}

pub struct TerminalHandle {
    raw: GhosttyTerminal,
    /// Heap-allocated so the address stays stable while the C side holds
    /// a pointer to it via userdata.
    effects: Box<TerminalEffects>,
}

struct TerminalEffects {
    reply_buf: Vec<u8>,
    cols: u16,
    rows: u16,
    cell_width_px: u32,
    cell_height_px: u32,
}

/// DA1 feature code for ANSI color (see device.h: GHOSTTY_DA_FEATURE_ANSI_COLOR).
const DA_FEATURE_ANSI_COLOR: u16 = 22;
/// VT220 conformance (see device.h: GHOSTTY_DA_CONFORMANCE_VT220).
const DA_CONFORMANCE_VT220: u16 = 62;
/// VT220 device type for DA2 (see device.h: GHOSTTY_DA_DEVICE_TYPE_VT220).
const DA_DEVICE_TYPE_VT220: u16 = 1;
/// DA2 firmware version. Matches cleat's pre-existing synthetic reply.
const DA_FIRMWARE_VERSION: u16 = 10;

unsafe extern "C" fn device_attributes_trampoline(
    _terminal: GhosttyTerminal,
    _userdata: *mut c_void,
    out_attrs: *mut GhosttyDeviceAttributes,
) -> bool {
    if out_attrs.is_null() {
        return false;
    }
    let mut features = [0u16; 64];
    features[0] = DA_FEATURE_ANSI_COLOR;
    let attrs = GhosttyDeviceAttributes {
        primary: GhosttyDeviceAttributesPrimary { conformance_level: DA_CONFORMANCE_VT220, features, num_features: 1 },
        secondary: GhosttyDeviceAttributesSecondary {
            device_type: DA_DEVICE_TYPE_VT220,
            firmware_version: DA_FIRMWARE_VERSION,
            rom_cartridge: 0,
        },
        tertiary: GhosttyDeviceAttributesTertiary { unit_id: 0 },
    };
    unsafe { *out_attrs = attrs };
    true
}

unsafe extern "C" fn write_pty_trampoline(_terminal: GhosttyTerminal, userdata: *mut c_void, data: *const u8, len: usize) {
    if userdata.is_null() || data.is_null() || len == 0 {
        return;
    }
    // SAFETY: userdata is the raw pointer to a Box<TerminalEffects> we
    // registered when constructing this terminal; ghostty calls us
    // synchronously from vt_write, so the Box is live for the duration.
    let effects = unsafe { &mut *(userdata as *mut TerminalEffects) };
    let slice = unsafe { std::slice::from_raw_parts(data, len) };
    effects.reply_buf.extend_from_slice(slice);
}

unsafe extern "C" fn size_trampoline(_terminal: GhosttyTerminal, userdata: *mut c_void, out_size: *mut GhosttySizeReportSize) -> bool {
    if userdata.is_null() || out_size.is_null() {
        return false;
    }
    let effects = unsafe { &*(userdata as *const TerminalEffects) };
    unsafe {
        *out_size = GhosttySizeReportSize {
            rows: effects.rows,
            columns: effects.cols,
            cell_width: effects.cell_width_px.max(1),
            cell_height: effects.cell_height_px.max(1),
        };
    }
    true
}

static GHOSTTY_SYS_INIT: OnceLock<Result<(), String>> = OnceLock::new();

fn ensure_sys_callbacks() -> Result<(), String> {
    GHOSTTY_SYS_INIT
        .get_or_init(|| {
            let decode_png_cb: GhosttySysDecodePngFn = decode_png_trampoline;
            let result = unsafe { ghostty_sys_set(GhosttySysOption::DecodePng, decode_png_cb as *const c_void) };
            check_result(result, "ghostty_sys_set(DecodePng)")
        })
        .clone()
}

unsafe extern "C" fn decode_png_trampoline(
    _userdata: *mut c_void,
    allocator: *const GhosttyAllocator,
    data: *const u8,
    data_len: usize,
    out: *mut GhosttySysImage,
) -> bool {
    if data.is_null() || out.is_null() {
        return false;
    }
    let png_bytes = unsafe { slice::from_raw_parts(data, data_len) };
    let decoded = match decode_png_rgba(png_bytes) {
        Ok(decoded) => decoded,
        Err(_) => return false,
    };
    let Some(data_len) = decoded.width.checked_mul(decoded.height).and_then(|px| px.checked_mul(4)).map(|len| len as usize) else {
        return false;
    };
    if decoded.rgba.len() != data_len {
        return false;
    }
    let out_data = unsafe { ghostty_alloc(allocator, data_len) };
    if out_data.is_null() {
        return false;
    }
    unsafe {
        ptr::copy_nonoverlapping(decoded.rgba.as_ptr(), out_data, data_len);
        *out = GhosttySysImage { width: decoded.width, height: decoded.height, data: out_data, data_len };
    }
    true
}

struct DecodedPng {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

fn decode_png_rgba(data: &[u8]) -> Result<DecodedPng, String> {
    let mut decoder = png::Decoder::new(Cursor::new(data));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|err| err.to_string())?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).map_err(|err| err.to_string())?;
    let bytes = &buf[..info.buffer_size()];
    let rgba = match info.color_type {
        png::ColorType::Rgba => bytes.to_vec(),
        png::ColorType::Rgb => {
            let mut out = Vec::with_capacity(pixel_count(info.width, info.height)? * 4);
            for rgb in bytes.as_chunks::<3>().0 {
                out.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
            }
            out
        }
        png::ColorType::GrayscaleAlpha => {
            let mut out = Vec::with_capacity(pixel_count(info.width, info.height)? * 4);
            for gray_alpha in bytes.as_chunks::<2>().0 {
                out.extend_from_slice(&[gray_alpha[0], gray_alpha[0], gray_alpha[0], gray_alpha[1]]);
            }
            out
        }
        png::ColorType::Grayscale => {
            let mut out = Vec::with_capacity(pixel_count(info.width, info.height)? * 4);
            for gray in bytes {
                out.extend_from_slice(&[*gray, *gray, *gray, 255]);
            }
            out
        }
        png::ColorType::Indexed => return Err("indexed PNG was not expanded by decoder".to_string()),
    };
    Ok(DecodedPng { width: info.width, height: info.height, rgba })
}

fn pixel_count(width: u32, height: u32) -> Result<usize, String> {
    width.checked_mul(height).and_then(|pixels| usize::try_from(pixels).ok()).ok_or_else(|| "PNG dimensions overflow".to_string())
}

impl TerminalHandle {
    /// Raw handle, for C APIs that read live terminal state (e.g. the mouse
    /// encoder's `setopt_from_terminal`). The handle stays valid for `&self`.
    pub fn raw_terminal(&self) -> GhosttyTerminal {
        self.raw
    }

    pub fn new(cols: u16, rows: u16, max_scrollback: usize) -> Result<Self, String> {
        ensure_sys_callbacks()?;
        let mut raw = ptr::null_mut();
        let result = unsafe { ghostty_terminal_new(ptr::null(), &mut raw, GhosttyTerminalOptions { cols, rows, max_scrollback }) };
        check_result(result, "ghostty_terminal_new")?;

        let mut effects = Box::new(TerminalEffects { reply_buf: Vec::new(), cols, rows, cell_width_px: 1, cell_height_px: 1 });
        let userdata_ptr: *mut c_void = (&mut *effects as *mut TerminalEffects).cast();

        let set_user = unsafe { ghostty_terminal_set(raw, GhosttyTerminalOption::Userdata, userdata_ptr as *const c_void) };
        if let Err(err) = check_result(set_user, "ghostty_terminal_set(Userdata)") {
            unsafe { ghostty_terminal_free(raw) };
            return Err(err);
        }

        let write_pty_cb: GhosttyTerminalWritePtyFn = write_pty_trampoline;
        // Libghostty's WritePty option takes the function pointer *by value*
        // (coerced through *const c_void), not a pointer to a function pointer.
        // Passing &cb as *const _ here triggers a SIGBUS from a bogus fn-ptr
        // dereference inside vt_write.
        let set_wp = unsafe { ghostty_terminal_set(raw, GhosttyTerminalOption::WritePty, write_pty_cb as *const c_void) };
        if let Err(err) = check_result(set_wp, "ghostty_terminal_set(WritePty)") {
            unsafe { ghostty_terminal_free(raw) };
            return Err(err);
        }

        let da_cb: GhosttyTerminalDeviceAttributesFn = device_attributes_trampoline;
        let set_da = unsafe { ghostty_terminal_set(raw, GhosttyTerminalOption::DeviceAttributes, da_cb as *const c_void) };
        if let Err(err) = check_result(set_da, "ghostty_terminal_set(DeviceAttributes)") {
            unsafe { ghostty_terminal_free(raw) };
            return Err(err);
        }

        let size_cb: GhosttyTerminalSizeFn = size_trampoline;
        let set_size = unsafe { ghostty_terminal_set(raw, GhosttyTerminalOption::Size, size_cb as *const c_void) };
        if let Err(err) = check_result(set_size, "ghostty_terminal_set(Size)") {
            unsafe { ghostty_terminal_free(raw) };
            return Err(err);
        }

        Ok(Self { raw, effects })
    }

    pub fn resize(&mut self, cols: u16, rows: u16, cell_width_px: u32, cell_height_px: u32) -> Result<(), String> {
        let cell_width_px = cell_width_px.max(1);
        let cell_height_px = cell_height_px.max(1);
        let result = unsafe { ghostty_terminal_resize(self.raw, cols, rows, cell_width_px, cell_height_px) };
        check_result(result, "ghostty_terminal_resize")?;
        self.effects.cols = cols;
        self.effects.rows = rows;
        self.effects.cell_width_px = cell_width_px;
        self.effects.cell_height_px = cell_height_px;
        Ok(())
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        unsafe { ghostty_terminal_vt_write(self.raw, bytes.as_ptr(), bytes.len()) };
    }

    pub fn set_default_foreground(&mut self, color: Option<GhosttyColorRgb>) -> Result<(), String> {
        self.set_color(GhosttyTerminalOption::ColorForeground, color, "ColorForeground")
    }

    pub fn set_default_background(&mut self, color: Option<GhosttyColorRgb>) -> Result<(), String> {
        self.set_color(GhosttyTerminalOption::ColorBackground, color, "ColorBackground")
    }

    pub fn set_default_cursor(&mut self, color: Option<GhosttyColorRgb>) -> Result<(), String> {
        self.set_color(GhosttyTerminalOption::ColorCursor, color, "ColorCursor")
    }

    #[allow(dead_code)]
    pub fn set_kitty_image_storage_limit(&mut self, limit: u64) -> Result<(), String> {
        let result =
            unsafe { ghostty_terminal_set(self.raw, GhosttyTerminalOption::KittyImageStorageLimit, &limit as *const u64 as *const c_void) };
        check_result(result, "ghostty_terminal_set(KittyImageStorageLimit)")
    }

    /// Enable reading Kitty images transmitted via file / temporary-file /
    /// shared-memory media. libghostty-vt performs the filesystem/shm reads
    /// itself (and deletes temp files), so this is only appropriate where the
    /// VT runs co-located with the program emitting the escape codes; i.e. the
    /// in-process backend. A future `load_media` embedder callback should
    /// replace this direct-read path so Cleat owns media access (vfs, remoting,
    /// testing).
    pub fn set_kitty_image_media(&mut self, file: bool, temp_file: bool, shared_memory: bool) -> Result<(), String> {
        for (option, value) in [
            (GhosttyTerminalOption::KittyImageMediumFile, file),
            (GhosttyTerminalOption::KittyImageMediumTempFile, temp_file),
            (GhosttyTerminalOption::KittyImageMediumSharedMem, shared_memory),
        ] {
            let result = unsafe { ghostty_terminal_set(self.raw, option, &value as *const bool as *const c_void) };
            check_result(result, "ghostty_terminal_set(KittyImageMedium)")?;
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub fn kitty_image_generation(&self, image_id: u32) -> Result<Option<u64>, String> {
        let mut graphics: GhosttyKittyGraphics = ptr::null_mut();
        let result = unsafe {
            ghostty_terminal_get(self.raw, GhosttyTerminalData::KittyGraphics, &mut graphics as *mut GhosttyKittyGraphics as *mut c_void)
        };
        check_result(result, "ghostty_terminal_get(KittyGraphics)")?;
        if graphics.is_null() {
            return Ok(None);
        }

        let image = unsafe { ghostty_kitty_graphics_image(graphics, image_id) };
        if image.is_null() {
            return Ok(None);
        }

        let mut generation = 0u64;
        let result = unsafe {
            ghostty_kitty_graphics_image_get(image, GhosttyKittyGraphicsImageData::Generation, &mut generation as *mut u64 as *mut c_void)
        };
        check_result(result, "ghostty_kitty_graphics_image_get(Generation)")?;
        Ok(Some(generation))
    }

    pub fn kitty_image_state(&self) -> Result<(Vec<KittyImageResourceInfo>, Vec<KittyImagePlacementInfo>), String> {
        let graphics = match self.kitty_graphics()? {
            Some(graphics) => graphics,
            None => return Ok((Vec::new(), Vec::new())),
        };

        let mut iterator = PlacementIteratorHandle::new()?;
        let result = unsafe {
            ghostty_kitty_graphics_get(
                graphics,
                GhosttyKittyGraphicsData::PlacementIterator,
                &mut iterator.raw as *mut GhosttyKittyGraphicsPlacementIterator as *mut c_void,
            )
        };
        check_result(result, "ghostty_kitty_graphics_get(PlacementIterator)")?;

        let mut resources = BTreeMap::new();
        let mut placements = Vec::new();
        while unsafe { ghostty_kitty_graphics_placement_next(iterator.raw) } {
            let mut image_id = 0u32;
            let result = unsafe {
                ghostty_kitty_graphics_placement_get(
                    iterator.raw,
                    GhosttyKittyGraphicsPlacementData::ImageId,
                    &mut image_id as *mut u32 as *mut c_void,
                )
            };
            check_result(result, "ghostty_kitty_graphics_placement_get(ImageId)")?;

            let image = unsafe { ghostty_kitty_graphics_image(graphics, image_id) };
            if image.is_null() {
                continue;
            }

            let resource = image_resource_info(image)?;
            let mut render_info = GhosttyKittyGraphicsPlacementRenderInfo::init();
            let result = unsafe { ghostty_kitty_graphics_placement_render_info(iterator.raw, image, self.raw, &mut render_info) };
            check_result(result, "ghostty_kitty_graphics_placement_render_info")?;
            if !render_info.viewport_visible {
                continue;
            }

            let mut is_virtual = false;
            let result = unsafe {
                ghostty_kitty_graphics_placement_get(
                    iterator.raw,
                    GhosttyKittyGraphicsPlacementData::IsVirtual,
                    &mut is_virtual as *mut bool as *mut c_void,
                )
            };
            check_result(result, "ghostty_kitty_graphics_placement_get(IsVirtual)")?;
            if is_virtual {
                continue;
            }

            let placement = KittyImagePlacementInfo {
                image_id,
                generation: resource.generation,
                placement_id: placement_u32(iterator.raw, GhosttyKittyGraphicsPlacementData::PlacementId, "PlacementId")?,
                is_virtual: false,
                z: placement_i32(iterator.raw, GhosttyKittyGraphicsPlacementData::Z, "Z")?,
                viewport_col: render_info.viewport_col,
                viewport_row: render_info.viewport_row,
                grid_cols: render_info.grid_cols,
                grid_rows: render_info.grid_rows,
                pixel_width: render_info.pixel_width,
                pixel_height: render_info.pixel_height,
                source_x: render_info.source_x,
                source_y: render_info.source_y,
                source_width: render_info.source_width,
                source_height: render_info.source_height,
                x_offset_px: placement_u32(iterator.raw, GhosttyKittyGraphicsPlacementData::XOffset, "XOffset")?,
                y_offset_px: placement_u32(iterator.raw, GhosttyKittyGraphicsPlacementData::YOffset, "YOffset")?,
            };
            resources.entry((resource.image_id, resource.generation)).or_insert(resource);
            placements.push(placement);
        }

        let virtual_iterator = VirtualPlacementIteratorHandle::new()?;
        let result = unsafe { ghostty_kitty_graphics_virtual_placement_iterator_reset(virtual_iterator.raw, self.raw) };
        match result {
            GhosttyResult::Success => loop {
                let mut info = GhosttyKittyGraphicsVirtualPlacementInfo::init();
                let result = unsafe { ghostty_kitty_graphics_virtual_placement_next(virtual_iterator.raw, &mut info) };
                match result {
                    GhosttyResult::Success => {}
                    GhosttyResult::NoValue => break,
                    other => {
                        check_result(other, "ghostty_kitty_graphics_virtual_placement_next")?;
                        unreachable!("check_result returned Ok for non-success virtual placement result");
                    }
                }

                let image = unsafe { ghostty_kitty_graphics_image(graphics, info.image_id) };
                if image.is_null() {
                    continue;
                }
                let resource = image_resource_info(image)?;
                let generation = resource.generation;
                resources.entry((resource.image_id, generation)).or_insert(resource);
                placements.push(KittyImagePlacementInfo {
                    image_id: info.image_id,
                    generation,
                    placement_id: info.placement_id,
                    is_virtual: true,
                    z: info.z,
                    viewport_col: info.viewport_col,
                    viewport_row: info.viewport_row,
                    grid_cols: info.grid_cols,
                    grid_rows: info.grid_rows,
                    pixel_width: info.pixel_width,
                    pixel_height: info.pixel_height,
                    source_x: info.source_x,
                    source_y: info.source_y,
                    source_width: info.source_width,
                    source_height: info.source_height,
                    x_offset_px: info.x_offset,
                    y_offset_px: info.y_offset,
                });
            },
            GhosttyResult::NoValue => {}
            other => {
                check_result(other, "ghostty_kitty_graphics_virtual_placement_iterator_reset")?;
            }
        }

        Ok((resources.into_values().collect(), placements))
    }

    pub fn with_kitty_image_data(&self, image_id: u32, generation: u64, callback: &mut dyn FnMut(&[u8]) -> bool) -> Result<bool, String> {
        let graphics = match self.kitty_graphics()? {
            Some(graphics) => graphics,
            None => return Ok(false),
        };
        let image = unsafe { ghostty_kitty_graphics_image(graphics, image_id) };
        if image.is_null() {
            return Ok(false);
        }
        let resource = image_resource_info(image)?;
        if resource.generation != generation {
            return Ok(false);
        }
        let mut data_ptr: *const u8 = ptr::null();
        let result = unsafe {
            ghostty_kitty_graphics_image_get(image, GhosttyKittyGraphicsImageData::DataPtr, &mut data_ptr as *mut *const u8 as *mut c_void)
        };
        check_result(result, "ghostty_kitty_graphics_image_get(DataPtr)")?;
        let mut data_len = 0usize;
        let result = unsafe {
            ghostty_kitty_graphics_image_get(image, GhosttyKittyGraphicsImageData::DataLen, &mut data_len as *mut usize as *mut c_void)
        };
        check_result(result, "ghostty_kitty_graphics_image_get(DataLen)")?;
        let bytes = if data_len == 0 {
            &[]
        } else {
            if data_ptr.is_null() {
                return Err("ghostty_kitty_graphics_image_get(DataPtr) returned null data".to_string());
            }
            unsafe { slice::from_raw_parts(data_ptr, data_len) }
        };
        Ok(callback(bytes))
    }

    fn kitty_graphics(&self) -> Result<Option<GhosttyKittyGraphics>, String> {
        let mut graphics: GhosttyKittyGraphics = ptr::null_mut();
        let result = unsafe {
            ghostty_terminal_get(self.raw, GhosttyTerminalData::KittyGraphics, &mut graphics as *mut GhosttyKittyGraphics as *mut c_void)
        };
        match result {
            GhosttyResult::Success => Ok((!graphics.is_null()).then_some(graphics)),
            GhosttyResult::NoValue => Ok(None),
            other => check_result(other, "ghostty_terminal_get(KittyGraphics)").map(|_| None),
        }
    }

    fn set_color(&mut self, option: GhosttyTerminalOption, color: Option<GhosttyColorRgb>, label: &str) -> Result<(), String> {
        let value = color.as_ref().map_or(ptr::null(), |color| color as *const GhosttyColorRgb as *const c_void);
        let result = unsafe { ghostty_terminal_set(self.raw, option, value) };
        check_result(result, &format!("ghostty_terminal_set({label})"))
    }

    pub fn active_screen(&self) -> Result<GhosttyTerminalScreen, String> {
        let mut screen = GhosttyTerminalScreen::Primary;
        let result = unsafe {
            ghostty_terminal_get(self.raw, GhosttyTerminalData::ActiveScreen, &mut screen as *mut GhosttyTerminalScreen as *mut c_void)
        };
        check_result(result, "ghostty_terminal_get(ActiveScreen)")?;
        Ok(screen)
    }

    pub fn scrollbar(&self) -> Result<GhosttyTerminalScrollbar, String> {
        let mut scrollbar = GhosttyTerminalScrollbar::default();
        let result = unsafe {
            ghostty_terminal_get(self.raw, GhosttyTerminalData::Scrollbar, &mut scrollbar as *mut GhosttyTerminalScrollbar as *mut c_void)
        };
        check_result(result, "ghostty_terminal_get(Scrollbar)")?;
        Ok(scrollbar)
    }

    pub fn scrollback_rows(&self) -> Result<usize, String> {
        let mut rows = 0usize;
        let result = unsafe { ghostty_terminal_get(self.raw, GhosttyTerminalData::ScrollbackRows, &mut rows as *mut usize as *mut c_void) };
        check_result(result, "ghostty_terminal_get(ScrollbackRows)")?;
        Ok(rows)
    }

    pub fn mode_enabled(&self, mode: GhosttyMode) -> Result<bool, String> {
        let mut enabled = false;
        let result = unsafe { ghostty_terminal_mode_get(self.raw, mode, &mut enabled) };
        check_result(result, "ghostty_terminal_mode_get")?;
        Ok(enabled)
    }

    pub fn scroll_viewport(&mut self, behavior: GhosttyTerminalScrollViewport) {
        unsafe { ghostty_terminal_scroll_viewport(self.raw, behavior) };
    }

    pub fn raw(&self) -> GhosttyTerminal {
        self.raw
    }

    /// Take all reply bytes libghostty has accumulated since the last drain.
    pub fn drain_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.effects.reply_buf)
    }
}

impl Drop for TerminalHandle {
    fn drop(&mut self) {
        // Free the terminal BEFORE the effects Box drops. libghostty will not
        // call our callback after this point, so the raw pointer stored in its
        // userdata becomes dead at the same instant the Box is released.
        unsafe { ghostty_terminal_free(self.raw) };
        // effects drops automatically afterwards.
    }
}

struct PlacementIteratorHandle {
    raw: GhosttyKittyGraphicsPlacementIterator,
}

impl PlacementIteratorHandle {
    fn new() -> Result<Self, String> {
        let mut raw = ptr::null_mut();
        let result = unsafe { ghostty_kitty_graphics_placement_iterator_new(ptr::null(), &mut raw) };
        check_result(result, "ghostty_kitty_graphics_placement_iterator_new")?;
        if raw.is_null() {
            return Err("ghostty_kitty_graphics_placement_iterator_new returned null iterator".to_string());
        }
        Ok(Self { raw })
    }
}

impl Drop for PlacementIteratorHandle {
    fn drop(&mut self) {
        unsafe { ghostty_kitty_graphics_placement_iterator_free(self.raw) };
    }
}

struct VirtualPlacementIteratorHandle {
    raw: GhosttyKittyGraphicsVirtualPlacementIterator,
}

impl VirtualPlacementIteratorHandle {
    fn new() -> Result<Self, String> {
        let mut raw = ptr::null_mut();
        let result = unsafe { ghostty_kitty_graphics_virtual_placement_iterator_new(ptr::null(), &mut raw) };
        check_result(result, "ghostty_kitty_graphics_virtual_placement_iterator_new")?;
        if raw.is_null() {
            return Err("ghostty_kitty_graphics_virtual_placement_iterator_new returned null iterator".to_string());
        }
        Ok(Self { raw })
    }
}

impl Drop for VirtualPlacementIteratorHandle {
    fn drop(&mut self) {
        unsafe { ghostty_kitty_graphics_virtual_placement_iterator_free(self.raw) };
    }
}

fn image_resource_info(image: GhosttyKittyGraphicsImage) -> Result<KittyImageResourceInfo, String> {
    Ok(KittyImageResourceInfo {
        image_id: image_u32(image, GhosttyKittyGraphicsImageData::Id, "Id")?,
        generation: image_u64(image, GhosttyKittyGraphicsImageData::Generation, "Generation")?,
        width_px: image_u32(image, GhosttyKittyGraphicsImageData::Width, "Width")?,
        height_px: image_u32(image, GhosttyKittyGraphicsImageData::Height, "Height")?,
        format: image_i32(image, GhosttyKittyGraphicsImageData::Format, "Format")? as u32,
        compression: image_i32(image, GhosttyKittyGraphicsImageData::Compression, "Compression")? as u32,
        data_len: image_usize(image, GhosttyKittyGraphicsImageData::DataLen, "DataLen")?,
    })
}

fn image_u32(image: GhosttyKittyGraphicsImage, data: GhosttyKittyGraphicsImageData, label: &str) -> Result<u32, String> {
    let mut value = 0u32;
    let result = unsafe { ghostty_kitty_graphics_image_get(image, data, &mut value as *mut u32 as *mut c_void) };
    check_result(result, &format!("ghostty_kitty_graphics_image_get({label})"))?;
    Ok(value)
}

fn image_u64(image: GhosttyKittyGraphicsImage, data: GhosttyKittyGraphicsImageData, label: &str) -> Result<u64, String> {
    let mut value = 0u64;
    let result = unsafe { ghostty_kitty_graphics_image_get(image, data, &mut value as *mut u64 as *mut c_void) };
    check_result(result, &format!("ghostty_kitty_graphics_image_get({label})"))?;
    Ok(value)
}

fn image_i32(image: GhosttyKittyGraphicsImage, data: GhosttyKittyGraphicsImageData, label: &str) -> Result<i32, String> {
    let mut value = 0i32;
    let result = unsafe { ghostty_kitty_graphics_image_get(image, data, &mut value as *mut i32 as *mut c_void) };
    check_result(result, &format!("ghostty_kitty_graphics_image_get({label})"))?;
    Ok(value)
}

fn image_usize(image: GhosttyKittyGraphicsImage, data: GhosttyKittyGraphicsImageData, label: &str) -> Result<usize, String> {
    let mut value = 0usize;
    let result = unsafe { ghostty_kitty_graphics_image_get(image, data, &mut value as *mut usize as *mut c_void) };
    check_result(result, &format!("ghostty_kitty_graphics_image_get({label})"))?;
    Ok(value)
}

fn placement_u32(
    iterator: GhosttyKittyGraphicsPlacementIterator,
    data: GhosttyKittyGraphicsPlacementData,
    label: &str,
) -> Result<u32, String> {
    let mut value = 0u32;
    let result = unsafe { ghostty_kitty_graphics_placement_get(iterator, data, &mut value as *mut u32 as *mut c_void) };
    check_result(result, &format!("ghostty_kitty_graphics_placement_get({label})"))?;
    Ok(value)
}

fn placement_i32(
    iterator: GhosttyKittyGraphicsPlacementIterator,
    data: GhosttyKittyGraphicsPlacementData,
    label: &str,
) -> Result<i32, String> {
    let mut value = 0i32;
    let result = unsafe { ghostty_kitty_graphics_placement_get(iterator, data, &mut value as *mut i32 as *mut c_void) };
    check_result(result, &format!("ghostty_kitty_graphics_placement_get({label})"))?;
    Ok(value)
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

    pub fn get_cursor_blinking(&self) -> Result<bool, String> {
        let mut blinking = false;
        let result = unsafe {
            ghostty_render_state_get(self.raw, GhosttyRenderStateData::CursorBlinking, &mut blinking as *mut bool as *mut c_void)
        };
        check_result(result, "ghostty_render_state_get(CursorBlinking)")?;
        Ok(blinking)
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

    pub fn get_raw_row(&self) -> Result<GhosttyRow, String> {
        let mut row: GhosttyRow = 0;
        let result =
            unsafe { ghostty_render_state_row_get(self.raw, GhosttyRenderStateRowData::Raw, &mut row as *mut GhosttyRow as *mut c_void) };
        check_result(result, "ghostty_render_state_row_get(Raw)")?;
        Ok(row)
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

const INLINE_GRAPHEME_UTF8_CAPACITY: usize = 64;

pub struct GhosttyCellSnapshot {
    pub style: GhosttyStyle,
    pub content_tag: GhosttyCellContentTag,
    pub wide: GhosttyCellWide,
    pub has_text: bool,
    pub has_styling: bool,
    pub style_id: u16,
    pub has_hyperlink: bool,
    pub protected: bool,
    pub semantic_content: GhosttyCellSemanticContent,
    pub color_palette: u8,
    pub color_rgb: GhosttyColorRgb,
}

pub struct RowCellsHandle {
    raw: GhosttyRenderStateRowCells,
    inline_grapheme_utf8: [u8; INLINE_GRAPHEME_UTF8_CAPACITY],
    overflow_grapheme_utf8: Vec<u8>,
}

impl RowCellsHandle {
    pub fn new() -> Result<Self, String> {
        let mut raw = ptr::null_mut();
        let result = unsafe { ghostty_render_state_row_cells_new(ptr::null(), &mut raw) };
        check_result(result, "ghostty_render_state_row_cells_new")?;
        Ok(Self { raw, inline_grapheme_utf8: [0; INLINE_GRAPHEME_UTF8_CAPACITY], overflow_grapheme_utf8: Vec::new() })
    }

    pub fn next(&mut self) -> bool {
        unsafe { ghostty_render_state_row_cells_next(self.raw) }
    }

    pub fn read_cell_into(&mut self, graphemes: &mut Vec<u32>) -> Result<GhosttyCellSnapshot, String> {
        let mut cell: GhosttyCell = 0;
        let mut style = GhosttyStyle::init();
        let raw = self.raw;
        let grapheme_len = match fetch_row_cell(raw, &mut self.inline_grapheme_utf8, &mut cell, &mut style) {
            Ok(len) => len,
            Err((GhosttyResult::OutOfSpace, 2, required)) => {
                self.overflow_grapheme_utf8.resize(required, 0);
                fetch_row_cell(raw, &mut self.overflow_grapheme_utf8, &mut cell, &mut style).map_err(|(result, written, _)| {
                    format!("ghostty_render_state_row_cells_get_multi failed after {written} values: {result:?}")
                })?
            }
            Err((result, written, _)) => {
                return Err(format!("ghostty_render_state_row_cells_get_multi failed after {written} values: {result:?}"));
            }
        };

        let grapheme_bytes = if grapheme_len <= self.inline_grapheme_utf8.len() {
            &self.inline_grapheme_utf8[..grapheme_len]
        } else {
            &self.overflow_grapheme_utf8[..grapheme_len]
        };
        let grapheme_text =
            std::str::from_utf8(grapheme_bytes).map_err(|err| format!("ghostty render-state grapheme was not valid utf-8: {err}"))?;
        graphemes.clear();
        graphemes.extend(grapheme_text.chars().map(u32::from));

        let mut content_tag = GhosttyCellContentTag::Codepoint;
        let mut wide = GhosttyCellWide::Narrow;
        let mut has_text = false;
        let mut has_styling = false;
        let mut style_id: u16 = 0;
        let mut has_hyperlink = false;
        let mut protected = false;
        let mut semantic_content = GhosttyCellSemanticContent::Output;
        let mut color_palette: u8 = 0;
        let mut color_rgb = GhosttyColorRgb::default();
        let keys = [
            GhosttyCellData::ContentTag,
            GhosttyCellData::Wide,
            GhosttyCellData::HasText,
            GhosttyCellData::HasStyling,
            GhosttyCellData::StyleId,
            GhosttyCellData::HasHyperlink,
            GhosttyCellData::Protected,
            GhosttyCellData::SemanticContent,
            GhosttyCellData::ColorPalette,
            GhosttyCellData::ColorRgb,
        ];
        let mut values = [
            (&mut content_tag as *mut GhosttyCellContentTag).cast::<c_void>(),
            (&mut wide as *mut GhosttyCellWide).cast::<c_void>(),
            (&mut has_text as *mut bool).cast::<c_void>(),
            (&mut has_styling as *mut bool).cast::<c_void>(),
            (&mut style_id as *mut u16).cast::<c_void>(),
            (&mut has_hyperlink as *mut bool).cast::<c_void>(),
            (&mut protected as *mut bool).cast::<c_void>(),
            (&mut semantic_content as *mut GhosttyCellSemanticContent).cast::<c_void>(),
            (&mut color_palette as *mut u8).cast::<c_void>(),
            (&mut color_rgb as *mut GhosttyColorRgb).cast::<c_void>(),
        ];
        let mut written = 0;
        let result = unsafe { ghostty_cell_get_multi(cell, keys.len(), keys.as_ptr(), values.as_mut_ptr(), &mut written) };
        check_multi_result(result, written, keys.len(), "ghostty_cell_get_multi")?;

        Ok(GhosttyCellSnapshot {
            style,
            content_tag,
            wide,
            has_text,
            has_styling,
            style_id,
            has_hyperlink,
            protected,
            semantic_content,
            color_palette,
            color_rgb,
        })
    }
}

fn fetch_row_cell(
    cells: GhosttyRenderStateRowCells,
    storage: &mut [u8],
    cell: &mut GhosttyCell,
    style: &mut GhosttyStyle,
) -> Result<usize, (GhosttyResult, usize, usize)> {
    let mut graphemes = GhosttyBuffer { ptr: storage.as_mut_ptr(), cap: storage.len(), len: 0 };
    let keys = [GhosttyRenderStateRowCellsData::Raw, GhosttyRenderStateRowCellsData::Style, GhosttyRenderStateRowCellsData::GraphemesUtf8];
    let mut values = [
        (cell as *mut GhosttyCell).cast::<c_void>(),
        (style as *mut GhosttyStyle).cast::<c_void>(),
        (&mut graphemes as *mut GhosttyBuffer).cast::<c_void>(),
    ];
    let mut written = 0;
    let result = unsafe { ghostty_render_state_row_cells_get_multi(cells, keys.len(), keys.as_ptr(), values.as_mut_ptr(), &mut written) };
    if result == GhosttyResult::Success && written == keys.len() {
        Ok(graphemes.len)
    } else {
        Err((result, written, graphemes.len))
    }
}

fn check_multi_result(result: GhosttyResult, written: usize, expected: usize, label: &'static str) -> Result<(), String> {
    check_result(result, label)?;
    if written == expected {
        Ok(())
    } else {
        Err(format!("{label} wrote {written} of {expected} values"))
    }
}

pub fn row_get_bool(row: GhosttyRow, data: GhosttyRowData, label: &'static str) -> Result<bool, String> {
    let mut value = false;
    let result = unsafe { ghostty_row_get(row, data, &mut value as *mut bool as *mut c_void) };
    check_result(result, label)?;
    Ok(value)
}

pub fn row_get_semantic_prompt(row: GhosttyRow) -> Result<GhosttyRowSemanticPrompt, String> {
    let mut value = GhosttyRowSemanticPrompt::None;
    let result =
        unsafe { ghostty_row_get(row, GhosttyRowData::SemanticPrompt, &mut value as *mut GhosttyRowSemanticPrompt as *mut c_void) };
    check_result(result, "ghostty_row_get(SemanticPrompt)")?;
    Ok(value)
}

impl Drop for RowCellsHandle {
    fn drop(&mut self) {
        unsafe { ghostty_render_state_row_cells_free(self.raw) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_captures_dsr_reply_into_drain_buffer() {
        let mut term = TerminalHandle::new(80, 24, 1024).expect("new terminal");
        // CSI 6 n = DSR Cursor Position Report — should produce CSI <row> ; <col> R
        term.feed(b"\x1b[6n");
        let reply = term.drain_replies();
        assert!(reply.starts_with(b"\x1b[") && reply.ends_with(b"R"), "expected CPR reply, got {reply:?}",);
    }

    #[test]
    fn terminal_answers_da1_with_vt220_and_ansi_color() {
        let mut term = TerminalHandle::new(80, 24, 1024).expect("new terminal");
        term.feed(b"\x1b[c");
        let reply = term.drain_replies();
        assert_eq!(reply, b"\x1b[?62;22c".to_vec());
    }

    #[test]
    fn terminal_answers_da2_with_vt220_firmware_10() {
        let mut term = TerminalHandle::new(80, 24, 1024).expect("new terminal");
        term.feed(b"\x1b[>c");
        let reply = term.drain_replies();
        assert_eq!(reply, b"\x1b[>1;10;0c".to_vec());
    }

    #[test]
    fn terminal_answers_xtwinops_size_reports() {
        let mut term = TerminalHandle::new(80, 24, 1024).expect("new terminal");
        term.resize(80, 24, 9, 18).expect("resize with cell geometry");

        term.feed(b"\x1b[16t");
        assert_eq!(term.drain_replies(), b"\x1b[6;18;9t".to_vec());

        term.feed(b"\x1b[14t");
        assert_eq!(term.drain_replies(), b"\x1b[4;432;720t".to_vec());

        term.feed(b"\x1b[18t");
        assert_eq!(term.drain_replies(), b"\x1b[8;24;80t".to_vec());
    }

    #[test]
    fn png_decoder_returns_rgba_pixels() {
        let decoded = decode_png_rgba(&[
            137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 31, 21, 196, 137, 0, 0, 0,
            13, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 240, 31, 0, 5, 0, 1, 255, 137, 153, 61, 29, 0, 0, 0, 0, 73, 69, 78, 68, 174,
            66, 96, 130,
        ])
        .expect("decode png");

        assert_eq!(decoded.width, 1);
        assert_eq!(decoded.height, 1);
        assert_eq!(decoded.rgba.len(), 4);
    }

    #[test]
    fn kitty_image_generation_changes_when_same_id_is_retransmitted() {
        let mut term = TerminalHandle::new(80, 24, 1024).expect("new terminal");
        term.set_kitty_image_storage_limit(64 * 1024 * 1024).expect("enable kitty image storage");

        term.feed(b"\x1b_Ga=t,t=d,f=24,i=1,s=1,v=1;AAAA\x1b\\");
        let first = term.kitty_image_generation(1).expect("first image generation").expect("first image exists");
        assert!(first > 0);

        term.feed(b"\x1b_Ga=t,t=d,f=24,i=1,s=1,v=1;////\x1b\\");
        let second = term.kitty_image_generation(1).expect("second image generation").expect("second image exists");

        assert_ne!(first, second);
    }
}
