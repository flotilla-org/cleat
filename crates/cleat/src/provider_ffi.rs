use std::{
    ffi::c_void,
    path::PathBuf,
    ptr, slice,
    str::Utf8Error,
    sync::{Arc, Mutex},
};

use http::{Method, StatusCode};

use crate::{
    host::actor::{ObservationState, SessionActor, SessionCommand, SessionMouseEvent, SessionWheelEvent},
    http_uds, keys,
    platform::ipc::connect_session_stream,
    provider::{
        DirtyState, ProviderFeatures, TerminalCell, TerminalCellFlags, TerminalCellWidth, TerminalCursor, TerminalCursorStyle,
        TerminalGeometry, TerminalImagePlacement, TerminalImageResource, TerminalRenderUpdate, TerminalRenderUpdateOpKind, TerminalRgb,
        TerminalScrollbackExtent, TerminalScrollbarState, TerminalSnapshot, TerminalStyleColor, TerminalStyleColorTag,
        TerminalViewportKind, ViewportCommand, ViewportCommandOutcome, TERMINAL_IMAGE_PLACEMENT_VIRTUAL,
    },
    runtime::{RuntimeLayout, TerminalSize},
    session::{ensure_session_started, SessionStartOptions},
    session_runtime::SessionRuntime,
    vt::{self, Rgb, TerminalColors, VtEngineKind},
};

pub const CLEAT_PROVIDER_ABI_VERSION: u32 = 6;
pub const CLEAT_PROVIDER_BACKEND_MOCK: u32 = 0;
pub const CLEAT_PROVIDER_BACKEND_IN_PROCESS: u32 = 1;
pub const CLEAT_PROVIDER_BACKEND_DAEMON: u32 = 2;
pub const CLEAT_PROVIDER_VT_DEFAULT: u32 = 0;
pub const CLEAT_PROVIDER_VT_PASSTHROUGH: u32 = 1;
pub const CLEAT_PROVIDER_VT_GHOSTTY: u32 = 2;
pub const CLEAT_INPUT_KEY: u32 = 1;
pub const CLEAT_INPUT_TEXT: u32 = 2;
pub const CLEAT_INPUT_MOUSE: u32 = 3;
pub const CLEAT_INPUT_FOCUS: u32 = 4;
pub const CLEAT_INPUT_PASTE: u32 = 5;
pub const CLEAT_INPUT_RESIZE: u32 = 6;
pub const CLEAT_KEY_UNICODE_SCALAR: u32 = 1;
pub const CLEAT_KEY_NAMED: u32 = 2;
pub const CLEAT_KEY_ENTER: u32 = 1;
pub const CLEAT_KEY_ESCAPE: u32 = 2;
pub const CLEAT_KEY_BACKSPACE: u32 = 3;
pub const CLEAT_KEY_TAB: u32 = 4;
pub const CLEAT_KEY_DELETE: u32 = 5;
pub const CLEAT_KEY_INSERT: u32 = 6;
pub const CLEAT_KEY_HOME: u32 = 7;
pub const CLEAT_KEY_END: u32 = 8;
pub const CLEAT_KEY_PAGE_UP: u32 = 9;
pub const CLEAT_KEY_PAGE_DOWN: u32 = 10;
pub const CLEAT_KEY_ARROW_UP: u32 = 12;
pub const CLEAT_KEY_ARROW_DOWN: u32 = 13;
pub const CLEAT_KEY_ARROW_LEFT: u32 = 14;
pub const CLEAT_KEY_ARROW_RIGHT: u32 = 15;
pub const CLEAT_KEY_FUNCTION_BASE: u32 = 100;
pub const CLEAT_KEY_F1: u32 = 101;
pub const CLEAT_KEY_F2: u32 = 102;
pub const CLEAT_KEY_F3: u32 = 103;
pub const CLEAT_KEY_F4: u32 = 104;
pub const CLEAT_KEY_F5: u32 = 105;
pub const CLEAT_KEY_F6: u32 = 106;
pub const CLEAT_KEY_F7: u32 = 107;
pub const CLEAT_KEY_F8: u32 = 108;
pub const CLEAT_KEY_F9: u32 = 109;
pub const CLEAT_KEY_F10: u32 = 110;
pub const CLEAT_KEY_F11: u32 = 111;
pub const CLEAT_KEY_F12: u32 = 112;
pub const CLEAT_KEY_ACTION_PRESS: u32 = 1;
pub const CLEAT_KEY_ACTION_REPEAT: u32 = 2;
pub const CLEAT_KEY_ACTION_RELEASE: u32 = 3;
pub const CLEAT_MOD_SHIFT: u16 = 1;
pub const CLEAT_MOD_CTRL: u16 = 2;
pub const CLEAT_MOD_ALT: u16 = 4;
pub const CLEAT_MOD_SUPER: u16 = 8;
pub const CLEAT_MOUSE_PRESS: u32 = 1;
pub const CLEAT_MOUSE_RELEASE: u32 = 2;
pub const CLEAT_MOUSE_MOVE: u32 = 3;
pub const CLEAT_MOUSE_WHEEL: u32 = 4;
pub const CLEAT_MOUSE_BUTTON_NONE: u32 = 0;
pub const CLEAT_MOUSE_BUTTON_LEFT: u32 = 1;
pub const CLEAT_MOUSE_BUTTON_MIDDLE: u32 = 2;
pub const CLEAT_MOUSE_BUTTON_RIGHT: u32 = 3;
pub const CLEAT_MOUSE_BUTTON_BACK: u32 = 4;
pub const CLEAT_MOUSE_BUTTON_FORWARD: u32 = 5;
pub const CLEAT_MOUSE_BUTTON_FLAG_LEFT: u16 = 1;
pub const CLEAT_MOUSE_BUTTON_FLAG_MIDDLE: u16 = 2;
pub const CLEAT_MOUSE_BUTTON_FLAG_RIGHT: u16 = 4;
pub const CLEAT_MOUSE_BUTTON_FLAG_BACK: u16 = 8;
pub const CLEAT_MOUSE_BUTTON_FLAG_FORWARD: u16 = 16;
pub const CLEAT_MOUSE_TRACKING_NONE: u32 = 0;
pub const CLEAT_MOUSE_TRACKING_X10: u32 = 1;
pub const CLEAT_MOUSE_TRACKING_NORMAL: u32 = 2;
pub const CLEAT_MOUSE_TRACKING_BUTTON: u32 = 3;
pub const CLEAT_MOUSE_TRACKING_ANY: u32 = 4;
pub const CLEAT_MOUSE_FORMAT_LEGACY: u32 = 0;
pub const CLEAT_MOUSE_FORMAT_SGR: u32 = 1;
pub const CLEAT_MOUSE_FORMAT_SGR_PIXELS: u32 = 2;
pub const CLEAT_CELL_WIDTH_NARROW: u32 = 0;
pub const CLEAT_CELL_WIDTH_WIDE: u32 = 1;
pub const CLEAT_CELL_WIDTH_SPACER_TAIL: u32 = 2;
pub const CLEAT_CELL_WIDTH_SPACER_HEAD: u32 = 3;
pub const CLEAT_CURSOR_STYLE_BAR: u32 = 0;
pub const CLEAT_CURSOR_STYLE_BLOCK: u32 = 1;
pub const CLEAT_CURSOR_STYLE_UNDERLINE: u32 = 2;
pub const CLEAT_CURSOR_STYLE_BLOCK_HOLLOW: u32 = 3;
pub const CLEAT_VIEWPORT_LIVE_NORMAL: u32 = 1;
pub const CLEAT_VIEWPORT_LIVE_ALTERNATE: u32 = 2;
pub const CLEAT_VIEWPORT_NORMAL_SCROLLBACK: u32 = 3;
pub const CLEAT_VIEWPORT_COMMAND_TOP: u32 = 1;
pub const CLEAT_VIEWPORT_COMMAND_BOTTOM: u32 = 2;
pub const CLEAT_VIEWPORT_COMMAND_DELTA_ROWS: u32 = 3;
pub const CLEAT_VIEWPORT_OUTCOME_MOVED: u32 = 1;
pub const CLEAT_VIEWPORT_OUTCOME_NO_OP: u32 = 2;
pub const CLEAT_VIEWPORT_OUTCOME_UNSUPPORTED: u32 = 3;
pub const CLEAT_RENDER_UPDATE_VERSION: u32 = 1;
pub const CLEAT_RENDER_OP_FULL_VISIBLE_REPLACE: u32 = 1;
pub const CLEAT_RENDER_OP_ROW_REPLACE: u32 = 2;
pub const CLEAT_RENDER_OP_SCROLL_COPY: u32 = 3;
pub const CLEAT_STYLE_COLOR_NONE: u32 = 0;
pub const CLEAT_STYLE_COLOR_PALETTE: u32 = 1;
pub const CLEAT_STYLE_COLOR_RGB: u32 = 2;
pub const CLEAT_IMAGE_FORMAT_RGB: u32 = 0;
pub const CLEAT_IMAGE_FORMAT_RGBA: u32 = 1;
pub const CLEAT_IMAGE_FORMAT_PNG: u32 = 2;
pub const CLEAT_IMAGE_FORMAT_GRAY_ALPHA: u32 = 3;
pub const CLEAT_IMAGE_FORMAT_GRAY: u32 = 4;
pub const CLEAT_IMAGE_COMPRESSION_NONE: u32 = 0;
pub const CLEAT_IMAGE_COMPRESSION_ZLIB_DEFLATE: u32 = 1;
pub const CLEAT_IMAGE_PLACEMENT_VIRTUAL: u32 = TERMINAL_IMAGE_PLACEMENT_VIRTUAL;

pub type CleatImageResourceDataFn = Option<unsafe extern "C" fn(user_data: *mut c_void, data: *const u8, data_len: usize) -> bool>;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CleatDirtyState {
    #[default]
    Clean = 0,
    Partial = 1,
    Full = 2,
}

impl From<DirtyState> for CleatDirtyState {
    fn from(value: DirtyState) -> Self {
        match value {
            DirtyState::Clean => Self::Clean,
            DirtyState::Partial => Self::Partial,
            DirtyState::Full => Self::Full,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CleatProviderDesc {
    pub abi_version: u32,
    pub requested_features: u32,
    pub backend: u32,
    pub runtime_root: *const u8,
    pub runtime_root_len: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CleatSessionDesc {
    pub cols: u16,
    pub rows: u16,
    pub cell_width_px: f32,
    pub cell_height_px: f32,
    pub vt_engine: u32,
    pub command: *const u8,
    pub command_len: usize,
    pub cwd: *const u8,
    pub cwd_len: usize,
    pub id: *const u8,
    pub id_len: usize,
    pub record: bool,
    pub colors: *const CleatSessionColors,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CleatRgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CleatSessionColors {
    pub size: usize,
    pub has_foreground: bool,
    pub foreground: CleatRgb,
    pub has_background: bool,
    pub background: CleatRgb,
    pub has_cursor: bool,
    pub cursor: CleatRgb,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CleatCell {
    pub graphemes: *const u32,
    pub grapheme_count: usize,
    pub fg: CleatRgb,
    pub bg: CleatRgb,
    pub flags: u32,
    pub width: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CleatCursor {
    pub col: u16,
    pub row: u16,
    pub visible: bool,
    pub style: u32,
    pub blink: bool,
    pub wide_tail: bool,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CleatTerminalGeometry {
    pub cell_width_px: f32,
    pub cell_height_px: f32,
    pub content_x_px: f32,
    pub content_y_px: f32,
    pub content_width_px: f32,
    pub content_height_px: f32,
}

impl From<CleatTerminalGeometry> for TerminalGeometry {
    fn from(value: CleatTerminalGeometry) -> Self {
        TerminalGeometry {
            cell_width_px: value.cell_width_px,
            cell_height_px: value.cell_height_px,
            content_x_px: value.content_x_px,
            content_y_px: value.content_y_px,
            content_width_px: value.content_width_px,
            content_height_px: value.content_height_px,
        }
        .sanitized()
    }
}

impl From<TerminalGeometry> for CleatTerminalGeometry {
    fn from(value: TerminalGeometry) -> Self {
        Self {
            cell_width_px: value.cell_width_px,
            cell_height_px: value.cell_height_px,
            content_x_px: value.content_x_px,
            content_y_px: value.content_y_px,
            content_width_px: value.content_width_px,
            content_height_px: value.content_height_px,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CleatTerminalModeState {
    pub mouse_tracking: bool,
    pub mouse_tracking_mode: u32,
    pub mouse_report_format: u32,
    pub mouse_sgr: bool,
    pub mouse_sgr_pixels: bool,
    pub active_alternate_screen: bool,
    pub application_cursor_keys: bool,
    pub alternate_scroll: bool,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CleatSnapshot {
    pub cols: u16,
    pub rows: u16,
    pub geometry: CleatTerminalGeometry,
    pub viewport_kind: u32,
    pub scrollback_offset_rows: u64,
    pub scrollbar: CleatTerminalScrollbarState,
    pub terminal_modes: CleatTerminalModeState,
    pub render_generation: u64,
    pub cells: *const CleatCell,
    pub cell_count: usize,
    pub dirty_rows: *const u16,
    pub dirty_row_count: usize,
    pub cursor: CleatCursor,
    pub dirty: CleatDirtyState,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CleatStyleColor {
    pub size: usize,
    pub tag: u32,
    pub palette_index: u8,
    pub rgb: CleatRgb,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CleatRenderStyle {
    pub size: usize,
    pub flags: u32,
    pub width: u32,
    pub fg: CleatRgb,
    pub bg: CleatRgb,
    pub fg_color: CleatStyleColor,
    pub bg_color: CleatStyleColor,
    pub underline_style: u32,
    pub underline_color: CleatStyleColor,
    pub protected_cell: bool,
    pub has_hyperlink: bool,
    pub semantic: u32,
    pub hyperlink_id: u64,
    pub content_tag: u32,
    pub has_text: bool,
    pub has_styling: bool,
    pub style_id: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CleatRenderCell {
    pub size: usize,
    pub graphemes: *const u32,
    pub grapheme_count: usize,
    pub style: CleatRenderStyle,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CleatRenderRow {
    pub size: usize,
    pub row: u16,
    pub col_count: u16,
    pub cells: *const CleatRenderCell,
    pub cell_count: usize,
    pub wrap: bool,
    pub wrap_continuation: bool,
    pub has_graphemes: bool,
    pub has_styling: bool,
    pub has_hyperlink: bool,
    pub semantic_prompt: u32,
    pub has_kitty_virtual_placeholder: bool,
    pub dirty: bool,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CleatRenderUpdateOp {
    pub size: usize,
    pub kind: u32,
    pub first_row: u16,
    pub row_count: u16,
    pub col_count: u16,
    pub rows: *const CleatRenderRow,
    pub row_desc_count: usize,
    pub cells: *const CleatRenderCell,
    pub cell_count: usize,
    pub src_row: u16,
    pub dst_row: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CleatImageResource {
    pub size: usize,
    pub image_id: u32,
    pub generation: u64,
    pub width_px: u32,
    pub height_px: u32,
    pub format: u32,
    pub compression: u32,
    pub data_len: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CleatImagePlacement {
    pub size: usize,
    pub image_id: u32,
    pub generation: u64,
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
    pub x_offset_px: u32,
    pub y_offset_px: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CleatRenderUpdate {
    pub size: usize,
    pub version: u32,
    pub cols: u16,
    pub rows: u16,
    pub geometry: CleatTerminalGeometry,
    pub viewport_kind: u32,
    pub scrollback_offset_rows: u64,
    pub scrollbar: CleatTerminalScrollbarState,
    pub terminal_modes: CleatTerminalModeState,
    pub render_generation: u64,
    pub cursor: CleatCursor,
    pub dirty: CleatDirtyState,
    pub ops: *const CleatRenderUpdateOp,
    pub op_count: usize,
    pub image_resources: *const CleatImageResource,
    pub image_resource_count: usize,
    pub image_placements: *const CleatImagePlacement,
    pub image_placement_count: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CleatInputEvent {
    pub kind: u32,
    pub modifiers: u16,
    pub consumed_modifiers: u16,
    pub focused: bool,
    pub key_action: u32,
    pub key_kind: u32,
    pub key_code: u32,
    pub text: *const u8,
    pub text_len: usize,
    pub generated_text: *const u8,
    pub generated_text_len: usize,
    pub platform_keycode: u32,
    pub mouse_kind: u32,
    pub mouse_button: u32,
    pub mouse_buttons: u16,
    pub cell_col: u16,
    pub cell_row: u16,
    pub x_px: f32,
    pub y_px: f32,
    pub wheel_delta_x: f32,
    pub wheel_delta_y: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CleatInputResult {
    pub first_sequence: u64,
    pub count: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CleatScrollbackExtent {
    pub normal_scrollback_rows: u64,
    pub live_rows: u16,
    pub alternate_screen: bool,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CleatTerminalScrollbarState {
    pub viewport_kind: u32,
    pub total_rows: u64,
    pub viewport_rows: u16,
    pub viewport_top_row: u64,
    pub at_bottom: bool,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CleatViewportRequest {
    pub kind: u32,
    pub scrollback_offset_rows: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CleatViewportCommand {
    pub kind: u32,
    pub delta_rows: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CleatViewportCommandResult {
    pub outcome: u32,
}

pub struct CleatProvider {
    features: ProviderFeatures,
    backend: ProviderBackend,
    runtime_root: PathBuf,
    wake: Arc<Mutex<WakeCallback>>,
}

pub struct CleatSession {
    backend: SessionBackend,
    geometry: TerminalGeometry,
    next_input_sequence: u64,
    wake: Arc<Mutex<WakeCallback>>,
    last_snapshot: Option<Box<OwnedSnapshot>>,
    last_render_update: Option<Box<OwnedRenderUpdate>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProviderBackend {
    Mock,
    InProcess,
    Daemon,
}

enum SessionBackend {
    Mock(MockSession),
    InProcess(Box<InProcessSession>),
    Daemon(DaemonSession),
}

struct MockSession {
    cols: u16,
    rows: u16,
    observation: ObservationState,
    input_count: u64,
}

struct InProcessSession {
    actor: SessionActor,
}

struct DaemonSession {
    id: String,
    runtime_root: PathBuf,
    rows: u16,
    observation: ObservationState,
}

pub type CleatWakeFn = Option<unsafe extern "C" fn(*mut c_void)>;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct WakeCallback {
    wake: CleatWakeFn,
    user_data: usize,
}

struct OwnedSnapshot {
    snapshot: CleatSnapshot,
    cells: Vec<CleatCell>,
    dirty_rows: Vec<u16>,
    _graphemes: Vec<Vec<u32>>,
}

struct OwnedRenderUpdate {
    update: CleatRenderUpdate,
    ops: Vec<CleatRenderUpdateOp>,
    rows: Vec<CleatRenderRow>,
    cells: Vec<CleatRenderCell>,
    image_resources: Vec<CleatImageResource>,
    image_placements: Vec<CleatImagePlacement>,
    _graphemes: Vec<Vec<u32>>,
}

impl OwnedSnapshot {
    fn from_snapshot(snapshot: TerminalSnapshot) -> Box<Self> {
        let graphemes: Vec<Vec<u32>> = snapshot.cells.iter().map(|cell| cell.graphemes.clone()).collect();
        let cells: Vec<CleatCell> = snapshot
            .cells
            .iter()
            .zip(graphemes.iter())
            .map(|(cell, graphemes)| CleatCell {
                graphemes: graphemes.as_ptr(),
                grapheme_count: graphemes.len(),
                fg: CleatRgb { r: cell.fg.r, g: cell.fg.g, b: cell.fg.b },
                bg: CleatRgb { r: cell.bg.r, g: cell.bg.g, b: cell.bg.b },
                flags: cell.flags.bits(),
                width: cell_width_tag(cell.width),
            })
            .collect();
        let mut owned = Box::new(Self {
            snapshot: CleatSnapshot {
                cols: snapshot.cols,
                rows: snapshot.rows,
                geometry: snapshot.geometry.into(),
                viewport_kind: viewport_kind_to_ffi(snapshot.viewport_kind),
                scrollback_offset_rows: snapshot.scrollback_offset_rows,
                scrollbar: scrollbar_to_ffi(snapshot.scrollbar),
                terminal_modes: terminal_modes_to_ffi(snapshot.terminal_modes),
                render_generation: snapshot.render_generation,
                cells: ptr::null(),
                cell_count: cells.len(),
                dirty_rows: ptr::null(),
                dirty_row_count: snapshot.dirty_rows.len(),
                cursor: cursor_to_ffi(snapshot.cursor),
                dirty: snapshot.dirty.into(),
            },
            cells,
            dirty_rows: snapshot.dirty_rows,
            _graphemes: graphemes,
        });
        owned.snapshot.cells = owned.cells.as_ptr();
        owned.snapshot.dirty_rows = if owned.dirty_rows.is_empty() { ptr::null() } else { owned.dirty_rows.as_ptr() };
        owned
    }
}

impl OwnedRenderUpdate {
    fn from_update(update: TerminalRenderUpdate) -> Box<Self> {
        let mut graphemes = Vec::new();
        let mut cells = Vec::new();
        let mut rows = Vec::new();
        let mut ops = Vec::new();
        for op in update.ops {
            let first_row = rows.len();
            let first_cell = cells.len();
            for row in op.rows {
                let row_first_cell = cells.len();
                for cell in row.cells {
                    graphemes.push(cell.graphemes);
                    cells.push(CleatRenderCell {
                        size: std::mem::size_of::<CleatRenderCell>(),
                        graphemes: ptr::null(),
                        grapheme_count: 0,
                        style: CleatRenderStyle {
                            size: std::mem::size_of::<CleatRenderStyle>(),
                            flags: cell.style.flags.bits(),
                            width: cell_width_tag(cell.style.width),
                            fg: rgb_to_ffi(cell.style.resolved_fg),
                            bg: rgb_to_ffi(cell.style.resolved_bg),
                            fg_color: style_color_to_ffi(&cell.style.fg_color),
                            bg_color: style_color_to_ffi(&cell.style.bg_color),
                            underline_style: cell.style.underline_style,
                            underline_color: style_color_to_ffi(&cell.style.underline_color),
                            protected_cell: cell.style.protected,
                            has_hyperlink: cell.style.has_hyperlink,
                            semantic: cell.style.semantic,
                            hyperlink_id: cell.style.hyperlink_id,
                            content_tag: cell.style.content_tag,
                            has_text: cell.style.has_text,
                            has_styling: cell.style.has_styling,
                            style_id: cell.style.style_id,
                        },
                    });
                }
                rows.push(CleatRenderRow {
                    size: std::mem::size_of::<CleatRenderRow>(),
                    row: row.row,
                    col_count: row.col_count,
                    cells: ptr::null(),
                    cell_count: cells.len() - row_first_cell,
                    wrap: row.wrap,
                    wrap_continuation: row.wrap_continuation,
                    has_graphemes: row.has_graphemes,
                    has_styling: row.has_styling,
                    has_hyperlink: row.has_hyperlink,
                    semantic_prompt: row.semantic_prompt,
                    has_kitty_virtual_placeholder: row.has_kitty_virtual_placeholder,
                    dirty: row.dirty,
                });
            }
            let cell_count = cells.len() - first_cell;
            ops.push(CleatRenderUpdateOp {
                size: std::mem::size_of::<CleatRenderUpdateOp>(),
                kind: render_update_op_kind_to_ffi(op.kind),
                first_row: op.first_row,
                row_count: op.row_count,
                col_count: op.col_count,
                rows: ptr::null(),
                row_desc_count: rows.len() - first_row,
                cells: ptr::null(),
                cell_count,
                src_row: op.src_row,
                dst_row: op.dst_row,
            });
        }
        let image_resources: Vec<CleatImageResource> = update.image_resources.iter().map(image_resource_to_ffi).collect();
        let image_placements: Vec<CleatImagePlacement> = update.image_placements.iter().map(image_placement_to_ffi).collect();
        let mut owned = Box::new(Self {
            update: CleatRenderUpdate {
                size: std::mem::size_of::<CleatRenderUpdate>(),
                version: CLEAT_RENDER_UPDATE_VERSION,
                cols: update.cols,
                rows: update.rows,
                geometry: update.geometry.into(),
                viewport_kind: viewport_kind_to_ffi(update.viewport_kind),
                scrollback_offset_rows: update.scrollback_offset_rows,
                scrollbar: scrollbar_to_ffi(update.scrollbar),
                terminal_modes: terminal_modes_to_ffi(update.terminal_modes),
                render_generation: update.render_generation,
                cursor: cursor_to_ffi(update.cursor),
                dirty: update.dirty.into(),
                ops: ptr::null(),
                op_count: ops.len(),
                image_resources: ptr::null(),
                image_resource_count: image_resources.len(),
                image_placements: ptr::null(),
                image_placement_count: image_placements.len(),
            },
            ops,
            rows,
            cells,
            image_resources,
            image_placements,
            _graphemes: graphemes,
        });
        for (cell, graphemes) in owned.cells.iter_mut().zip(owned._graphemes.iter()) {
            cell.graphemes = if graphemes.is_empty() { ptr::null() } else { graphemes.as_ptr() };
            cell.grapheme_count = graphemes.len();
        }
        let mut first_cell = 0usize;
        for row in &mut owned.rows {
            row.cells = if row.cell_count == 0 { ptr::null() } else { owned.cells[first_cell..].as_ptr() };
            first_cell += row.cell_count;
        }
        first_cell = 0;
        let mut first_row = 0usize;
        let mut row_ranges = Vec::new();
        for (op_idx, op) in owned.ops.iter_mut().enumerate() {
            op.cells = if op.cell_count == 0 { ptr::null() } else { owned.cells[first_cell..].as_ptr() };
            row_ranges.push((op_idx, first_row, op.row_desc_count));
            first_row += op.row_desc_count;
            first_cell += op.cell_count;
        }
        for (op_idx, first_row, row_desc_count) in row_ranges {
            owned.ops[op_idx].rows = if row_desc_count == 0 { ptr::null() } else { owned.rows[first_row..].as_ptr() };
            owned.ops[op_idx].row_desc_count = row_desc_count;
        }
        owned.update.ops = if owned.ops.is_empty() { ptr::null() } else { owned.ops.as_ptr() };
        owned.update.image_resources = if owned.image_resources.is_empty() { ptr::null() } else { owned.image_resources.as_ptr() };
        owned.update.image_placements = if owned.image_placements.is_empty() { ptr::null() } else { owned.image_placements.as_ptr() };
        owned
    }
}

fn image_resource_to_ffi(resource: &TerminalImageResource) -> CleatImageResource {
    CleatImageResource {
        size: std::mem::size_of::<CleatImageResource>(),
        image_id: resource.image_id,
        generation: resource.generation,
        width_px: resource.width_px,
        height_px: resource.height_px,
        format: resource.format,
        compression: resource.compression,
        data_len: resource.data_len,
    }
}

fn image_placement_to_ffi(placement: &TerminalImagePlacement) -> CleatImagePlacement {
    CleatImagePlacement {
        size: std::mem::size_of::<CleatImagePlacement>(),
        image_id: placement.image_id,
        generation: placement.generation,
        placement_id: placement.placement_id,
        z: placement.z,
        viewport_col: placement.viewport_col,
        viewport_row: placement.viewport_row,
        grid_cols: placement.grid_cols,
        grid_rows: placement.grid_rows,
        pixel_width: placement.pixel_width,
        pixel_height: placement.pixel_height,
        source_x: placement.source_x,
        source_y: placement.source_y,
        source_width: placement.source_width,
        source_height: placement.source_height,
        x_offset_px: placement.x_offset_px,
        y_offset_px: placement.y_offset_px,
        flags: placement.flags,
    }
}

fn terminal_modes_to_ffi(modes: vt::TerminalModeState) -> CleatTerminalModeState {
    CleatTerminalModeState {
        mouse_tracking: modes.mouse_tracking,
        mouse_tracking_mode: match modes.mouse_tracking_mode {
            vt::MouseTrackingMode::None => CLEAT_MOUSE_TRACKING_NONE,
            vt::MouseTrackingMode::X10 => CLEAT_MOUSE_TRACKING_X10,
            vt::MouseTrackingMode::Normal => CLEAT_MOUSE_TRACKING_NORMAL,
            vt::MouseTrackingMode::Button => CLEAT_MOUSE_TRACKING_BUTTON,
            vt::MouseTrackingMode::Any => CLEAT_MOUSE_TRACKING_ANY,
        },
        mouse_report_format: match modes.mouse_report_format {
            vt::MouseReportFormat::Legacy => CLEAT_MOUSE_FORMAT_LEGACY,
            vt::MouseReportFormat::Sgr => CLEAT_MOUSE_FORMAT_SGR,
            vt::MouseReportFormat::SgrPixels => CLEAT_MOUSE_FORMAT_SGR_PIXELS,
        },
        mouse_sgr: modes.mouse_sgr,
        mouse_sgr_pixels: modes.mouse_sgr_pixels,
        active_alternate_screen: modes.active_alternate_screen,
        application_cursor_keys: modes.application_cursor_keys,
        alternate_scroll: modes.alternate_scroll,
    }
}

fn render_update_op_kind_to_ffi(kind: TerminalRenderUpdateOpKind) -> u32 {
    match kind {
        TerminalRenderUpdateOpKind::FullVisibleReplace => CLEAT_RENDER_OP_FULL_VISIBLE_REPLACE,
        TerminalRenderUpdateOpKind::RowReplace => CLEAT_RENDER_OP_ROW_REPLACE,
        TerminalRenderUpdateOpKind::ScrollCopy => CLEAT_RENDER_OP_SCROLL_COPY,
    }
}

fn rgb_to_ffi(rgb: TerminalRgb) -> CleatRgb {
    CleatRgb { r: rgb.r, g: rgb.g, b: rgb.b }
}

fn style_color_to_ffi(color: &TerminalStyleColor) -> CleatStyleColor {
    CleatStyleColor {
        size: std::mem::size_of::<CleatStyleColor>(),
        tag: match color.tag {
            TerminalStyleColorTag::None => CLEAT_STYLE_COLOR_NONE,
            TerminalStyleColorTag::Palette => CLEAT_STYLE_COLOR_PALETTE,
            TerminalStyleColorTag::Rgb => CLEAT_STYLE_COLOR_RGB,
        },
        palette_index: color.palette_index,
        rgb: color.rgb.map(rgb_to_ffi).unwrap_or_default(),
    }
}

fn viewport_kind_to_ffi(kind: TerminalViewportKind) -> u32 {
    match kind {
        TerminalViewportKind::LiveNormal => CLEAT_VIEWPORT_LIVE_NORMAL,
        TerminalViewportKind::LiveAlternate => CLEAT_VIEWPORT_LIVE_ALTERNATE,
        TerminalViewportKind::NormalScrollback => CLEAT_VIEWPORT_NORMAL_SCROLLBACK,
    }
}

fn scrollbar_to_ffi(scrollbar: TerminalScrollbarState) -> CleatTerminalScrollbarState {
    CleatTerminalScrollbarState {
        viewport_kind: viewport_kind_to_ffi(scrollbar.viewport_kind),
        total_rows: scrollbar.total_rows,
        viewport_rows: scrollbar.viewport_rows,
        viewport_top_row: scrollbar.viewport_top_row,
        at_bottom: scrollbar.at_bottom,
    }
}

fn viewport_kind_from_ffi(kind: u32) -> Option<TerminalViewportKind> {
    match kind {
        0 | CLEAT_VIEWPORT_LIVE_NORMAL => Some(TerminalViewportKind::LiveNormal),
        CLEAT_VIEWPORT_LIVE_ALTERNATE => Some(TerminalViewportKind::LiveAlternate),
        CLEAT_VIEWPORT_NORMAL_SCROLLBACK => Some(TerminalViewportKind::NormalScrollback),
        _ => None,
    }
}

fn viewport_command_from_ffi(command: CleatViewportCommand) -> Option<ViewportCommand> {
    match command.kind {
        CLEAT_VIEWPORT_COMMAND_TOP => Some(ViewportCommand::Top),
        CLEAT_VIEWPORT_COMMAND_BOTTOM => Some(ViewportCommand::Bottom),
        CLEAT_VIEWPORT_COMMAND_DELTA_ROWS => Some(ViewportCommand::DeltaRows(command.delta_rows)),
        _ => None,
    }
}

fn viewport_command_outcome_to_ffi(outcome: ViewportCommandOutcome) -> u32 {
    match outcome {
        ViewportCommandOutcome::Moved => CLEAT_VIEWPORT_OUTCOME_MOVED,
        ViewportCommandOutcome::NoOp => CLEAT_VIEWPORT_OUTCOME_NO_OP,
        ViewportCommandOutcome::Unsupported => CLEAT_VIEWPORT_OUTCOME_UNSUPPORTED,
    }
}

#[no_mangle]
pub extern "C" fn cleat_provider_abi_version() -> u32 {
    CLEAT_PROVIDER_ABI_VERSION
}

/// # Safety
///
/// `desc` may be null. When non-null, it must point to a valid `CleatProviderDesc`.
#[no_mangle]
pub unsafe extern "C" fn cleat_provider_open(desc: *const CleatProviderDesc) -> *mut CleatProvider {
    let requested = unsafe { desc.as_ref() }.copied().unwrap_or(CleatProviderDesc {
        abi_version: CLEAT_PROVIDER_ABI_VERSION,
        requested_features: ProviderFeatures::CELL_SNAPSHOTS.bits(),
        backend: CLEAT_PROVIDER_BACKEND_MOCK,
        runtime_root: ptr::null(),
        runtime_root_len: 0,
    });
    if requested.abi_version != CLEAT_PROVIDER_ABI_VERSION {
        return ptr::null_mut();
    }
    let backend = match requested.backend {
        CLEAT_PROVIDER_BACKEND_MOCK => ProviderBackend::Mock,
        CLEAT_PROVIDER_BACKEND_IN_PROCESS => ProviderBackend::InProcess,
        CLEAT_PROVIDER_BACKEND_DAEMON => ProviderBackend::Daemon,
        _ => return ptr::null_mut(),
    };
    let runtime_root = match read_optional_utf8(requested.runtime_root, requested.runtime_root_len) {
        Ok(Some(path)) => PathBuf::from(path),
        Ok(None) => RuntimeLayout::discover().root().to_path_buf(),
        Err(_) => return ptr::null_mut(),
    };
    let features = ProviderFeatures::from_bits_truncate(requested.requested_features)
        | ProviderFeatures::CELL_SNAPSHOTS
        | ProviderFeatures::STRUCTURED_MOUSE_INPUT
        | ProviderFeatures::RENDER_UPDATES;
    Box::into_raw(Box::new(CleatProvider { features, backend, runtime_root, wake: Arc::new(Mutex::new(WakeCallback::default())) }))
}

/// # Safety
///
/// `provider` must be a valid provider pointer. `user_data` is stored
/// opaquely and passed back to `wake` when provider state transitions to dirty.
/// The callback is a scheduling nudge: it may run synchronously from a Cleat
/// API call today, and future backends may call it from provider-owned IO
/// threads. Callers should bounce to their session-owner thread before calling
/// session APIs.
#[no_mangle]
pub unsafe extern "C" fn cleat_provider_set_wake_callback(provider: *mut CleatProvider, wake: CleatWakeFn, user_data: *mut c_void) {
    if let Some(provider) = unsafe { provider.as_mut() } {
        if let Ok(mut callback) = provider.wake.lock() {
            callback.wake = wake;
            callback.user_data = user_data as usize;
        }
    }
}

/// # Safety
///
/// `provider` must be a pointer returned by `cleat_provider_open` that has not
/// already been closed.
#[no_mangle]
pub unsafe extern "C" fn cleat_provider_close(provider: *mut CleatProvider) {
    if !provider.is_null() {
        drop(unsafe { Box::from_raw(provider) });
    }
}

/// # Safety
///
/// `provider` must be a valid provider pointer. `desc` may be null. When
/// non-null, it must point to a valid `CleatSessionDesc`.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_create(provider: *mut CleatProvider, desc: *const CleatSessionDesc) -> *mut CleatSession {
    let provider = match unsafe { provider.as_ref() } {
        Some(provider) => provider,
        None => return ptr::null_mut(),
    };
    if !provider.features.contains(ProviderFeatures::CELL_SNAPSHOTS) {
        return ptr::null_mut();
    }
    let desc = unsafe { desc.as_ref() }.copied().unwrap_or(CleatSessionDesc {
        cols: 80,
        rows: 24,
        cell_width_px: 0.0,
        cell_height_px: 0.0,
        vt_engine: 0,
        command: ptr::null(),
        command_len: 0,
        cwd: ptr::null(),
        cwd_len: 0,
        id: ptr::null(),
        id_len: 0,
        record: false,
        colors: ptr::null(),
    });
    let geometry = TerminalGeometry::from_cell_size(desc.cols.max(1), desc.rows.max(1), desc.cell_width_px, desc.cell_height_px);
    let backend = match provider.backend {
        ProviderBackend::Mock => {
            let rows = desc.rows.max(1);
            SessionBackend::Mock(MockSession { cols: desc.cols.max(1), rows, observation: ObservationState::new(rows), input_count: 0 })
        }
        ProviderBackend::InProcess => match create_in_process_session(provider, desc) {
            Ok(session) => SessionBackend::InProcess(Box::new(session)),
            Err(_) => return ptr::null_mut(),
        },
        ProviderBackend::Daemon => match create_daemon_session(provider, desc) {
            Ok(session) => SessionBackend::Daemon(session),
            Err(_) => return ptr::null_mut(),
        },
    };
    Box::into_raw(Box::new(CleatSession {
        backend,
        geometry,
        next_input_sequence: 1,
        wake: provider.wake.clone(),
        last_snapshot: None,
        last_render_update: None,
    }))
}

/// # Safety
///
/// `session` must be a pointer returned by `cleat_session_create` that has not
/// already been destroyed.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_destroy(session: *mut CleatSession) {
    if !session.is_null() {
        drop(unsafe { Box::from_raw(session) });
    }
}

/// # Safety
///
/// `session` must be a valid session pointer.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_resize(session: *mut CleatSession, cols: u16, rows: u16) -> bool {
    let session = match unsafe { session.as_mut() } {
        Some(session) => session,
        None => return false,
    };
    match &mut session.backend {
        SessionBackend::Mock(mock) => {
            mock.cols = cols.max(1);
            mock.rows = rows.max(1);
            mark_full_and_wake(&mut mock.observation, mock.rows, &session.wake);
            true
        }
        SessionBackend::InProcess(in_process) => {
            in_process.actor.request_result(|reply| SessionCommand::Resize { cols: cols.max(1), rows: rows.max(1), reply }).is_ok()
        }
        SessionBackend::Daemon(daemon) => {
            if daemon_resize(daemon, cols.max(1), rows.max(1)).is_err() {
                return false;
            }
            daemon.rows = rows.max(1);
            mark_full_and_wake(&mut daemon.observation, rows.max(1), &session.wake);
            true
        }
    }
}

/// # Safety
///
/// `session` must be a valid session pointer. `geometry`, when non-null, must
/// point to a valid `CleatTerminalGeometry` for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_update_geometry(session: *mut CleatSession, geometry: *const CleatTerminalGeometry) -> bool {
    let session = match unsafe { session.as_mut() } {
        Some(session) => session,
        None => return false,
    };
    let geometry = match unsafe { geometry.as_ref() } {
        Some(geometry) => TerminalGeometry::from(*geometry),
        None => return false,
    };
    if session.geometry == geometry {
        return true;
    }
    match &mut session.backend {
        SessionBackend::Mock(mock) => {
            session.geometry = geometry;
            mark_full_and_wake(&mut mock.observation, mock.rows, &session.wake);
        }
        SessionBackend::InProcess(in_process) => {
            let (cell_width_px, cell_height_px) = geometry_cell_size_to_backend(geometry);
            if in_process.actor.request_result(|reply| SessionCommand::SetCellSize { cell_width_px, cell_height_px, reply }).is_err() {
                return false;
            }
            session.geometry = geometry;
        }
        SessionBackend::Daemon(daemon) => {
            session.geometry = geometry;
            mark_full_and_wake(&mut daemon.observation, 1, &session.wake);
        }
    }
    true
}

fn geometry_cell_size_to_backend(geometry: TerminalGeometry) -> (u32, u32) {
    (cell_px_to_backend(geometry.cell_width_px), cell_px_to_backend(geometry.cell_height_px))
}

fn cell_px_to_backend(value: f32) -> u32 {
    if value.is_finite() && value > 0.0 {
        value.round().max(1.0).min(u32::MAX as f32) as u32
    } else {
        1
    }
}

/// # Safety
///
/// `session` must be a valid session pointer. `event`, when non-null, must
/// point to a valid `CleatInputEvent` for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_send_input(session: *mut CleatSession, event: *const CleatInputEvent) -> bool {
    unsafe { cleat_session_send_input_ex(session, event, ptr::null_mut()) }
}

/// # Safety
///
/// `session` must be a valid session pointer. `event`, when non-null, must
/// point to a valid `CleatInputEvent` for the duration of the call. `out`, when
/// non-null, must point to writable `CleatInputResult` storage.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_send_input_ex(
    session: *mut CleatSession,
    event: *const CleatInputEvent,
    out: *mut CleatInputResult,
) -> bool {
    let session = match unsafe { session.as_mut() } {
        Some(session) => session,
        None => return false,
    };
    let event = match unsafe { event.as_ref() } {
        Some(event) => *event,
        None => return false,
    };
    let accepted_count = match send_input_event(session, &event) {
        Some(count) => count,
        None => return false,
    };
    record_input_acceptance(session, accepted_count, out);
    true
}

/// # Safety
///
/// `session` must be a valid session pointer. `events` must either be null with
/// `event_count == 0` or point to `event_count` readable `CleatInputEvent`
/// values. `out`, when non-null, must point to writable `CleatInputResult`
/// storage.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_send_input_batch(
    session: *mut CleatSession,
    events: *const CleatInputEvent,
    event_count: usize,
    out: *mut CleatInputResult,
) -> bool {
    let session = match unsafe { session.as_mut() } {
        Some(session) => session,
        None => return false,
    };
    if event_count > 0 && events.is_null() {
        return false;
    }
    let events = if event_count > 0 { unsafe { slice::from_raw_parts(events, event_count) } } else { &[] };
    let mut accepted_count = 0usize;
    for event in events {
        let Some(count) = send_input_event(session, event) else {
            return false;
        };
        accepted_count = accepted_count.saturating_add(count);
    }
    record_input_acceptance(session, accepted_count, out);
    true
}

fn send_input_event(session: &mut CleatSession, event: &CleatInputEvent) -> Option<usize> {
    match &mut session.backend {
        SessionBackend::Mock(mock) => match input_event_bytes(event) {
            Ok(Some(_)) => {
                mock.input_count = mock.input_count.saturating_add(1);
                mark_partial_rows_and_wake(&mut mock.observation, [0], &session.wake);
                Some(1)
            }
            Ok(None) if event.kind == CLEAT_INPUT_PASTE => match read_event_text_bytes(event) {
                Ok(bytes) if bytes.is_empty() => Some(0),
                Ok(_) => {
                    mock.input_count = mock.input_count.saturating_add(1);
                    mark_partial_rows_and_wake(&mut mock.observation, [0], &session.wake);
                    Some(1)
                }
                Err(_) => None,
            },
            Ok(None) => Some(0),
            Err(_) => None,
        },
        SessionBackend::InProcess(in_process) => match input_event_bytes(event) {
            Ok(Some(bytes)) => in_process.actor.request_result(|reply| SessionCommand::WriteInput { bytes, reply }).ok().map(|_| 1),
            Ok(None) if is_wheel_event(event) => route_in_process_wheel_event(in_process, event),
            Ok(None) if event.kind == CLEAT_INPUT_MOUSE => route_in_process_mouse_event(in_process, event),
            Ok(None) if event.kind == CLEAT_INPUT_PASTE => route_in_process_paste_event(in_process, event),
            Ok(None) => Some(0),
            Err(_) => None,
        },
        SessionBackend::Daemon(daemon) => match daemon_input_request(event) {
            Ok(Some(input)) => {
                if daemon_send_input(daemon, input).is_err() {
                    return None;
                }
                Some(1)
            }
            Ok(None) => Some(0),
            Err(_) => None,
        },
    }
}

fn is_wheel_event(event: &CleatInputEvent) -> bool {
    event.kind == CLEAT_INPUT_MOUSE && event.mouse_kind == CLEAT_MOUSE_WHEEL
}

fn route_in_process_wheel_event(in_process: &InProcessSession, event: &CleatInputEvent) -> Option<usize> {
    let wheel = SessionWheelEvent {
        modifiers: mouse_modifiers(event.modifiers),
        cell_col: event.cell_col,
        cell_row: event.cell_row,
        x_px: event.x_px,
        y_px: event.y_px,
        wheel_delta_x: event.wheel_delta_x,
        wheel_delta_y: event.wheel_delta_y,
    };
    in_process.actor.request_result(|reply| SessionCommand::Wheel { event: wheel, reply }).ok()
}

fn route_in_process_paste_event(in_process: &InProcessSession, event: &CleatInputEvent) -> Option<usize> {
    let text = read_event_text_bytes(event).ok()?;
    in_process.actor.request_result(|reply| SessionCommand::Paste { text, reply }).ok()
}

fn route_in_process_mouse_event(in_process: &InProcessSession, event: &CleatInputEvent) -> Option<usize> {
    let action = match event.mouse_kind {
        CLEAT_MOUSE_PRESS => vt::MouseAction::Press,
        CLEAT_MOUSE_RELEASE => vt::MouseAction::Release,
        CLEAT_MOUSE_MOVE => vt::MouseAction::Motion,
        _ => return Some(0),
    };
    let mouse = SessionMouseEvent {
        action,
        button: cleat_mouse_button(event.mouse_button),
        any_button_pressed: event.mouse_buttons != 0,
        modifiers: mouse_modifiers(event.modifiers),
        x_px: event.x_px,
        y_px: event.y_px,
    };
    in_process.actor.request_result(|reply| SessionCommand::Mouse { event: mouse, reply }).ok()
}

fn mouse_modifiers(modifiers: u16) -> vt::MouseModifiers {
    vt::MouseModifiers {
        shift: modifiers & CLEAT_MOD_SHIFT != 0,
        ctrl: modifiers & CLEAT_MOD_CTRL != 0,
        alt: modifiers & CLEAT_MOD_ALT != 0,
    }
}

/// Map a Cleat button id to a backend-neutral named button. Wheel directions are
/// handled separately (the wheel path), so this only covers physical buttons.
fn cleat_mouse_button(button: u32) -> Option<vt::MouseButton> {
    match button {
        CLEAT_MOUSE_BUTTON_LEFT => Some(vt::MouseButton::Left),
        CLEAT_MOUSE_BUTTON_MIDDLE => Some(vt::MouseButton::Middle),
        CLEAT_MOUSE_BUTTON_RIGHT => Some(vt::MouseButton::Right),
        CLEAT_MOUSE_BUTTON_BACK => Some(vt::MouseButton::Eight),
        CLEAT_MOUSE_BUTTON_FORWARD => Some(vt::MouseButton::Nine),
        _ => None,
    }
}

#[cfg(test)]
fn mouse_report_bytes(event: &CleatInputEvent, modes: vt::TerminalModeState) -> Option<Vec<u8>> {
    crate::host::actor::mouse_report_bytes_from_wheel(
        SessionWheelEvent {
            modifiers: mouse_modifiers(event.modifiers),
            cell_col: event.cell_col,
            cell_row: event.cell_row,
            x_px: event.x_px,
            y_px: event.y_px,
            wheel_delta_x: event.wheel_delta_x,
            wheel_delta_y: event.wheel_delta_y,
        },
        modes,
    )
}

#[cfg(test)]
fn alternate_scroll_cursor_bytes(event: &CleatInputEvent, modes: vt::TerminalModeState) -> Vec<u8> {
    crate::host::actor::alternate_scroll_cursor_bytes_from_wheel(
        SessionWheelEvent {
            modifiers: mouse_modifiers(event.modifiers),
            cell_col: event.cell_col,
            cell_row: event.cell_row,
            x_px: event.x_px,
            y_px: event.y_px,
            wheel_delta_x: event.wheel_delta_x,
            wheel_delta_y: event.wheel_delta_y,
        },
        modes,
    )
}

fn record_input_acceptance(session: &mut CleatSession, count: usize, out: *mut CleatInputResult) {
    let first_sequence = session.next_input_sequence;
    let count_u64 = u64::try_from(count).unwrap_or(u64::MAX);
    session.next_input_sequence = session.next_input_sequence.saturating_add(count_u64);
    if let Some(out) = unsafe { out.as_mut() } {
        *out = CleatInputResult { first_sequence, count };
    }
}

/// # Safety
///
/// `session` must be a valid session pointer. `bytes` must either be null with
/// `size == 0` or point to `size` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_write_bytes(session: *mut CleatSession, bytes: *const u8, size: usize) -> bool {
    let session = match unsafe { session.as_mut() } {
        Some(session) => session,
        None => return false,
    };
    if size > 0 && bytes.is_null() {
        return false;
    }
    let bytes = if size > 0 { unsafe { slice::from_raw_parts(bytes, size) } } else { &[] };
    match &mut session.backend {
        SessionBackend::Mock(mock) => {
            mock.input_count = mock.input_count.saturating_add(1);
            mark_partial_rows_and_wake(&mut mock.observation, [0], &session.wake);
            true
        }
        SessionBackend::InProcess(in_process) => {
            in_process.actor.request_result(|reply| SessionCommand::WriteInput { bytes: bytes.to_vec(), reply }).is_ok()
        }
        SessionBackend::Daemon(daemon) => {
            if daemon_write_bytes(daemon, bytes).is_err() {
                return false;
            }
            true
        }
    }
}

/// # Safety
///
/// `session` must be a valid session pointer.
///
/// Returns known dirty state. In-process sessions are progressed by their
/// provider-owned actor; this no longer pumps PTY output on the caller thread.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_poll(session: *mut CleatSession) -> CleatDirtyState {
    let session = match unsafe { session.as_mut() } {
        Some(session) => session,
        None => return CleatDirtyState::Full,
    };
    match &mut session.backend {
        SessionBackend::Mock(mock) => mock.observation.dirty().into(),
        SessionBackend::InProcess(in_process) => in_process.actor.observation().dirty().into(),
        SessionBackend::Daemon(daemon) => daemon.observation.dirty().into(),
    }
}

/// # Safety
///
/// `session` must be a valid session pointer.
///
/// Returns already-known dirty state without pumping provider IO.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_dirty(session: *const CleatSession) -> CleatDirtyState {
    unsafe { session.as_ref() }
        .map(|session| match &session.backend {
            SessionBackend::Mock(mock) => mock.observation.dirty().into(),
            SessionBackend::InProcess(in_process) => in_process.actor.observation().dirty().into(),
            SessionBackend::Daemon(daemon) => daemon.observation.dirty().into(),
        })
        .unwrap_or(CleatDirtyState::Full)
}

/// # Safety
///
/// `session` must be a valid session pointer. `generation` should be a render
/// generation returned by `cleat_session_snapshot`.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_mark_observed(session: *mut CleatSession, generation: u64) -> bool {
    let session = match unsafe { session.as_mut() } {
        Some(session) => session,
        None => return false,
    };
    match &mut session.backend {
        SessionBackend::Mock(mock) => mock.observation.mark_observed(generation),
        SessionBackend::InProcess(in_process) => {
            in_process.actor.request(|reply| SessionCommand::MarkObserved { generation, reply }, false)
        }
        SessionBackend::Daemon(daemon) => daemon.observation.mark_observed(generation),
    }
}

/// # Safety
///
/// `session` must be a valid session pointer. `out` must point to writable
/// `CleatScrollbackExtent` storage.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_scrollback_extent(session: *mut CleatSession, out: *mut CleatScrollbackExtent) -> bool {
    let session = match unsafe { session.as_mut() } {
        Some(session) => session,
        None => return false,
    };
    let out = match unsafe { out.as_mut() } {
        Some(out) => out,
        None => return false,
    };
    let extent = session_scrollback_extent(session);
    *out = CleatScrollbackExtent {
        normal_scrollback_rows: extent.normal_scrollback_rows,
        live_rows: extent.live_rows,
        alternate_screen: extent.alternate_screen,
    };
    true
}

/// # Safety
///
/// `session` must be a valid session pointer. `out` must point to writable
/// `CleatTerminalScrollbarState` storage.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_scrollbar_state(session: *mut CleatSession, out: *mut CleatTerminalScrollbarState) -> bool {
    let session = match unsafe { session.as_mut() } {
        Some(session) => session,
        None => return false,
    };
    let out = match unsafe { out.as_mut() } {
        Some(out) => out,
        None => return false,
    };
    *out = scrollbar_to_ffi(session_scrollbar_state(session));
    true
}

/// # Safety
///
/// `session` must be a valid session pointer. `command` must point to a valid
/// `CleatViewportCommand`. `out`, when non-null, must point to writable
/// `CleatViewportCommandResult` storage.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_scroll_viewport(
    session: *mut CleatSession,
    command: *const CleatViewportCommand,
    out: *mut CleatViewportCommandResult,
) -> bool {
    let session = match unsafe { session.as_mut() } {
        Some(session) => session,
        None => return false,
    };
    let command = match unsafe { command.as_ref() }.copied() {
        Some(command) => viewport_command_from_ffi(command),
        None => return false,
    };
    let outcome = session_scroll_viewport(session, command);
    if let Some(out) = unsafe { out.as_mut() } {
        out.outcome = viewport_command_outcome_to_ffi(outcome);
    }
    true
}

/// # Safety
///
/// `session` must be a valid session pointer. `out` must point to writable
/// storage for a `CleatSnapshot`. Only one snapshot may be live per session;
/// callers must release the previous snapshot before requesting another one.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_snapshot(session: *mut CleatSession, out: *mut CleatSnapshot) -> bool {
    let session = match unsafe { session.as_mut() } {
        Some(session) => session,
        None => return false,
    };
    if session.last_snapshot.is_some() {
        return false;
    }
    let out = match unsafe { out.as_mut() } {
        Some(out) => out,
        None => return false,
    };
    let mut snapshot = match &mut session.backend {
        SessionBackend::Mock(mock) => {
            let mut snapshot = mock_snapshot(mock.cols, mock.rows, mock.observation.dirty(), mock.input_count);
            mock.observation.annotate_snapshot(&mut snapshot);
            snapshot
        }
        SessionBackend::InProcess(in_process) => match in_process.actor.request_result(|reply| SessionCommand::Snapshot { reply }) {
            Ok(snapshot) => snapshot,
            Err(_) => return false,
        },
        SessionBackend::Daemon(daemon) => match daemon_snapshot(daemon) {
            Ok(mut snapshot) => {
                daemon.observation.annotate_snapshot(&mut snapshot);
                snapshot
            }
            Err(_) => return false,
        },
    };
    snapshot.geometry = session.geometry;
    snapshot.scrollbar = session_scrollbar_state(session);
    let owned = OwnedSnapshot::from_snapshot(snapshot);
    *out = owned.snapshot;
    session.last_snapshot = Some(owned);
    true
}

/// # Safety
///
/// `session` must be a valid session pointer. `out` must point to writable
/// storage for a `CleatRenderUpdate`. Only one render update may be live per
/// session; callers must release the previous update before requesting another.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_render_update(session: *mut CleatSession, out: *mut CleatRenderUpdate) -> bool {
    let session = match unsafe { session.as_mut() } {
        Some(session) => session,
        None => return false,
    };
    if session.last_render_update.is_some() {
        return false;
    }
    let out = match unsafe { out.as_mut() } {
        Some(out) => out,
        None => return false,
    };
    let mut update = match &mut session.backend {
        SessionBackend::Mock(mock) => {
            let mut snapshot = mock_snapshot(mock.cols, mock.rows, mock.observation.dirty(), mock.input_count);
            mock.observation.annotate_snapshot(&mut snapshot);
            TerminalRenderUpdate::from_snapshot(snapshot)
        }
        SessionBackend::InProcess(in_process) => match in_process.actor.request_result(|reply| SessionCommand::RenderUpdate { reply }) {
            Ok(update) => update,
            Err(_) => return false,
        },
        SessionBackend::Daemon(daemon) => match daemon_snapshot(daemon) {
            Ok(mut snapshot) => {
                daemon.observation.annotate_snapshot(&mut snapshot);
                TerminalRenderUpdate::from_snapshot(snapshot)
            }
            Err(_) => return false,
        },
    };
    update.geometry = session.geometry;
    update.scrollbar = session_scrollbar_state(session);
    let owned = OwnedRenderUpdate::from_update(update);
    *out = owned.update;
    session.last_render_update = Some(owned);
    true
}

/// # Safety
///
/// `session` must be a valid session pointer. `callback`, when non-null, is
/// called synchronously with bytes borrowed from the terminal backend. The data
/// pointer is only valid for the duration of the callback. The callback must not
/// call back into this session.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_with_image_resource_data(
    session: *mut CleatSession,
    image_id: u32,
    generation: u64,
    callback: CleatImageResourceDataFn,
    user_data: *mut c_void,
) -> bool {
    let callback = match callback {
        Some(callback) => callback,
        None => return false,
    };
    let session = match unsafe { session.as_mut() } {
        Some(session) => session,
        None => return false,
    };
    match &mut session.backend {
        SessionBackend::Mock(_) | SessionBackend::Daemon(_) => false,
        SessionBackend::InProcess(in_process) => {
            let user_data = user_data as usize;
            in_process
                .actor
                .request_result(|reply| SessionCommand::ImageResourceData {
                    image_id,
                    generation,
                    callback: Box::new(move |bytes| unsafe { callback(user_data as *mut c_void, bytes.as_ptr(), bytes.len()) }),
                    reply,
                })
                .unwrap_or(false)
        }
    }
}

/// # Safety
///
/// `session` must be a valid session pointer. `request`, when non-null, must
/// point to a valid `CleatViewportRequest`. `out` must point to writable
/// storage for a `CleatSnapshot`.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_viewport_snapshot(
    session: *mut CleatSession,
    request: *const CleatViewportRequest,
    out: *mut CleatSnapshot,
) -> bool {
    let request = unsafe { request.as_ref() }
        .copied()
        .unwrap_or(CleatViewportRequest { kind: CLEAT_VIEWPORT_LIVE_NORMAL, scrollback_offset_rows: 0 });
    let Some(kind) = viewport_kind_from_ffi(request.kind) else {
        return false;
    };
    if kind != TerminalViewportKind::LiveNormal || request.scrollback_offset_rows != 0 {
        return false;
    }
    unsafe { cleat_session_snapshot(session, out) }
}

fn session_scrollback_extent(session: &mut CleatSession) -> TerminalScrollbackExtent {
    match &mut session.backend {
        SessionBackend::Mock(mock) => TerminalScrollbackExtent { normal_scrollback_rows: 0, live_rows: mock.rows, alternate_screen: false },
        SessionBackend::InProcess(in_process) => {
            in_process.actor.request(|reply| SessionCommand::ScrollbackExtent { reply }, TerminalScrollbackExtent {
                normal_scrollback_rows: 0,
                live_rows: 0,
                alternate_screen: false,
            })
        }
        SessionBackend::Daemon(daemon) => {
            TerminalScrollbackExtent { normal_scrollback_rows: 0, live_rows: daemon.rows, alternate_screen: false }
        }
    }
}

fn session_scrollbar_state(session: &mut CleatSession) -> TerminalScrollbarState {
    match &mut session.backend {
        SessionBackend::Mock(mock) => TerminalScrollbarState::for_live_viewport(TerminalViewportKind::LiveNormal, mock.rows),
        SessionBackend::InProcess(in_process) => {
            in_process.actor.request(|reply| SessionCommand::ScrollbarState { reply }, TerminalScrollbarState::default())
        }
        SessionBackend::Daemon(daemon) => TerminalScrollbarState::for_live_viewport(TerminalViewportKind::LiveNormal, daemon.rows),
    }
}

fn session_scroll_viewport(session: &mut CleatSession, command: Option<ViewportCommand>) -> ViewportCommandOutcome {
    let Some(command) = command else {
        return ViewportCommandOutcome::Unsupported;
    };
    match &mut session.backend {
        SessionBackend::Mock(_) | SessionBackend::Daemon(_) => match command {
            ViewportCommand::Top | ViewportCommand::Bottom | ViewportCommand::DeltaRows(_) => ViewportCommandOutcome::NoOp,
        },
        SessionBackend::InProcess(in_process) => {
            match in_process.actor.request_result(|reply| SessionCommand::ScrollViewport { command, reply }) {
                Ok(outcome) => outcome,
                Err(_) => ViewportCommandOutcome::Unsupported,
            }
        }
    }
}

fn mark_full_and_wake(observation: &mut ObservationState, rows: u16, wake: &Arc<Mutex<WakeCallback>>) {
    if observation.mark_full(rows) {
        notify_wake(wake);
    }
}

fn mark_partial_rows_and_wake(observation: &mut ObservationState, rows: impl IntoIterator<Item = u16>, wake: &Arc<Mutex<WakeCallback>>) {
    if observation.mark_partial_rows(rows) {
        notify_wake(wake);
    }
}

fn notify_wake(wake: &Arc<Mutex<WakeCallback>>) {
    let callback = match wake.lock() {
        Ok(callback) => *callback,
        Err(_) => return,
    };
    if let Some(wake) = callback.wake {
        unsafe { wake(callback.user_data as *mut c_void) };
    }
}

/// # Safety
///
/// `session` must be a valid session pointer. `snapshot` may be null; when
/// non-null, it must point to a `CleatSnapshot` previously filled by
/// `cleat_session_snapshot` for this session.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_release_snapshot(session: *mut CleatSession, snapshot: *mut CleatSnapshot) {
    if let Some(session) = unsafe { session.as_mut() } {
        session.last_snapshot = None;
    }
    if let Some(snapshot) = unsafe { snapshot.as_mut() } {
        *snapshot = CleatSnapshot::default();
    }
}

/// # Safety
///
/// `session` must be a valid session pointer. `update` may be null; when
/// non-null, it must point to a `CleatRenderUpdate` previously filled by
/// `cleat_session_render_update` for this session.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_release_render_update(session: *mut CleatSession, update: *mut CleatRenderUpdate) {
    if let Some(session) = unsafe { session.as_mut() } {
        session.last_render_update = None;
    }
    if let Some(update) = unsafe { update.as_mut() } {
        *update = CleatRenderUpdate::default();
    }
}

fn create_in_process_session(provider: &CleatProvider, desc: CleatSessionDesc) -> Result<InProcessSession, String> {
    let layout = RuntimeLayout::new(provider.runtime_root.clone());
    let vt_engine = vt_engine_from_tag(desc.vt_engine)?;
    vt_engine.ensure_available()?;
    let colors = session_colors_from_desc(desc);

    let cmd = read_optional_utf8(desc.command, desc.command_len).map_err(|err| format!("command is not valid UTF-8: {err}"))?;
    let cwd = read_optional_utf8(desc.cwd, desc.cwd_len).map_err(|err| format!("cwd is not valid UTF-8: {err}"))?.map(PathBuf::from);
    let id = read_optional_utf8(desc.id, desc.id_len).map_err(|err| format!("id is not valid UTF-8: {err}"))?;
    let mut metadata = layout.create_session(id, vt_engine, cwd, cmd)?;
    metadata.record = desc.record;
    let cols = desc.cols.max(1);
    let rows = desc.rows.max(1);
    metadata.initial_size = TerminalSize { cols, rows };
    let session_dir = layout.session_dir(&metadata.id);
    let initial_geometry = TerminalGeometry::from_cell_size(cols, rows, desc.cell_width_px, desc.cell_height_px);
    let (cell_width_px, cell_height_px) = geometry_cell_size_to_backend(initial_geometry);
    let wake = provider.wake.clone();
    let actor_wake = Arc::new(move || notify_wake(&wake));
    let actor = SessionActor::spawn(rows, actor_wake, move || {
        let mut vt_engine = vt::make_vt_engine_with_colors(vt_engine, cols, rows, colors)?;
        vt_engine.set_cell_size(cell_width_px, cell_height_px)?;
        let mut runtime = SessionRuntime::spawn(session_dir, &metadata, vt_engine)?;
        runtime.resize(cols, rows)?;
        Ok(runtime)
    })
    .map_err(|err| err.replace("session actor", "in-process session actor"))?;
    actor.set_client_presence(false)?;
    Ok(InProcessSession { actor })
}

fn create_daemon_session(provider: &CleatProvider, desc: CleatSessionDesc) -> Result<DaemonSession, String> {
    let layout = RuntimeLayout::new(provider.runtime_root.clone());
    let vt_engine = vt_engine_from_tag(desc.vt_engine)?;
    let colors = session_colors_from_desc(desc);
    let cmd = read_optional_utf8(desc.command, desc.command_len).map_err(|err| format!("command is not valid UTF-8: {err}"))?;
    let cwd = read_optional_utf8(desc.cwd, desc.cwd_len).map_err(|err| format!("cwd is not valid UTF-8: {err}"))?.map(PathBuf::from);
    let id = read_optional_utf8(desc.id, desc.id_len).map_err(|err| format!("id is not valid UTF-8: {err}"))?;
    let cols = desc.cols.max(1);
    let rows = desc.rows.max(1);
    let metadata = ensure_session_started(&layout, id, Some(vt_engine), cwd, cmd, SessionStartOptions {
        record: desc.record,
        initial_size: TerminalSize { cols, rows },
        colors,
        tags: Vec::new(),
    })?;
    let mut session =
        DaemonSession { id: metadata.id, runtime_root: provider.runtime_root.clone(), rows, observation: ObservationState::new(rows) };
    daemon_resize(&mut session, cols, rows)?;
    Ok(session)
}

fn vt_engine_from_tag(tag: u32) -> Result<VtEngineKind, String> {
    match tag {
        CLEAT_PROVIDER_VT_DEFAULT => Ok(vt::default_vt_engine_kind()),
        CLEAT_PROVIDER_VT_PASSTHROUGH => Ok(VtEngineKind::Passthrough),
        CLEAT_PROVIDER_VT_GHOSTTY => Ok(VtEngineKind::Ghostty),
        other => Err(format!("unsupported vt engine tag {other}")),
    }
}

fn session_colors_from_desc(desc: CleatSessionDesc) -> TerminalColors {
    let Some(colors) = (unsafe { desc.colors.as_ref() }) else {
        return TerminalColors::default();
    };
    if colors.size < std::mem::size_of::<CleatSessionColors>() {
        return TerminalColors::default();
    }
    TerminalColors {
        default_foreground: colors.has_foreground.then_some(rgb_from_ffi(colors.foreground)),
        default_background: colors.has_background.then_some(rgb_from_ffi(colors.background)),
        default_cursor: colors.has_cursor.then_some(rgb_from_ffi(colors.cursor)),
    }
}

fn rgb_from_ffi(rgb: CleatRgb) -> Rgb {
    Rgb { r: rgb.r, g: rgb.g, b: rgb.b }
}

fn daemon_resize(session: &mut DaemonSession, cols: u16, rows: u16) -> Result<(), String> {
    let body =
        serde_json::to_vec(&serde_json::json!({ "cols": cols, "rows": rows })).map_err(|err| format!("serialize resize request: {err}"))?;
    let response = daemon_request(session, Method::POST, &format!("/sessions/{}/resize", session.id), &body)?;
    expect_status(response, StatusCode::NO_CONTENT, "resize")?;
    session.rows = rows;
    Ok(())
}

fn daemon_write_bytes(session: &mut DaemonSession, bytes: &[u8]) -> Result<(), String> {
    let body = serde_json::to_vec(&serde_json::json!({ "bytes": bytes })).map_err(|err| format!("serialize keys request: {err}"))?;
    let response = daemon_request(session, Method::POST, &format!("/sessions/{}/keys", session.id), &body)?;
    expect_status(response, StatusCode::NO_CONTENT, "keys")
}

fn daemon_send_input(session: &mut DaemonSession, input: http_uds::InputRequest) -> Result<(), String> {
    let body = serde_json::to_vec(&input).map_err(|err| format!("serialize input request: {err}"))?;
    let response = daemon_request(session, Method::POST, &format!("/sessions/{}/input", session.id), &body)?;
    expect_status(response, StatusCode::NO_CONTENT, "input")
}

fn daemon_snapshot(session: &mut DaemonSession) -> Result<TerminalSnapshot, String> {
    let response = daemon_request(session, Method::GET, &format!("/sessions/{}/snapshot", session.id), &[])?;
    if response.status != StatusCode::OK {
        return Err(format!("snapshot returned {}", response.status));
    }
    let snapshot: http_uds::SnapshotResponse =
        serde_json::from_slice(&response.body).map_err(|err| format!("parse snapshot response: {err}"))?;
    terminal_snapshot_from_http(snapshot)
}

fn daemon_request(session: &DaemonSession, method: Method, path: &str, body: &[u8]) -> Result<http_uds::HttpResponse, String> {
    let socket_path = RuntimeLayout::new(session.runtime_root.clone()).socket_path();
    let mut stream = connect_session_stream(&socket_path)?;
    http_uds::write_request(&mut stream, method, path, body).map_err(|err| format!("write HTTP request: {err}"))?;
    http_uds::read_response(&mut stream).map_err(|err| format!("read HTTP response: {err}"))
}

fn expect_status(response: http_uds::HttpResponse, expected: StatusCode, operation: &str) -> Result<(), String> {
    if response.status == expected {
        Ok(())
    } else {
        Err(format!("{operation} returned {}", response.status))
    }
}

fn terminal_snapshot_from_http(snapshot: http_uds::SnapshotResponse) -> Result<TerminalSnapshot, String> {
    Ok(TerminalSnapshot {
        cols: snapshot.cols,
        rows: snapshot.rows,
        geometry: geometry_from_http(snapshot.geometry),
        viewport_kind: viewport_kind_from_name(&snapshot.viewport_kind)?,
        scrollback_offset_rows: snapshot.scrollback_offset_rows,
        scrollbar: scrollbar_from_http(snapshot.scrollbar)?,
        terminal_modes: terminal_modes_from_http(snapshot.terminal_modes)?,
        render_generation: 0,
        cells: snapshot
            .cells
            .into_iter()
            .map(|cell| {
                Ok(TerminalCell {
                    graphemes: cell.graphemes,
                    fg: TerminalRgb { r: cell.fg.r, g: cell.fg.g, b: cell.fg.b },
                    bg: TerminalRgb { r: cell.bg.r, g: cell.bg.g, b: cell.bg.b },
                    flags: TerminalCellFlags::from_bits_truncate(cell.flags),
                    width: cell_width_from_name(&cell.width)?,
                    ..TerminalCell::default()
                })
            })
            .collect::<Result<Vec<_>, String>>()?,
        cursor: TerminalCursor {
            col: snapshot.cursor.col,
            row: snapshot.cursor.row,
            visible: snapshot.cursor.visible,
            style: cursor_style_from_name(&snapshot.cursor.style)?,
            blink: snapshot.cursor.blink,
            wide_tail: snapshot.cursor.wide_tail,
        },
        dirty: dirty_from_name(&snapshot.dirty)?,
        dirty_rows: Vec::new(),
    })
}

fn scrollbar_from_http(scrollbar: http_uds::ScrollbarResponse) -> Result<TerminalScrollbarState, String> {
    Ok(TerminalScrollbarState::new(
        viewport_kind_from_name(&scrollbar.viewport_kind)?,
        scrollbar.total_rows,
        scrollbar.viewport_rows,
        scrollbar.viewport_top_row,
    ))
}

fn geometry_from_http(geometry: http_uds::GeometryResponse) -> TerminalGeometry {
    TerminalGeometry {
        cell_width_px: geometry.cell_width_px,
        cell_height_px: geometry.cell_height_px,
        content_x_px: geometry.content_x_px,
        content_y_px: geometry.content_y_px,
        content_width_px: geometry.content_width_px,
        content_height_px: geometry.content_height_px,
    }
    .sanitized()
}

fn terminal_modes_from_http(modes: http_uds::TerminalModeResponse) -> Result<vt::TerminalModeState, String> {
    let mouse_tracking_mode = mouse_tracking_mode_from_name(&modes.mouse_tracking_mode)?;
    let mouse_report_format = mouse_report_format_from_name(&modes.mouse_report_format)?;
    Ok(vt::TerminalModeState {
        active_alternate_screen: modes.active_alternate_screen,
        application_cursor_keys: modes.application_cursor_keys,
        alternate_scroll: modes.alternate_scroll,
        mouse_tracking: modes.mouse_tracking,
        mouse_tracking_mode,
        mouse_report_format,
        mouse_sgr: modes.mouse_sgr,
        mouse_sgr_pixels: modes.mouse_sgr_pixels,
    })
}

fn dirty_from_name(name: &str) -> Result<DirtyState, String> {
    match name {
        "clean" => Ok(DirtyState::Clean),
        "partial" => Ok(DirtyState::Partial),
        "full" => Ok(DirtyState::Full),
        other => Err(format!("unknown dirty state {other}")),
    }
}

fn cell_width_from_name(name: &str) -> Result<TerminalCellWidth, String> {
    match name {
        "narrow" => Ok(TerminalCellWidth::Narrow),
        "wide" => Ok(TerminalCellWidth::Wide),
        "spacer_tail" => Ok(TerminalCellWidth::SpacerTail),
        "spacer_head" => Ok(TerminalCellWidth::SpacerHead),
        other => Err(format!("unknown cell width {other}")),
    }
}

fn cursor_style_from_name(name: &str) -> Result<TerminalCursorStyle, String> {
    match name {
        "bar" => Ok(TerminalCursorStyle::Bar),
        "block" => Ok(TerminalCursorStyle::Block),
        "underline" => Ok(TerminalCursorStyle::Underline),
        "block_hollow" => Ok(TerminalCursorStyle::BlockHollow),
        other => Err(format!("unknown cursor style {other}")),
    }
}

fn viewport_kind_from_name(name: &str) -> Result<TerminalViewportKind, String> {
    match name {
        "live_normal" => Ok(TerminalViewportKind::LiveNormal),
        "live_alternate" => Ok(TerminalViewportKind::LiveAlternate),
        "normal_scrollback" => Ok(TerminalViewportKind::NormalScrollback),
        other => Err(format!("unknown viewport kind {other}")),
    }
}

fn mouse_tracking_mode_from_name(name: &str) -> Result<vt::MouseTrackingMode, String> {
    match name {
        "none" => Ok(vt::MouseTrackingMode::None),
        "x10" => Ok(vt::MouseTrackingMode::X10),
        "normal" => Ok(vt::MouseTrackingMode::Normal),
        "button" => Ok(vt::MouseTrackingMode::Button),
        "any" => Ok(vt::MouseTrackingMode::Any),
        other => Err(format!("unknown mouse tracking mode {other}")),
    }
}

fn mouse_report_format_from_name(name: &str) -> Result<vt::MouseReportFormat, String> {
    match name {
        "legacy" => Ok(vt::MouseReportFormat::Legacy),
        "sgr" => Ok(vt::MouseReportFormat::Sgr),
        "sgr_pixels" => Ok(vt::MouseReportFormat::SgrPixels),
        other => Err(format!("unknown mouse report format {other}")),
    }
}

fn daemon_input_request(event: &CleatInputEvent) -> Result<Option<http_uds::InputRequest>, Utf8Error> {
    match event.kind {
        CLEAT_INPUT_TEXT => read_event_text(event).map(|text| Some(http_uds::InputRequest::Text { text })),
        CLEAT_INPUT_PASTE => read_event_text(event).map(|text| Some(http_uds::InputRequest::Paste { text })),
        CLEAT_INPUT_KEY => key_event_bytes(event).map(|bytes| bytes.map(|bytes| http_uds::InputRequest::RawBytes { bytes })),
        CLEAT_INPUT_RESIZE => Ok(Some(http_uds::InputRequest::Resize { cols: event.cell_col.max(1), rows: event.cell_row.max(1) })),
        CLEAT_INPUT_MOUSE | CLEAT_INPUT_FOCUS => Ok(None),
        _ => Ok(None),
    }
}

fn read_optional_utf8(ptr: *const u8, len: usize) -> Result<Option<String>, Utf8Error> {
    if ptr.is_null() || len == 0 {
        return Ok(None);
    }
    let bytes = unsafe { slice::from_raw_parts(ptr, len) };
    std::str::from_utf8(bytes).map(|value| Some(value.to_string()))
}

fn input_event_bytes(event: &CleatInputEvent) -> Result<Option<Vec<u8>>, Utf8Error> {
    match event.kind {
        CLEAT_INPUT_TEXT => read_event_text_bytes(event).map(Some),
        // Paste is routed through the engine's paste encoder (bracketed-paste
        // wrapping + unsafe-byte stripping) rather than written raw.
        CLEAT_INPUT_PASTE => Ok(None),
        CLEAT_INPUT_KEY => key_event_bytes(event),
        CLEAT_INPUT_RESIZE | CLEAT_INPUT_MOUSE | CLEAT_INPUT_FOCUS => Ok(None),
        _ => Ok(None),
    }
}

fn read_event_text_bytes(event: &CleatInputEvent) -> Result<Vec<u8>, Utf8Error> {
    read_event_text(event).map(|text| text.into_bytes())
}

fn read_event_text(event: &CleatInputEvent) -> Result<String, Utf8Error> {
    if event.text.is_null() || event.text_len == 0 {
        return Ok(String::new());
    }
    let bytes = unsafe { slice::from_raw_parts(event.text, event.text_len) };
    std::str::from_utf8(bytes).map(|text| text.to_string())
}

fn read_generated_text_bytes(event: &CleatInputEvent) -> Result<Option<Vec<u8>>, Utf8Error> {
    if event.generated_text.is_null() || event.generated_text_len == 0 {
        return Ok(None);
    }
    let bytes = unsafe { slice::from_raw_parts(event.generated_text, event.generated_text_len) };
    std::str::from_utf8(bytes).map(|_| Some(bytes.to_vec()))
}

fn key_event_bytes(event: &CleatInputEvent) -> Result<Option<Vec<u8>>, Utf8Error> {
    if event.key_action == CLEAT_KEY_ACTION_RELEASE {
        return Ok(None);
    }
    if let Some(bytes) = read_generated_text_bytes(event)? {
        return Ok(Some(bytes));
    }

    let modifiers = key_modifiers(event.modifiers);
    if event.key_kind == CLEAT_KEY_UNICODE_SCALAR {
        return Ok(keys::encode_unicode_scalar(event.key_code, modifiers));
    }

    Ok(named_key(event.key_code).and_then(|key| keys::encode_named_key(key, modifiers)))
}

fn key_modifiers(modifiers: u16) -> keys::Modifiers {
    keys::Modifiers {
        control: modifiers & CLEAT_MOD_CTRL != 0,
        meta: modifiers & CLEAT_MOD_ALT != 0,
        shift: modifiers & CLEAT_MOD_SHIFT != 0,
    }
}

fn named_key(key_code: u32) -> Option<keys::NamedKey> {
    Some(match key_code {
        CLEAT_KEY_ENTER => keys::NamedKey::Char(b'\r'),
        CLEAT_KEY_ESCAPE => keys::NamedKey::Esc,
        CLEAT_KEY_BACKSPACE => keys::NamedKey::Backspace,
        CLEAT_KEY_TAB => keys::NamedKey::Tab,
        CLEAT_KEY_DELETE => keys::NamedKey::Delete,
        CLEAT_KEY_INSERT => keys::NamedKey::Insert,
        CLEAT_KEY_HOME => keys::NamedKey::Home,
        CLEAT_KEY_END => keys::NamedKey::End,
        CLEAT_KEY_PAGE_UP => keys::NamedKey::PageUp,
        CLEAT_KEY_PAGE_DOWN => keys::NamedKey::PageDown,
        CLEAT_KEY_ARROW_UP => keys::NamedKey::Cursor { final_byte: b'A' },
        CLEAT_KEY_ARROW_DOWN => keys::NamedKey::Cursor { final_byte: b'B' },
        CLEAT_KEY_ARROW_RIGHT => keys::NamedKey::Cursor { final_byte: b'C' },
        CLEAT_KEY_ARROW_LEFT => keys::NamedKey::Cursor { final_byte: b'D' },
        CLEAT_KEY_FUNCTION_BASE..=u32::MAX => {
            let function = u8::try_from(key_code - CLEAT_KEY_FUNCTION_BASE).ok()?;
            keys::NamedKey::Function(function)
        }
        _ => return None,
    })
}

fn mock_snapshot(cols: u16, rows: u16, dirty: DirtyState, input_count: u64) -> TerminalSnapshot {
    let mut cells = Vec::with_capacity(cols as usize * rows as usize);
    for row in 0..rows {
        for col in 0..cols {
            let ch = if row == 0 && col < 5 {
                b"cleat"[col as usize] as u32
            } else if row == 1 && col < 4 {
                b"mock"[col as usize] as u32
            } else {
                ' ' as u32
            };
            cells.push(TerminalCell {
                graphemes: vec![ch],
                fg: crate::provider::TerminalRgb { r: 230, g: 230, b: 230 },
                bg: crate::provider::TerminalRgb { r: 0, g: 0, b: 0 },
                flags: if input_count > 0 { TerminalCellFlags::BOLD } else { TerminalCellFlags::empty() },
                width: TerminalCellWidth::Narrow,
                ..TerminalCell::default()
            });
        }
    }
    TerminalSnapshot {
        cols,
        rows,
        geometry: TerminalGeometry::default(),
        viewport_kind: TerminalViewportKind::LiveNormal,
        scrollback_offset_rows: 0,
        scrollbar: TerminalScrollbarState::for_live_viewport(TerminalViewportKind::LiveNormal, rows),
        terminal_modes: vt::TerminalModeState::default(),
        render_generation: 0,
        cells,
        cursor: TerminalCursor {
            col: (input_count as u16) % cols,
            row: 0,
            visible: true,
            style: TerminalCursorStyle::Block,
            blink: false,
            wide_tail: false,
        },
        dirty,
        dirty_rows: Vec::new(),
    }
}

fn cursor_to_ffi(cursor: TerminalCursor) -> CleatCursor {
    CleatCursor {
        col: cursor.col,
        row: cursor.row,
        visible: cursor.visible,
        style: match cursor.style {
            TerminalCursorStyle::Bar => CLEAT_CURSOR_STYLE_BAR,
            TerminalCursorStyle::Block => CLEAT_CURSOR_STYLE_BLOCK,
            TerminalCursorStyle::Underline => CLEAT_CURSOR_STYLE_UNDERLINE,
            TerminalCursorStyle::BlockHollow => CLEAT_CURSOR_STYLE_BLOCK_HOLLOW,
        },
        blink: cursor.blink,
        wide_tail: cursor.wide_tail,
    }
}

fn cell_width_tag(width: TerminalCellWidth) -> u32 {
    match width {
        TerminalCellWidth::Narrow => CLEAT_CELL_WIDTH_NARROW,
        TerminalCellWidth::Wide => CLEAT_CELL_WIDTH_WIDE,
        TerminalCellWidth::SpacerTail => CLEAT_CELL_WIDTH_SPACER_TAIL,
        TerminalCellWidth::SpacerHead => CLEAT_CELL_WIDTH_SPACER_HEAD,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        time::Duration,
    };

    use super::*;
    use crate::{
        host::actor::ObservationMirror,
        provider::{
            TerminalImagePlacement, TerminalImageResource, TerminalRenderCell, TerminalRenderRow, TerminalRenderStyle,
            TerminalRenderUpdateOp,
        },
    };

    unsafe extern "C" fn count_wake(user_data: *mut c_void) {
        let counter = unsafe { &*(user_data as *const AtomicUsize) };
        counter.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn observation_generation_advances_until_latest_generation_is_observed() {
        let mut observation = ObservationState::new(24);
        assert_eq!(observation.render_generation, 1);
        assert_eq!(observation.dirty(), DirtyState::Full);

        assert!(observation.mark_observed(1));
        assert_eq!(observation.dirty(), DirtyState::Clean);

        assert!(observation.mark_partial_unknown());
        assert_eq!(observation.render_generation, 2);
        assert_eq!(observation.dirty(), DirtyState::Partial);

        assert!(!observation.mark_partial_unknown());
        assert_eq!(observation.render_generation, 3);
        assert_eq!(observation.dirty(), DirtyState::Partial);

        assert!(observation.mark_observed(2));
        assert_eq!(observation.dirty(), DirtyState::Partial);
        assert!(observation.mark_observed(3));
        assert_eq!(observation.dirty(), DirtyState::Clean);
        assert!(!observation.mark_observed(4));
    }

    #[test]
    fn observation_generation_advances_for_terminal_mode_changes() {
        let mut observation = ObservationState::new(24);
        assert_eq!(observation.render_generation, 1);
        assert!(!observation.sync_terminal_modes(vt::TerminalModeState::default()));
        assert_eq!(observation.render_generation, 1);
        assert!(observation.mark_observed(1));
        assert_eq!(observation.dirty(), DirtyState::Clean);

        let modes = vt::TerminalModeState {
            mouse_tracking: true,
            mouse_tracking_mode: vt::MouseTrackingMode::Any,
            mouse_report_format: vt::MouseReportFormat::Sgr,
            mouse_sgr: true,
            ..vt::TerminalModeState::default()
        };
        assert!(observation.sync_terminal_modes(modes));
        assert_eq!(observation.render_generation, 2);
        assert_eq!(observation.dirty(), DirtyState::Partial);

        let mut update = TerminalRenderUpdate { dirty: DirtyState::Clean, ..TerminalRenderUpdate::default() };
        observation.annotate_render_update(&mut update);
        assert_eq!(update.render_generation, 2);
        assert_eq!(update.dirty, DirtyState::Partial);
        assert!(update.ops.is_empty());
    }

    #[test]
    fn session_desc_colors_convert_to_terminal_defaults() {
        let colors = CleatSessionColors {
            size: std::mem::size_of::<CleatSessionColors>(),
            has_foreground: true,
            foreground: CleatRgb { r: 1, g: 2, b: 3 },
            has_background: true,
            background: CleatRgb { r: 4, g: 5, b: 6 },
            has_cursor: false,
            cursor: CleatRgb { r: 7, g: 8, b: 9 },
        };
        let converted = session_colors_from_desc(CleatSessionDesc { colors: &colors, ..CleatSessionDesc::default() });

        assert_eq!(converted.default_foreground, Some(Rgb { r: 1, g: 2, b: 3 }));
        assert_eq!(converted.default_background, Some(Rgb { r: 4, g: 5, b: 6 }));
        assert_eq!(converted.default_cursor, None);
    }

    #[test]
    fn undersized_session_colors_are_ignored() {
        let colors = CleatSessionColors {
            size: 0,
            has_foreground: true,
            foreground: CleatRgb { r: 1, g: 2, b: 3 },
            ..CleatSessionColors::default()
        };
        let converted = session_colors_from_desc(CleatSessionDesc { colors: &colors, ..CleatSessionDesc::default() });

        assert_eq!(converted, TerminalColors::default());
    }

    #[test]
    fn mock_provider_lifecycle_returns_and_releases_snapshot() {
        unsafe {
            let provider = cleat_provider_open(&CleatProviderDesc {
                abi_version: CLEAT_PROVIDER_ABI_VERSION,
                requested_features: ProviderFeatures::CELL_SNAPSHOTS.bits(),
                ..CleatProviderDesc::default()
            });
            assert!(!provider.is_null());
            let session = cleat_session_create(provider, &CleatSessionDesc {
                cols: 8,
                rows: 3,
                cell_width_px: 10.0,
                cell_height_px: 20.0,
                ..CleatSessionDesc::default()
            });
            assert!(!session.is_null());

            let mut snapshot = CleatSnapshot::default();
            assert!(cleat_session_snapshot(session, &mut snapshot));
            assert_eq!(snapshot.cols, 8);
            assert_eq!(snapshot.rows, 3);
            assert_eq!(snapshot.cell_count, 24);
            assert!(!snapshot.cells.is_null());
            let cells = slice::from_raw_parts(snapshot.cells, snapshot.cell_count);
            assert_eq!(slice::from_raw_parts(cells[0].graphemes, cells[0].grapheme_count), &['c' as u32]);

            cleat_session_release_snapshot(session, &mut snapshot);
            assert!(snapshot.cells.is_null());
            assert_eq!(snapshot.cell_count, 0);
            cleat_session_destroy(session);
            cleat_provider_close(provider);
        }
    }

    #[test]
    fn snapshot_requires_release_before_next_snapshot() {
        unsafe {
            let provider = cleat_provider_open(ptr::null());
            let session = cleat_session_create(provider, ptr::null());

            let mut first = CleatSnapshot::default();
            let mut second = CleatSnapshot::default();
            assert!(cleat_session_snapshot(session, &mut first));
            assert!(!first.cells.is_null());
            assert!(!cleat_session_snapshot(session, &mut second));
            assert!(second.cells.is_null());

            cleat_session_release_snapshot(session, &mut first);
            assert!(cleat_session_snapshot(session, &mut second));

            cleat_session_release_snapshot(session, &mut second);
            cleat_session_destroy(session);
            cleat_provider_close(provider);
        }
    }

    #[test]
    fn live_viewport_snapshot_and_zero_scrollback_extent_are_exposed() {
        unsafe {
            let provider = cleat_provider_open(ptr::null());
            let session = cleat_session_create(provider, &CleatSessionDesc { cols: 12, rows: 5, ..CleatSessionDesc::default() });

            let mut extent = CleatScrollbackExtent::default();
            assert!(cleat_session_scrollback_extent(session, &mut extent));
            assert_eq!(extent, CleatScrollbackExtent { normal_scrollback_rows: 0, live_rows: 5, alternate_screen: false });

            let mut snapshot = CleatSnapshot::default();
            assert!(cleat_session_viewport_snapshot(session, ptr::null(), &mut snapshot));
            assert_eq!(snapshot.cols, 12);
            assert_eq!(snapshot.rows, 5);
            assert_eq!(snapshot.viewport_kind, CLEAT_VIEWPORT_LIVE_NORMAL);
            assert_eq!(snapshot.scrollback_offset_rows, 0);
            assert_eq!(snapshot.scrollbar, CleatTerminalScrollbarState {
                viewport_kind: CLEAT_VIEWPORT_LIVE_NORMAL,
                total_rows: 5,
                viewport_rows: 5,
                viewport_top_row: 0,
                at_bottom: true,
            });
            cleat_session_release_snapshot(session, &mut snapshot);

            let request = CleatViewportRequest { kind: CLEAT_VIEWPORT_NORMAL_SCROLLBACK, scrollback_offset_rows: 1 };
            assert!(!cleat_session_viewport_snapshot(session, &request, &mut snapshot));

            cleat_session_destroy(session);
            cleat_provider_close(provider);
        }
    }

    #[test]
    fn scrollbar_state_query_and_viewport_command_outcome_are_exposed() {
        unsafe {
            let provider = cleat_provider_open(ptr::null());
            let session = cleat_session_create(provider, &CleatSessionDesc { cols: 12, rows: 5, ..CleatSessionDesc::default() });

            let mut scrollbar = CleatTerminalScrollbarState::default();
            assert!(cleat_session_scrollbar_state(session, &mut scrollbar));
            assert_eq!(scrollbar, CleatTerminalScrollbarState {
                viewport_kind: CLEAT_VIEWPORT_LIVE_NORMAL,
                total_rows: 5,
                viewport_rows: 5,
                viewport_top_row: 0,
                at_bottom: true,
            });

            let mut before = CleatSnapshot::default();
            assert!(cleat_session_snapshot(session, &mut before));
            let before_generation = before.render_generation;
            cleat_session_release_snapshot(session, &mut before);

            let command = CleatViewportCommand { kind: CLEAT_VIEWPORT_COMMAND_DELTA_ROWS, delta_rows: 0 };
            let mut result = CleatViewportCommandResult::default();
            assert!(cleat_session_scroll_viewport(session, &command, &mut result));
            assert_eq!(result.outcome, CLEAT_VIEWPORT_OUTCOME_NO_OP);

            let mut after = CleatSnapshot::default();
            assert!(cleat_session_snapshot(session, &mut after));
            assert_eq!(after.render_generation, before_generation);
            cleat_session_release_snapshot(session, &mut after);

            let bad_command = CleatViewportCommand { kind: u32::MAX, delta_rows: 0 };
            assert!(cleat_session_scroll_viewport(session, &bad_command, &mut result));
            assert_eq!(result.outcome, CLEAT_VIEWPORT_OUTCOME_UNSUPPORTED);

            cleat_session_destroy(session);
            cleat_provider_close(provider);
        }
    }

    #[test]
    fn mock_provider_dirty_tracks_input_and_snapshot() {
        unsafe {
            let wake_count = AtomicUsize::new(0);
            let provider = cleat_provider_open(ptr::null());
            cleat_provider_set_wake_callback(provider, Some(count_wake), &wake_count as *const AtomicUsize as *mut c_void);
            let session = cleat_session_create(provider, ptr::null());
            assert_eq!(cleat_session_dirty(session), CleatDirtyState::Full);

            let mut initial = CleatSnapshot::default();
            assert!(cleat_session_snapshot(session, &mut initial));
            assert_eq!(initial.dirty, CleatDirtyState::Full);
            assert_eq!(initial.render_generation, 1);
            assert_eq!(cleat_session_dirty(session), CleatDirtyState::Full);
            assert!(cleat_session_mark_observed(session, initial.render_generation));
            assert_eq!(cleat_session_dirty(session), CleatDirtyState::Clean);
            cleat_session_release_snapshot(session, &mut initial);

            let event = CleatInputEvent {
                kind: CLEAT_INPUT_KEY,
                key_kind: CLEAT_KEY_NAMED,
                key_code: CLEAT_KEY_ENTER,
                ..CleatInputEvent::default()
            };
            assert!(cleat_session_send_input(session, &event));
            assert_eq!(cleat_session_dirty(session), CleatDirtyState::Partial);
            assert_eq!(wake_count.load(Ordering::SeqCst), 1);

            let mut snapshot = CleatSnapshot::default();
            assert!(cleat_session_snapshot(session, &mut snapshot));
            assert_eq!(snapshot.dirty, CleatDirtyState::Partial);
            assert_eq!(snapshot.render_generation, 2);
            assert_eq!(snapshot.dirty_row_count, 1);
            assert_eq!(slice::from_raw_parts(snapshot.dirty_rows, snapshot.dirty_row_count), &[0]);
            assert_eq!(cleat_session_dirty(session), CleatDirtyState::Partial);

            cleat_session_release_snapshot(session, &mut snapshot);
            assert!(cleat_session_send_input(session, &event));
            assert_eq!(wake_count.load(Ordering::SeqCst), 1, "dirty-to-dirty input should coalesce wakeups");
            assert!(cleat_session_mark_observed(session, 2));
            assert_eq!(cleat_session_dirty(session), CleatDirtyState::Partial, "stale observation must not clear newer dirty state");

            let mut later = CleatSnapshot::default();
            assert!(cleat_session_snapshot(session, &mut later));
            assert_eq!(later.render_generation, 3);
            cleat_session_release_snapshot(session, &mut later);
            assert!(cleat_session_mark_observed(session, 3));
            assert_eq!(cleat_session_dirty(session), CleatDirtyState::Clean);
            assert!(!cleat_session_mark_observed(session, 4), "future observations should be rejected");

            cleat_session_destroy(session);
            cleat_provider_close(provider);
        }
    }

    #[test]
    fn in_process_dirty_queries_do_not_wait_for_actor_replies() {
        let (tx, _rx) = mpsc::channel::<SessionCommand>();
        let observation = Arc::new(ObservationMirror::new());
        observation.store(1, 0, DirtyState::Full);
        let session = Box::into_raw(Box::new(CleatSession {
            backend: SessionBackend::InProcess(Box::new(InProcessSession { actor: SessionActor::from_parts(tx, observation, None) })),
            geometry: TerminalGeometry::default(),
            next_input_sequence: 1,
            wake: Arc::new(Mutex::new(WakeCallback::default())),
            last_snapshot: None,
            last_render_update: None,
        }));

        let session_addr = session as usize;
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || unsafe {
            let dirty = cleat_session_dirty(session_addr as *const CleatSession);
            let poll = cleat_session_poll(session_addr as *mut CleatSession);
            let _ = done_tx.send((dirty, poll));
        });
        let result = done_rx.recv_timeout(Duration::from_millis(100)).expect("dirty queries blocked waiting for actor replies");
        assert_eq!(result, (CleatDirtyState::Full, CleatDirtyState::Full));

        unsafe {
            cleat_session_destroy(session);
        }
    }

    #[test]
    fn in_process_dirty_queries_read_clean_mirror_state() {
        let (tx, _rx) = mpsc::channel::<SessionCommand>();
        let observation = Arc::new(ObservationMirror::new());
        observation.store(1, 1, DirtyState::Full);
        let session = Box::into_raw(Box::new(CleatSession {
            backend: SessionBackend::InProcess(Box::new(InProcessSession { actor: SessionActor::from_parts(tx, observation, None) })),
            geometry: TerminalGeometry::default(),
            next_input_sequence: 1,
            wake: Arc::new(Mutex::new(WakeCallback::default())),
            last_snapshot: None,
            last_render_update: None,
        }));

        unsafe {
            let dirty = cleat_session_dirty(session);
            let poll = cleat_session_poll(session);
            assert_eq!((dirty, poll), (CleatDirtyState::Clean, CleatDirtyState::Clean));

            cleat_session_destroy(session);
        }
    }

    #[test]
    fn in_process_dirty_queries_read_partial_mirror_state() {
        let (tx, _rx) = mpsc::channel::<SessionCommand>();
        let observation = Arc::new(ObservationMirror::new());
        observation.store(2, 1, DirtyState::Partial);
        let session = Box::into_raw(Box::new(CleatSession {
            backend: SessionBackend::InProcess(Box::new(InProcessSession { actor: SessionActor::from_parts(tx, observation, None) })),
            geometry: TerminalGeometry::default(),
            next_input_sequence: 1,
            wake: Arc::new(Mutex::new(WakeCallback::default())),
            last_snapshot: None,
            last_render_update: None,
        }));

        unsafe {
            let dirty = cleat_session_dirty(session);
            let poll = cleat_session_poll(session);
            assert_eq!((dirty, poll), (CleatDirtyState::Partial, CleatDirtyState::Partial));

            cleat_session_destroy(session);
        }
    }

    #[test]
    fn mock_provider_render_update_exposes_full_and_row_replace_ops() {
        unsafe {
            let provider = cleat_provider_open(ptr::null());
            let session = cleat_session_create(provider, &CleatSessionDesc { cols: 6, rows: 2, ..CleatSessionDesc::default() });

            let mut initial = CleatRenderUpdate::default();
            assert!(cleat_session_render_update(session, &mut initial));
            assert_eq!(initial.version, CLEAT_RENDER_UPDATE_VERSION);
            assert_eq!(initial.terminal_modes.mouse_tracking_mode, CLEAT_MOUSE_TRACKING_NONE);
            assert_eq!(initial.terminal_modes.mouse_report_format, CLEAT_MOUSE_FORMAT_LEGACY);
            assert_eq!(initial.dirty, CleatDirtyState::Full);
            assert_eq!(initial.op_count, 1);
            let initial_ops = slice::from_raw_parts(initial.ops, initial.op_count);
            assert_eq!(initial_ops[0].kind, CLEAT_RENDER_OP_FULL_VISIBLE_REPLACE);
            assert_eq!(initial_ops[0].row_desc_count, 2);
            assert!(!initial_ops[0].rows.is_null());
            assert_eq!(initial_ops[0].cell_count, 12);
            assert!(cleat_session_mark_observed(session, initial.render_generation));
            cleat_session_release_render_update(session, &mut initial);

            let event = CleatInputEvent {
                kind: CLEAT_INPUT_KEY,
                key_kind: CLEAT_KEY_NAMED,
                key_code: CLEAT_KEY_ENTER,
                ..CleatInputEvent::default()
            };
            assert!(cleat_session_send_input(session, &event));

            let mut update = CleatRenderUpdate::default();
            assert!(cleat_session_render_update(session, &mut update));
            assert_eq!(update.dirty, CleatDirtyState::Partial);
            assert_eq!(update.op_count, 1);
            let ops = slice::from_raw_parts(update.ops, update.op_count);
            assert_eq!(ops[0].kind, CLEAT_RENDER_OP_ROW_REPLACE);
            assert_eq!(ops[0].first_row, 0);
            assert_eq!(ops[0].row_count, 1);
            assert_eq!(ops[0].col_count, 6);
            assert_eq!(ops[0].row_desc_count, 1);
            let rows = slice::from_raw_parts(ops[0].rows, ops[0].row_desc_count);
            assert_eq!(rows[0].row, 0);
            assert_eq!(rows[0].col_count, 6);
            assert_eq!(rows[0].cell_count, 6);
            assert_eq!(ops[0].cell_count, 6);
            assert!(!ops[0].cells.is_null());
            cleat_session_release_render_update(session, &mut update);
            assert!(update.ops.is_null());
            assert_eq!(update.op_count, 0);

            cleat_session_destroy(session);
            cleat_provider_close(provider);
        }
    }

    #[test]
    fn owned_render_update_maps_extended_style_fields() {
        let update = TerminalRenderUpdate {
            cols: 1,
            rows: 1,
            render_generation: 7,
            dirty: DirtyState::Full,
            image_resources: vec![TerminalImageResource {
                image_id: 11,
                generation: 99,
                width_px: 3,
                height_px: 4,
                format: CLEAT_IMAGE_FORMAT_RGB,
                compression: CLEAT_IMAGE_COMPRESSION_NONE,
                data_len: 36,
            }],
            image_placements: vec![TerminalImagePlacement {
                image_id: 11,
                generation: 99,
                placement_id: 2,
                z: -1,
                viewport_col: 5,
                viewport_row: -1,
                grid_cols: 3,
                grid_rows: 2,
                pixel_width: 30,
                pixel_height: 20,
                source_x: 1,
                source_y: 2,
                source_width: 3,
                source_height: 4,
                x_offset_px: 6,
                y_offset_px: 7,
                flags: TERMINAL_IMAGE_PLACEMENT_VIRTUAL,
            }],
            ops: vec![TerminalRenderUpdateOp {
                kind: TerminalRenderUpdateOpKind::FullVisibleReplace,
                first_row: 0,
                row_count: 1,
                col_count: 1,
                rows: vec![TerminalRenderRow {
                    row: 0,
                    col_count: 1,
                    cells: vec![TerminalRenderCell {
                        graphemes: vec!['x' as u32],
                        style: TerminalRenderStyle {
                            resolved_fg: TerminalRgb { r: 1, g: 2, b: 3 },
                            resolved_bg: TerminalRgb { r: 4, g: 5, b: 6 },
                            fg_color: TerminalStyleColor::palette(2),
                            bg_color: TerminalStyleColor::rgb(TerminalRgb { r: 4, g: 5, b: 6 }),
                            underline_color: TerminalStyleColor::rgb(TerminalRgb { r: 7, g: 8, b: 9 }),
                            flags: TerminalCellFlags::BOLD | TerminalCellFlags::UNDERLINE,
                            underline_style: 5,
                            width: TerminalCellWidth::Wide,
                            protected: true,
                            semantic: 2,
                            has_hyperlink: true,
                            hyperlink_id: 42,
                            content_tag: 1,
                            has_text: true,
                            has_styling: true,
                            style_id: 9,
                        },
                    }],
                    wrap: true,
                    has_graphemes: true,
                    has_styling: true,
                    has_hyperlink: true,
                    semantic_prompt: 1,
                    dirty: true,
                    ..TerminalRenderRow::default()
                }],
                src_row: 0,
                dst_row: 0,
            }],
            ..TerminalRenderUpdate::default()
        };

        let owned = OwnedRenderUpdate::from_update(update);
        let ops = unsafe { slice::from_raw_parts(owned.update.ops, owned.update.op_count) };
        assert_eq!(ops[0].row_desc_count, 1);
        let rows = unsafe { slice::from_raw_parts(ops[0].rows, ops[0].row_desc_count) };
        assert_eq!(rows[0].row, 0);
        assert_eq!(rows[0].col_count, 1);
        assert_eq!(rows[0].cell_count, 1);
        assert!(rows[0].wrap);
        assert!(rows[0].has_graphemes);
        assert!(rows[0].has_styling);
        assert!(rows[0].has_hyperlink);
        assert_eq!(rows[0].semantic_prompt, 1);
        assert!(rows[0].dirty);
        let cells = unsafe { slice::from_raw_parts(ops[0].cells, ops[0].cell_count) };
        let style = cells[0].style;
        assert_eq!(style.size, std::mem::size_of::<CleatRenderStyle>());
        assert_eq!(style.flags, (TerminalCellFlags::BOLD | TerminalCellFlags::UNDERLINE).bits());
        assert_eq!(style.width, CLEAT_CELL_WIDTH_WIDE);
        assert_eq!(style.fg, CleatRgb { r: 1, g: 2, b: 3 });
        assert_eq!(style.bg, CleatRgb { r: 4, g: 5, b: 6 });
        assert_eq!(style.fg_color.tag, CLEAT_STYLE_COLOR_PALETTE);
        assert_eq!(style.fg_color.palette_index, 2);
        assert_eq!(style.bg_color.tag, CLEAT_STYLE_COLOR_RGB);
        assert_eq!(style.underline_style, 5);
        assert_eq!(style.underline_color.tag, CLEAT_STYLE_COLOR_RGB);
        assert_eq!(style.underline_color.rgb, CleatRgb { r: 7, g: 8, b: 9 });
        assert!(style.protected_cell);
        assert!(style.has_hyperlink);
        assert_eq!(style.semantic, 2);
        assert_eq!(style.hyperlink_id, 42);
        assert_eq!(style.content_tag, 1);
        assert!(style.has_text);
        assert!(style.has_styling);
        assert_eq!(style.style_id, 9);

        assert_eq!(owned.update.image_resource_count, 1);
        let resources = unsafe { slice::from_raw_parts(owned.update.image_resources, owned.update.image_resource_count) };
        assert_eq!(resources[0].size, std::mem::size_of::<CleatImageResource>());
        assert_eq!(resources[0].image_id, 11);
        assert_eq!(resources[0].generation, 99);
        assert_eq!(resources[0].width_px, 3);
        assert_eq!(resources[0].height_px, 4);
        assert_eq!(resources[0].format, CLEAT_IMAGE_FORMAT_RGB);
        assert_eq!(resources[0].compression, CLEAT_IMAGE_COMPRESSION_NONE);
        assert_eq!(resources[0].data_len, 36);

        assert_eq!(owned.update.image_placement_count, 1);
        let placements = unsafe { slice::from_raw_parts(owned.update.image_placements, owned.update.image_placement_count) };
        assert_eq!(placements[0].size, std::mem::size_of::<CleatImagePlacement>());
        assert_eq!(placements[0].image_id, 11);
        assert_eq!(placements[0].generation, 99);
        assert_eq!(placements[0].placement_id, 2);
        assert_eq!(placements[0].z, -1);
        assert_eq!(placements[0].viewport_col, 5);
        assert_eq!(placements[0].viewport_row, -1);
        assert_eq!(placements[0].grid_cols, 3);
        assert_eq!(placements[0].grid_rows, 2);
        assert_eq!(placements[0].pixel_width, 30);
        assert_eq!(placements[0].pixel_height, 20);
        assert_eq!(placements[0].source_x, 1);
        assert_eq!(placements[0].source_y, 2);
        assert_eq!(placements[0].source_width, 3);
        assert_eq!(placements[0].source_height, 4);
        assert_eq!(placements[0].x_offset_px, 6);
        assert_eq!(placements[0].y_offset_px, 7);
        assert_eq!(placements[0].flags, CLEAT_IMAGE_PLACEMENT_VIRTUAL);
    }

    #[test]
    fn mock_provider_updates_geometry_without_cell_resize() {
        unsafe {
            let wake_count = AtomicUsize::new(0);
            let provider = cleat_provider_open(ptr::null());
            cleat_provider_set_wake_callback(provider, Some(count_wake), &wake_count as *const AtomicUsize as *mut c_void);
            let session = cleat_session_create(provider, &CleatSessionDesc {
                cols: 8,
                rows: 3,
                cell_width_px: 10.0,
                cell_height_px: 20.0,
                ..CleatSessionDesc::default()
            });

            let mut initial = CleatSnapshot::default();
            assert!(cleat_session_snapshot(session, &mut initial));
            assert_eq!(initial.cols, 8);
            assert_eq!(initial.rows, 3);
            assert_eq!(initial.geometry.cell_width_px, 10.0);
            assert_eq!(initial.geometry.cell_height_px, 20.0);
            assert_eq!(initial.geometry.content_width_px, 80.0);
            assert_eq!(initial.geometry.content_height_px, 60.0);
            let initial_generation = initial.render_generation;
            cleat_session_release_snapshot(session, &mut initial);
            assert!(cleat_session_mark_observed(session, initial_generation));

            let geometry = CleatTerminalGeometry {
                cell_width_px: 12.0,
                cell_height_px: 24.0,
                content_x_px: 5.0,
                content_y_px: 7.0,
                content_width_px: 96.0,
                content_height_px: 72.0,
            };
            assert!(cleat_session_update_geometry(session, &geometry));
            assert_eq!(cleat_session_dirty(session), CleatDirtyState::Full);
            assert_eq!(wake_count.load(Ordering::SeqCst), 1);

            let mut snapshot = CleatSnapshot::default();
            assert!(cleat_session_snapshot(session, &mut snapshot));
            assert_eq!(snapshot.cols, 8);
            assert_eq!(snapshot.rows, 3);
            assert_eq!(snapshot.render_generation, initial_generation + 1);
            assert_eq!(snapshot.geometry, geometry);

            cleat_session_release_snapshot(session, &mut snapshot);
            assert!(cleat_session_mark_observed(session, initial_generation + 1));
            assert_eq!(cleat_session_dirty(session), CleatDirtyState::Clean);
            assert!(cleat_session_update_geometry(session, &geometry));
            assert_eq!(cleat_session_dirty(session), CleatDirtyState::Clean);
            assert_eq!(wake_count.load(Ordering::SeqCst), 1);

            cleat_session_destroy(session);
            cleat_provider_close(provider);
        }
    }

    #[test]
    fn daemon_input_request_maps_ffi_text_and_named_key_events() {
        let text = b"hello";
        let text_event =
            CleatInputEvent { kind: CLEAT_INPUT_TEXT, text: text.as_ptr(), text_len: text.len(), ..CleatInputEvent::default() };
        assert_eq!(
            daemon_input_request(&text_event).expect("text input"),
            Some(http_uds::InputRequest::Text { text: "hello".to_string() })
        );

        let key_event =
            CleatInputEvent { kind: CLEAT_INPUT_KEY, key_kind: CLEAT_KEY_NAMED, key_code: CLEAT_KEY_ENTER, ..CleatInputEvent::default() };
        assert_eq!(daemon_input_request(&key_event).expect("key input"), Some(http_uds::InputRequest::RawBytes { bytes: b"\r".to_vec() }));
    }

    #[test]
    fn key_event_bytes_use_generated_text_actions_and_shared_encoder() {
        let generated = "é";
        let generated_event = CleatInputEvent {
            kind: CLEAT_INPUT_KEY,
            key_kind: CLEAT_KEY_NAMED,
            key_code: CLEAT_KEY_F1,
            generated_text: generated.as_ptr(),
            generated_text_len: generated.len(),
            ..CleatInputEvent::default()
        };
        assert_eq!(key_event_bytes(&generated_event).expect("generated text"), Some(generated.as_bytes().to_vec()));

        let release_event = CleatInputEvent {
            kind: CLEAT_INPUT_KEY,
            key_action: CLEAT_KEY_ACTION_RELEASE,
            key_kind: CLEAT_KEY_NAMED,
            key_code: CLEAT_KEY_F1,
            ..CleatInputEvent::default()
        };
        assert_eq!(key_event_bytes(&release_event).expect("release"), None);

        let modified_function = CleatInputEvent {
            kind: CLEAT_INPUT_KEY,
            key_action: CLEAT_KEY_ACTION_REPEAT,
            key_kind: CLEAT_KEY_NAMED,
            key_code: CLEAT_KEY_F2,
            modifiers: CLEAT_MOD_SHIFT,
            ..CleatInputEvent::default()
        };
        assert_eq!(key_event_bytes(&modified_function).expect("modified function"), Some(b"\x1b[1;2Q".to_vec()));
    }

    #[test]
    fn provider_key_events_match_cli_encoder_for_equivalent_keys() {
        let cases = [
            (
                CleatInputEvent {
                    kind: CLEAT_INPUT_KEY,
                    key_kind: CLEAT_KEY_NAMED,
                    key_code: CLEAT_KEY_HOME,
                    ..CleatInputEvent::default()
                },
                vec!["Home".to_string()],
            ),
            (
                CleatInputEvent {
                    kind: CLEAT_INPUT_KEY,
                    key_kind: CLEAT_KEY_NAMED,
                    key_code: CLEAT_KEY_ARROW_LEFT,
                    modifiers: CLEAT_MOD_CTRL,
                    ..CleatInputEvent::default()
                },
                vec!["C-Left".to_string()],
            ),
            (
                CleatInputEvent {
                    kind: CLEAT_INPUT_KEY,
                    key_kind: CLEAT_KEY_NAMED,
                    key_code: CLEAT_KEY_PAGE_UP,
                    modifiers: CLEAT_MOD_ALT,
                    ..CleatInputEvent::default()
                },
                vec!["M-PageUp".to_string()],
            ),
            (
                CleatInputEvent { kind: CLEAT_INPUT_KEY, key_kind: CLEAT_KEY_NAMED, key_code: CLEAT_KEY_F12, ..CleatInputEvent::default() },
                vec!["F12".to_string()],
            ),
        ];

        for (event, tokens) in cases {
            let provider_bytes = key_event_bytes(&event).expect("provider key").expect("encoded provider key");
            let cli_bytes = keys::encode_send_keys(&tokens, false, false, 1).expect("cli key");
            assert_eq!(provider_bytes, cli_bytes);
        }
    }

    #[test]
    fn send_input_reports_single_and_batch_sequence_ranges() {
        unsafe {
            let provider = cleat_provider_open(ptr::null());
            let session = cleat_session_create(provider, ptr::null());
            let event = CleatInputEvent {
                kind: CLEAT_INPUT_KEY,
                key_kind: CLEAT_KEY_NAMED,
                key_code: CLEAT_KEY_ENTER,
                ..CleatInputEvent::default()
            };

            let mut single = CleatInputResult::default();
            assert!(cleat_session_send_input_ex(session, &event, &mut single));
            assert_eq!(single, CleatInputResult { first_sequence: 1, count: 1 });

            let events = [event, event, CleatInputEvent { key_action: CLEAT_KEY_ACTION_RELEASE, ..event }];
            let mut batch = CleatInputResult::default();
            assert!(cleat_session_send_input_batch(session, events.as_ptr(), events.len(), &mut batch));
            assert_eq!(batch, CleatInputResult { first_sequence: 2, count: 2 });

            let mut empty = CleatInputResult::default();
            assert!(cleat_session_send_input_batch(session, ptr::null(), 0, &mut empty));
            assert_eq!(empty, CleatInputResult { first_sequence: 4, count: 0 });

            cleat_session_destroy(session);
            cleat_provider_close(provider);
        }
    }

    #[test]
    fn mouse_events_are_semantic_and_do_not_emit_bytes_without_mouse_mode() {
        let event = CleatInputEvent {
            kind: CLEAT_INPUT_MOUSE,
            modifiers: CLEAT_MOD_SHIFT,
            mouse_kind: CLEAT_MOUSE_WHEEL,
            mouse_button: CLEAT_MOUSE_BUTTON_NONE,
            mouse_buttons: CLEAT_MOUSE_BUTTON_FLAG_LEFT,
            cell_col: 7,
            cell_row: 3,
            x_px: 70.5,
            y_px: 31.25,
            wheel_delta_y: -1.0,
            ..CleatInputEvent::default()
        };
        assert_eq!(input_event_bytes(&event).expect("mouse input"), None);
        assert_eq!(daemon_input_request(&event).expect("mouse daemon input"), None);

        unsafe {
            let provider = cleat_provider_open(ptr::null());
            let session = cleat_session_create(provider, ptr::null());
            let mut result = CleatInputResult::default();
            assert!(cleat_session_send_input_ex(session, &event, &mut result));
            assert_eq!(result, CleatInputResult { first_sequence: 1, count: 0 });
            cleat_session_destroy(session);
            cleat_provider_close(provider);
        }
    }

    #[test]
    fn no_byte_input_events_do_not_consume_sequence_numbers() {
        unsafe {
            let provider = cleat_provider_open(ptr::null());
            let session = cleat_session_create(provider, ptr::null());

            let release = CleatInputEvent {
                kind: CLEAT_INPUT_KEY,
                key_kind: CLEAT_KEY_NAMED,
                key_code: CLEAT_KEY_ENTER,
                key_action: CLEAT_KEY_ACTION_RELEASE,
                ..CleatInputEvent::default()
            };
            let mut release_result = CleatInputResult::default();
            assert!(cleat_session_send_input_ex(session, &release, &mut release_result));
            assert_eq!(release_result, CleatInputResult { first_sequence: 1, count: 0 });

            let press = CleatInputEvent {
                kind: CLEAT_INPUT_KEY,
                key_kind: CLEAT_KEY_NAMED,
                key_code: CLEAT_KEY_ENTER,
                ..CleatInputEvent::default()
            };
            let mut press_result = CleatInputResult::default();
            assert!(cleat_session_send_input_ex(session, &press, &mut press_result));
            assert_eq!(press_result, CleatInputResult { first_sequence: 1, count: 1 });

            cleat_session_destroy(session);
            cleat_provider_close(provider);
        }
    }

    #[test]
    fn mock_provider_paste_is_semantic_input_and_consumes_sequence_number() {
        let text = b"hello";
        let paste = CleatInputEvent { kind: CLEAT_INPUT_PASTE, text: text.as_ptr(), text_len: text.len(), ..CleatInputEvent::default() };
        assert_eq!(input_event_bytes(&paste).expect("paste input bytes"), None);

        unsafe {
            let provider = cleat_provider_open(ptr::null());
            let session = cleat_session_create(provider, ptr::null());
            let mut result = CleatInputResult::default();
            assert!(cleat_session_send_input_ex(session, &paste, &mut result));
            assert_eq!(result, CleatInputResult { first_sequence: 1, count: 1 });
            cleat_session_destroy(session);
            cleat_provider_close(provider);
        }
    }

    #[test]
    fn wheel_mouse_report_encoding_uses_sgr_coordinates_and_modifiers() {
        let event = CleatInputEvent {
            kind: CLEAT_INPUT_MOUSE,
            mouse_kind: CLEAT_MOUSE_WHEEL,
            modifiers: CLEAT_MOD_SHIFT | CLEAT_MOD_CTRL,
            cell_col: 7,
            cell_row: 3,
            wheel_delta_y: 1.0,
            ..CleatInputEvent::default()
        };
        let bytes =
            mouse_report_bytes(&event, vt::TerminalModeState { mouse_tracking: true, mouse_sgr: true, ..vt::TerminalModeState::default() })
                .expect("mouse report bytes");
        assert_eq!(bytes, b"\x1b[<84;8;4M");
    }

    #[test]
    fn alternate_scroll_wheel_encoding_uses_cursor_key_mode() {
        let event =
            CleatInputEvent { kind: CLEAT_INPUT_MOUSE, mouse_kind: CLEAT_MOUSE_WHEEL, wheel_delta_y: -2.0, ..CleatInputEvent::default() };
        let bytes = alternate_scroll_cursor_bytes(&event, vt::TerminalModeState {
            application_cursor_keys: true,
            ..vt::TerminalModeState::default()
        });
        assert_eq!(bytes, b"\x1bOB\x1bOB");
    }

    #[cfg(all(unix, feature = "ghostty-vt"))]
    #[test]
    fn in_process_viewport_move_advances_generation_and_reports_scrollback() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().to_string_lossy();
        let command = b"cat";

        unsafe {
            let wake_count = AtomicUsize::new(0);
            let provider = cleat_provider_open(&CleatProviderDesc {
                abi_version: CLEAT_PROVIDER_ABI_VERSION,
                requested_features: ProviderFeatures::CELL_SNAPSHOTS.bits(),
                backend: CLEAT_PROVIDER_BACKEND_IN_PROCESS,
                runtime_root: root.as_ptr(),
                runtime_root_len: root.len(),
            });
            assert!(!provider.is_null());
            cleat_provider_set_wake_callback(provider, Some(count_wake), &wake_count as *const AtomicUsize as *mut c_void);

            let session = cleat_session_create(provider, &CleatSessionDesc {
                cols: 20,
                rows: 3,
                vt_engine: CLEAT_PROVIDER_VT_GHOSTTY,
                command: command.as_ptr(),
                command_len: command.len(),
                ..CleatSessionDesc::default()
            });
            assert!(!session.is_null());

            let mut initial = CleatSnapshot::default();
            assert!(cleat_session_snapshot(session, &mut initial));
            assert!(cleat_session_mark_observed(session, initial.render_generation));
            cleat_session_release_snapshot(session, &mut initial);

            let input = b"line 0\nline 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\n";
            assert!(cleat_session_write_bytes(session, input.as_ptr(), input.len()));

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while cleat_session_dirty(session) == CleatDirtyState::Clean && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert_ne!(cleat_session_dirty(session), CleatDirtyState::Clean, "session did not produce scrollback output");

            let mut bottom = CleatSnapshot::default();
            assert!(cleat_session_snapshot(session, &mut bottom));
            assert_eq!(bottom.scrollbar.viewport_kind, CLEAT_VIEWPORT_LIVE_NORMAL);
            assert!(bottom.scrollbar.total_rows > bottom.scrollbar.viewport_rows as u64, "expected scrollback: {:?}", bottom.scrollbar);
            assert!(bottom.scrollbar.at_bottom);
            let bottom_generation = bottom.render_generation;
            assert!(cleat_session_mark_observed(session, bottom_generation));
            cleat_session_release_snapshot(session, &mut bottom);
            assert_eq!(cleat_session_dirty(session), CleatDirtyState::Clean);
            let wake_count_before_scroll = wake_count.load(Ordering::SeqCst);

            let command = CleatViewportCommand { kind: CLEAT_VIEWPORT_COMMAND_DELTA_ROWS, delta_rows: -2 };
            let mut result = CleatViewportCommandResult::default();
            assert!(cleat_session_scroll_viewport(session, &command, &mut result));
            assert_eq!(result.outcome, CLEAT_VIEWPORT_OUTCOME_MOVED);
            assert_eq!(cleat_session_dirty(session), CleatDirtyState::Full);
            assert_eq!(wake_count.load(Ordering::SeqCst), wake_count_before_scroll + 1);

            let mut scrolled = CleatSnapshot::default();
            assert!(cleat_session_snapshot(session, &mut scrolled));
            assert!(scrolled.render_generation > bottom_generation);
            assert_eq!(scrolled.viewport_kind, CLEAT_VIEWPORT_NORMAL_SCROLLBACK);
            assert_eq!(scrolled.scrollbar.viewport_kind, CLEAT_VIEWPORT_NORMAL_SCROLLBACK);
            assert!(!scrolled.scrollbar.at_bottom);
            let scrolled_generation = scrolled.render_generation;
            assert!(cleat_session_mark_observed(session, scrolled_generation));
            cleat_session_release_snapshot(session, &mut scrolled);
            assert_eq!(cleat_session_dirty(session), CleatDirtyState::Clean);

            let wheel = CleatInputEvent {
                kind: CLEAT_INPUT_MOUSE,
                mouse_kind: CLEAT_MOUSE_WHEEL,
                mouse_button: CLEAT_MOUSE_BUTTON_NONE,
                wheel_delta_y: -2.0,
                ..CleatInputEvent::default()
            };
            let wake_count_before_wheel = wake_count.load(Ordering::SeqCst);
            let mut input_result = CleatInputResult::default();
            assert!(cleat_session_send_input_ex(session, &wheel, &mut input_result));
            assert_eq!(input_result, CleatInputResult { first_sequence: 1, count: 0 });
            assert_eq!(cleat_session_dirty(session), CleatDirtyState::Full);
            assert_eq!(wake_count.load(Ordering::SeqCst), wake_count_before_wheel + 1);

            let mut wheel_snapshot = CleatSnapshot::default();
            assert!(cleat_session_snapshot(session, &mut wheel_snapshot));
            assert!(wheel_snapshot.render_generation > scrolled_generation);
            assert_eq!(wheel_snapshot.viewport_kind, CLEAT_VIEWPORT_LIVE_NORMAL);
            assert!(wheel_snapshot.scrollbar.at_bottom);
            cleat_session_release_snapshot(session, &mut wheel_snapshot);

            cleat_session_destroy(session);
            cleat_provider_close(provider);
        }
    }

    #[cfg(unix)]
    #[test]
    fn in_process_provider_creates_runtime_session_and_accepts_raw_input() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().to_string_lossy();
        let command = b"cat";

        unsafe {
            let provider = cleat_provider_open(&CleatProviderDesc {
                abi_version: CLEAT_PROVIDER_ABI_VERSION,
                requested_features: ProviderFeatures::CELL_SNAPSHOTS.bits(),
                backend: CLEAT_PROVIDER_BACKEND_IN_PROCESS,
                runtime_root: root.as_ptr(),
                runtime_root_len: root.len(),
            });
            assert!(!provider.is_null());

            let session = cleat_session_create(provider, &CleatSessionDesc {
                cols: 80,
                rows: 24,
                vt_engine: CLEAT_PROVIDER_VT_PASSTHROUGH,
                command: command.as_ptr(),
                command_len: command.len(),
                ..CleatSessionDesc::default()
            });
            assert!(!session.is_null());
            assert!(cleat_session_mark_observed(session, 1));
            assert_eq!(cleat_session_dirty(session), CleatDirtyState::Clean);
            assert!(cleat_session_write_bytes(session, b"hello\n".as_ptr(), b"hello\n".len()));

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while cleat_session_dirty(session) == CleatDirtyState::Clean && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            assert_eq!(cleat_session_dirty(session), CleatDirtyState::Partial);

            cleat_session_destroy(session);
            cleat_provider_close(provider);
        }
    }
}
