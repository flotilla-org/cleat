use std::{
    ffi::c_void,
    path::PathBuf,
    ptr, slice,
    str::Utf8Error,
    sync::{Arc, Mutex},
};

use crate::{
    host::actor::{ObservationState, SessionActor, SessionCommand, SessionMouseEvent, SessionWheelEvent},
    keys,
    provider::{
        DirtyState, ProviderFeatures, TerminalCell, TerminalCellFlags, TerminalCellWidth, TerminalCursor, TerminalCursorStyle,
        TerminalFocusEvent, TerminalGeometry, TerminalImagePlacement, TerminalImageResource, TerminalInputEvent, TerminalModifiers,
        TerminalMouseButtons, TerminalMouseEvent, TerminalMouseEventKind, TerminalPasteEvent, TerminalRenderUpdate,
        TerminalRenderUpdateOpKind, TerminalRgb, TerminalScrollbackExtent, TerminalScrollbarState, TerminalSnapshot, TerminalStyleColor,
        TerminalStyleColorTag, TerminalTextEvent, TerminalViewportKind, ViewportCommand, ViewportCommandOutcome,
        TERMINAL_IMAGE_PLACEMENT_VIRTUAL,
    },
    provider_daemon::{ChannelSlot, DaemonConnection},
    runtime::{RuntimeLayout, TerminalSize},
    session::{ensure_session_started, SessionStartOptions},
    session_runtime::SessionRuntime,
    vt::{self, Rgb, TerminalColors, VtEngineKind},
};

pub const CLEAT_PROVIDER_ABI_VERSION: u32 = 7;
pub const CLEAT_PROVIDER_BACKEND_MOCK: u32 = 0;
pub const CLEAT_PROVIDER_BACKEND_IN_PROCESS: u32 = 1;
pub const CLEAT_PROVIDER_BACKEND_DAEMON: u32 = 2;
pub const CLEAT_PROVIDER_VT_DEFAULT: u32 = 0;
pub const CLEAT_PROVIDER_VT_PASSTHROUGH: u32 = 1;
pub const CLEAT_PROVIDER_VT_GHOSTTY: u32 = 2;
pub const CLEAT_SESSION_CONNECTING: u32 = 0;
pub const CLEAT_SESSION_STREAMING: u32 = 1;
pub const CLEAT_SESSION_DISCONNECTED: u32 = 2;
pub const CLEAT_SESSION_CLOSED: u32 = 3;
pub const CLEAT_ROLE_UNKNOWN: u32 = 0;
pub const CLEAT_ROLE_WATCHER: u32 = 1;
pub const CLEAT_ROLE_CONTROLLER: u32 = 2;
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
pub struct CleatStr {
    pub ptr: *const u8,
    pub len: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CleatProviderDesc {
    pub abi_version: u32,
    pub requested_features: u32,
    pub backend: u32,
    pub runtime_root: *const u8,
    pub runtime_root_len: usize,
    pub daemon_name: *const u8,
    pub daemon_name_len: usize,
    pub directory_selectors: *const CleatStr,
    pub directory_selector_count: usize,
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
    pub tags: *const CleatStr,
    pub tag_count: usize,
    /// Requested attachment role for daemon sessions: CLEAT_ROLE_UNKNOWN /
    /// CLEAT_ROLE_CONTROLLER request control (the daemon may grant watcher if
    /// another controller holds the session); CLEAT_ROLE_WATCHER attaches
    /// read-only. Ignored by other backends.
    pub role: u32,
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
    daemon: Option<Arc<DaemonConnection>>,
    last_directory: Option<Box<OwnedDirectory>>,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CleatDirectoryEntry {
    pub session_id: CleatStr,
    pub state: CleatStr,
    pub tags: *const CleatStr,
    pub tag_count: usize,
    pub controller_count: u32,
    pub watcher_count: u32,
    pub recreatable: bool,
    pub cols: u16,
    pub rows: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CleatDirectory {
    pub generation: u64,
    pub entries: *const CleatDirectoryEntry,
    pub entry_count: usize,
}

/// Keeps the heap buffers behind `directory`'s FFI pointers alive.
struct OwnedDirectory {
    directory: CleatDirectory,
    _entries: Vec<CleatDirectoryEntry>,
    _tag_lists: Vec<Vec<CleatStr>>,
    _strings: Vec<String>,
}

impl OwnedDirectory {
    fn from_state(generation: u64, mut sessions: Vec<crate::packet::DirectoryEntry>) -> Box<Self> {
        struct EntryIndices {
            session_id: usize,
            state: usize,
            tags: Vec<usize>,
            controller_count: u32,
            watcher_count: u32,
            recreatable: bool,
            cols: u16,
            rows: u16,
        }
        sessions.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        let mut strings = Vec::new();
        let mut per_entry: Vec<EntryIndices> = Vec::with_capacity(sessions.len());
        for entry in sessions {
            let session_id = strings.len();
            strings.push(entry.session_id);
            let state = strings.len();
            strings.push(entry.state);
            let mut tags = Vec::with_capacity(entry.tags.len());
            for tag in entry.tags {
                tags.push(strings.len());
                strings.push(tag);
            }
            per_entry.push(EntryIndices {
                session_id,
                state,
                tags,
                controller_count: entry.controller_count,
                watcher_count: entry.watcher_count,
                recreatable: entry.recreatable,
                cols: entry.cols,
                rows: entry.rows,
            });
        }
        let str_ref = |index: usize| CleatStr { ptr: strings[index].as_ptr(), len: strings[index].len() };
        let tag_lists: Vec<Vec<CleatStr>> =
            per_entry.iter().map(|entry| entry.tags.iter().map(|&index| str_ref(index)).collect()).collect();
        let entries: Vec<CleatDirectoryEntry> = per_entry
            .iter()
            .zip(tag_lists.iter())
            .map(|(entry, tags)| CleatDirectoryEntry {
                session_id: str_ref(entry.session_id),
                state: str_ref(entry.state),
                tags: if tags.is_empty() { ptr::null() } else { tags.as_ptr() },
                tag_count: tags.len(),
                controller_count: entry.controller_count,
                watcher_count: entry.watcher_count,
                recreatable: entry.recreatable,
                cols: entry.cols,
                rows: entry.rows,
            })
            .collect();
        // Every pointer handed to the FFI targets a Vec/String HEAP buffer.
        // Moving the owning headers into the struct/Box relocates only the
        // headers, never the buffers, so the pointers computed above stay
        // valid — nothing here is self-referential and no fix-up is needed.
        let entries_ptr = if entries.is_empty() { ptr::null() } else { entries.as_ptr() };
        Box::new(Self {
            directory: CleatDirectory { generation, entries: entries_ptr, entry_count: entries.len() },
            _entries: entries,
            _tag_lists: tag_lists,
            _strings: strings,
        })
    }
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
    connection: Arc<DaemonConnection>,
    channel: u32,
    slot: Arc<Mutex<ChannelSlot>>,
}

impl Drop for DaemonSession {
    fn drop(&mut self) {
        self.connection.close_session_channel(self.channel);
    }
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
        daemon_name: ptr::null(),
        daemon_name_len: 0,
        directory_selectors: ptr::null(),
        directory_selector_count: 0,
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
        Ok(None) => match RuntimeLayout::discover() {
            Ok(layout) => layout.root().to_path_buf(),
            Err(_) => return ptr::null_mut(),
        },
        Err(_) => return ptr::null_mut(),
    };
    let daemon_name = match read_optional_utf8(requested.daemon_name, requested.daemon_name_len) {
        Ok(name) => name,
        Err(_) => return ptr::null_mut(),
    };
    let selectors = match read_selector_strings(requested.directory_selectors, requested.directory_selector_count) {
        Ok(selectors) => selectors,
        Err(_) => return ptr::null_mut(),
    };
    let features = ProviderFeatures::from_bits_truncate(requested.requested_features)
        | ProviderFeatures::CELL_SNAPSHOTS
        | ProviderFeatures::STRUCTURED_MOUSE_INPUT
        | ProviderFeatures::RENDER_UPDATES;
    let wake = Arc::new(Mutex::new(WakeCallback::default()));
    let daemon = if backend == ProviderBackend::Daemon {
        let mut layout = RuntimeLayout::new(runtime_root.clone());
        if let Some(name) = daemon_name {
            layout = match layout.with_daemon(name) {
                Ok(layout) => layout,
                Err(_) => return ptr::null_mut(),
            };
        }
        let wake_for_connection = Arc::clone(&wake);
        Some(DaemonConnection::open(layout, selectors, Arc::new(move || notify_wake(&wake_for_connection))))
    } else {
        None
    };
    Box::into_raw(Box::new(CleatProvider { features, backend, runtime_root, wake, daemon, last_directory: None }))
}

/// Generation counter of the daemon directory subscription. Starts at 0
/// before the first snapshot arrives and bumps on every snapshot or delta;
/// callers poll it and re-read the directory when it changes. Always 0 for
/// non-daemon providers.
///
/// # Safety
///
/// `provider` must be a valid provider pointer.
#[no_mangle]
pub unsafe extern "C" fn cleat_provider_directory_generation(provider: *const CleatProvider) -> u64 {
    unsafe { provider.as_ref() }
        .and_then(|provider| provider.daemon.as_ref())
        .map(|daemon| daemon.with_directory(|directory| directory.generation))
        .unwrap_or(0)
}

/// Copy the current daemon directory (session ids, opaque tags, state,
/// controller/watcher counts, sizes) into caller-visible storage. Entries are
/// sorted by session id. Only one directory may be live per provider; release
/// the previous one first.
///
/// # Safety
///
/// `provider` must be a valid provider pointer. `out` must point to writable
/// `CleatDirectory` storage. Pointers in `out` stay valid until
/// `cleat_provider_directory_release`.
#[no_mangle]
pub unsafe extern "C" fn cleat_provider_directory_snapshot(provider: *mut CleatProvider, out: *mut CleatDirectory) -> bool {
    let provider = match unsafe { provider.as_mut() } {
        Some(provider) => provider,
        None => return false,
    };
    if provider.last_directory.is_some() {
        return false;
    }
    let out = match unsafe { out.as_mut() } {
        Some(out) => out,
        None => return false,
    };
    let Some(daemon) = provider.daemon.as_ref() else {
        return false;
    };
    let (generation, sessions) =
        daemon.with_directory(|directory| (directory.generation, directory.entries.values().cloned().collect::<Vec<_>>()));
    let owned = OwnedDirectory::from_state(generation, sessions);
    *out = owned.directory;
    provider.last_directory = Some(owned);
    true
}

/// # Safety
///
/// `provider` must be a valid provider pointer. `directory` must be the live
/// directory returned by `cleat_provider_directory_snapshot`.
#[no_mangle]
pub unsafe extern "C" fn cleat_provider_directory_release(provider: *mut CleatProvider, directory: *mut CleatDirectory) {
    let Some(provider) = (unsafe { provider.as_mut() }) else {
        return;
    };
    if let Some(directory) = unsafe { directory.as_mut() } {
        *directory = CleatDirectory::default();
    }
    provider.last_directory = None;
}

fn read_selector_strings(selectors: *const CleatStr, count: usize) -> Result<Vec<String>, Utf8Error> {
    if count == 0 || selectors.is_null() {
        return Ok(Vec::new());
    }
    let slices = unsafe { slice::from_raw_parts(selectors, count) };
    slices.iter().map(|selector| read_optional_utf8(selector.ptr, selector.len).map(|value| value.unwrap_or_default())).collect()
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
        let provider = unsafe { Box::from_raw(provider) };
        if let Some(daemon) = &provider.daemon {
            daemon.shutdown();
        }
        drop(provider);
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
        tags: ptr::null(),
        tag_count: 0,
        role: CLEAT_ROLE_UNKNOWN,
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

/// Attach to an existing daemon session by id instead of creating one. The
/// session must already exist in the daemon's directory; if it does not, the
/// daemon reports an error on the channel and the session surfaces
/// `CLEAT_SESSION_CLOSED`. Only `id`, `cols`, `rows`, `cell_width_px`, and
/// `cell_height_px` of the desc are honored.
///
/// # Safety
///
/// `provider` must be a valid provider pointer opened with the daemon
/// backend. `desc` must point to a valid `CleatSessionDesc` with a non-empty
/// id.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_attach(provider: *mut CleatProvider, desc: *const CleatSessionDesc) -> *mut CleatSession {
    let provider = match unsafe { provider.as_ref() } {
        Some(provider) => provider,
        None => return ptr::null_mut(),
    };
    if provider.backend != ProviderBackend::Daemon {
        return ptr::null_mut();
    }
    let desc = match unsafe { desc.as_ref() } {
        Some(desc) => *desc,
        None => return ptr::null_mut(),
    };
    let geometry = TerminalGeometry::from_cell_size(desc.cols.max(1), desc.rows.max(1), desc.cell_width_px, desc.cell_height_px);
    let backend = match attach_daemon_session(provider, desc) {
        Ok(session) => SessionBackend::Daemon(session),
        Err(_) => return ptr::null_mut(),
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

/// Daemon session id (the identity used for attach-by-id and shown in the
/// directory). False for in-process and mock sessions, which have no daemon
/// identity.
///
/// # Safety
///
/// `session` must be a valid session pointer. `out` must point to writable
/// `CleatStr` storage. The returned pointer borrows from the session and stays
/// valid until the session is destroyed.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_id(session: *const CleatSession, out: *mut CleatStr) -> bool {
    let session = match unsafe { session.as_ref() } {
        Some(session) => session,
        None => return false,
    };
    let out = match unsafe { out.as_mut() } {
        Some(out) => out,
        None => return false,
    };
    match &session.backend {
        SessionBackend::Mock(_) | SessionBackend::InProcess(_) => false,
        SessionBackend::Daemon(daemon) => {
            *out = CleatStr { ptr: daemon.id.as_ptr(), len: daemon.id.len() };
            true
        }
    }
}

/// Granted attachment role (CLEAT_ROLE_*). In-process and mock sessions are
/// their own controllers. Daemon sessions report CLEAT_ROLE_UNKNOWN until the
/// daemon's grant arrives, then the granted role — which may change later
/// (another client can take control; the wake callback fires on the change).
///
/// # Safety
///
/// `session` must be a valid session pointer.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_role(session: *const CleatSession) -> u32 {
    let session = match unsafe { session.as_ref() } {
        Some(session) => session,
        None => return CLEAT_ROLE_UNKNOWN,
    };
    match &session.backend {
        SessionBackend::Mock(_) | SessionBackend::InProcess(_) => CLEAT_ROLE_CONTROLLER,
        SessionBackend::Daemon(daemon) => match daemon.slot.lock().ok().and_then(|slot| slot.granted_role) {
            Some(crate::packet::ChannelRole::Controller) => CLEAT_ROLE_CONTROLLER,
            Some(crate::packet::ChannelRole::Watcher) => CLEAT_ROLE_WATCHER,
            None => CLEAT_ROLE_UNKNOWN,
        },
    }
}

/// Request the controller role, preempting another packet controller if one
/// holds the session (a legacy stream controller is never preempted). The
/// grant lands asynchronously: poll `cleat_session_role` after the wake
/// callback fires. Returns false if the request could not be sent.
///
/// # Safety
///
/// `session` must be a valid session pointer.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_take_control(session: *mut CleatSession) -> bool {
    let session = match unsafe { session.as_mut() } {
        Some(session) => session,
        None => return false,
    };
    match &session.backend {
        SessionBackend::Mock(_) | SessionBackend::InProcess(_) => true,
        SessionBackend::Daemon(daemon) => {
            daemon.connection.request_role(daemon.channel, crate::packet::ChannelRole::Controller, true).is_ok()
        }
    }
}

/// Connection state of the session's transport. In-process and mock sessions
/// are always `CLEAT_SESSION_STREAMING`. Daemon sessions report
/// `CLEAT_SESSION_CONNECTING` until the first render packet arrives,
/// `CLEAT_SESSION_DISCONNECTED` while the daemon connection is down (it
/// reconnects with backoff and recovers the stream), and
/// `CLEAT_SESSION_CLOSED` once the daemon reported the session gone.
///
/// # Safety
///
/// `session` must be a valid session pointer.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_connection_state(session: *const CleatSession) -> u32 {
    let session = match unsafe { session.as_ref() } {
        Some(session) => session,
        None => return CLEAT_SESSION_CLOSED,
    };
    match &session.backend {
        SessionBackend::Mock(_) | SessionBackend::InProcess(_) => CLEAT_SESSION_STREAMING,
        SessionBackend::Daemon(daemon) => {
            // Copy the slot fields before checking connectivity:
            // is_connected() takes the connection-state lock, and the reader
            // thread takes connection-state -> slot (install_connection,
            // request_role), so holding the slot across it would invert the
            // lock order and deadlock the connection.
            let (closed, streaming) = match daemon.slot.lock() {
                Ok(slot) => (slot.closed.is_some(), slot.pending.is_some() || slot.last.render_generation > 0),
                Err(_) => return CLEAT_SESSION_CLOSED,
            };
            if closed {
                CLEAT_SESSION_CLOSED
            } else if !daemon.connection.is_connected() {
                CLEAT_SESSION_DISCONNECTED
            } else if streaming {
                CLEAT_SESSION_STREAMING
            } else {
                CLEAT_SESSION_CONNECTING
            }
        }
    }
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
            let (cols, rows) = (cols.max(1), rows.max(1));
            if let Ok(mut slot) = daemon.slot.lock() {
                slot.desired_cols = cols;
                slot.desired_rows = rows;
            }
            // Best-effort while connected; the desired size is re-asserted on
            // every reconnect, so a send failure is not a caller error.
            let _ = daemon.connection.send_resize(daemon.channel, cols, rows);
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
        SessionBackend::Daemon(_) => {
            session.geometry = geometry;
            notify_wake(&session.wake);
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
        SessionBackend::Daemon(daemon) => match packet_input_event(event) {
            Ok(Some(input)) => {
                if daemon.connection.send_input(daemon.channel, input).is_err() {
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
            daemon.connection.send_input(daemon.channel, TerminalInputEvent::RawBytes(bytes.to_vec())).is_ok()
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
        SessionBackend::Daemon(daemon) => daemon_slot_dirty(daemon).into(),
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
            SessionBackend::Daemon(daemon) => daemon_slot_dirty(daemon).into(),
        })
        .unwrap_or(CleatDirtyState::Full)
}

fn daemon_slot_dirty(daemon: &DaemonSession) -> DirtyState {
    daemon.slot.lock().map(|slot| slot.dirty()).unwrap_or(DirtyState::Clean)
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
        // Daemon renders are acked (and thereby marked observed daemon-side)
        // when the pending packet is consumed by cleat_session_render_update.
        SessionBackend::Daemon(_) => true,
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

/// Always returns false for daemon-backed sessions: they are fed by render
/// updates rather than snapshots, and no full-grid state is held client-side.
/// Use `cleat_session_render_update` instead; the first update after a channel
/// opens is full-dirty and carries the complete grid.
///
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
        // Daemon sessions are render-update-fed; there is no full-grid state
        // FFI-side to serve a snapshot from.
        SessionBackend::Daemon(_) => return false,
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
        SessionBackend::Daemon(daemon) => {
            let taken = {
                let mut slot = match daemon.slot.lock() {
                    Ok(slot) => slot,
                    Err(_) => return false,
                };
                match slot.pending.take() {
                    Some(update) => {
                        slot.last.absorb(&update);
                        Some(update)
                    }
                    None => None,
                }
            };
            match taken {
                Some(update) => {
                    // Consuming the packet is the observation; ack it so the
                    // daemon may send the next one (one-un-acked backpressure).
                    let _ = daemon.connection.send_ack(daemon.channel, update.render_generation);
                    update
                }
                None => match daemon.slot.lock() {
                    Ok(slot) => slot.last.clean_update(),
                    Err(_) => return false,
                },
            }
        }
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
        SessionBackend::Daemon(daemon) => daemon
            .slot
            .lock()
            .map(|slot| TerminalScrollbackExtent {
                normal_scrollback_rows: slot.last.scrollbar.total_rows.saturating_sub(u64::from(slot.last.rows)),
                live_rows: slot.last.rows,
                alternate_screen: slot.last.terminal_modes.active_alternate_screen,
            })
            .unwrap_or(TerminalScrollbackExtent { normal_scrollback_rows: 0, live_rows: 0, alternate_screen: false }),
    }
}

fn session_scrollbar_state(session: &mut CleatSession) -> TerminalScrollbarState {
    match &mut session.backend {
        SessionBackend::Mock(mock) => TerminalScrollbarState::for_live_viewport(TerminalViewportKind::LiveNormal, mock.rows),
        SessionBackend::InProcess(in_process) => {
            in_process.actor.request(|reply| SessionCommand::ScrollbarState { reply }, TerminalScrollbarState::default())
        }
        SessionBackend::Daemon(daemon) => daemon
            .slot
            .lock()
            .map(|slot| slot.last.scrollbar)
            .unwrap_or_else(|_| TerminalScrollbarState::for_live_viewport(TerminalViewportKind::LiveNormal, 0)),
    }
}

fn session_scroll_viewport(session: &mut CleatSession, command: Option<ViewportCommand>) -> ViewportCommandOutcome {
    let Some(command) = command else {
        return ViewportCommandOutcome::Unsupported;
    };
    match &mut session.backend {
        SessionBackend::Mock(_) => match command {
            ViewportCommand::Top | ViewportCommand::Bottom | ViewportCommand::DeltaRows(_) => ViewportCommandOutcome::NoOp,
        },
        SessionBackend::InProcess(in_process) => {
            match in_process.actor.request_result(|reply| SessionCommand::ScrollViewport { command, reply }) {
                Ok(outcome) => outcome,
                Err(_) => ViewportCommandOutcome::Unsupported,
            }
        }
        // Fire-and-forget over the packet connection: the true outcome shows
        // up as viewport/scrollbar state in the next render packet, so report
        // Moved optimistically on send.
        SessionBackend::Daemon(daemon) => match daemon.connection.send_viewport(daemon.channel, command) {
            Ok(()) => ViewportCommandOutcome::Moved,
            Err(_) => ViewportCommandOutcome::NoOp,
        },
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
    let connection = provider.daemon.as_ref().ok_or_else(|| "provider has no daemon connection".to_string())?;
    let vt_engine = vt_engine_from_tag(desc.vt_engine)?;
    let colors = session_colors_from_desc(desc);
    let cmd = read_optional_utf8(desc.command, desc.command_len).map_err(|err| format!("command is not valid UTF-8: {err}"))?;
    let cwd = read_optional_utf8(desc.cwd, desc.cwd_len).map_err(|err| format!("cwd is not valid UTF-8: {err}"))?.map(PathBuf::from);
    let id = read_optional_utf8(desc.id, desc.id_len).map_err(|err| format!("id is not valid UTF-8: {err}"))?;
    let cols = desc.cols.max(1);
    let rows = desc.rows.max(1);
    let tags = read_selector_strings(desc.tags, desc.tag_count).map_err(|err| format!("tag is not valid UTF-8: {err}"))?;
    let metadata = ensure_session_started(connection.layout(), id, Some(vt_engine), cwd, cmd, SessionStartOptions {
        record: desc.record,
        initial_size: TerminalSize { cols, rows },
        colors,
        tags,
        environment: Vec::new(),
    })?;
    let (channel, slot) = connection.open_session_channel(metadata.id.clone(), cols, rows, channel_role_from_ffi(desc.role)?);
    Ok(DaemonSession { id: metadata.id, connection: Arc::clone(connection), channel, slot })
}

fn attach_daemon_session(provider: &CleatProvider, desc: CleatSessionDesc) -> Result<DaemonSession, String> {
    let connection = provider.daemon.as_ref().ok_or_else(|| "provider has no daemon connection".to_string())?;
    let id = read_optional_utf8(desc.id, desc.id_len)
        .map_err(|err| format!("id is not valid UTF-8: {err}"))?
        .ok_or_else(|| "attach requires a session id".to_string())?;
    let (channel, slot) =
        connection.open_session_channel(id.clone(), desc.cols.max(1), desc.rows.max(1), channel_role_from_ffi(desc.role)?);
    Ok(DaemonSession { id, connection: Arc::clone(connection), channel, slot })
}

fn channel_role_from_ffi(role: u32) -> Result<crate::packet::ChannelRole, String> {
    match role {
        CLEAT_ROLE_UNKNOWN | CLEAT_ROLE_CONTROLLER => Ok(crate::packet::ChannelRole::Controller),
        CLEAT_ROLE_WATCHER => Ok(crate::packet::ChannelRole::Watcher),
        other => Err(format!("unsupported role tag {other}")),
    }
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

fn read_optional_utf8(ptr: *const u8, len: usize) -> Result<Option<String>, Utf8Error> {
    if ptr.is_null() || len == 0 {
        return Ok(None);
    }
    let bytes = unsafe { slice::from_raw_parts(ptr, len) };
    std::str::from_utf8(bytes).map(|value| Some(value.to_string()))
}

/// Map an FFI input event onto the packet-transported `TerminalInputEvent`.
/// Key events are encoded to bytes locally (matching the in-process path) so
/// both backends share one key-encoding implementation; mouse and paste events
/// stay structured because their encoding is terminal-mode-dependent and the
/// session actor resolves them daemon-side.
fn packet_input_event(event: &CleatInputEvent) -> Result<Option<TerminalInputEvent>, Utf8Error> {
    match event.kind {
        CLEAT_INPUT_TEXT => read_event_text(event).map(|text| Some(TerminalInputEvent::Text(TerminalTextEvent { text }))),
        CLEAT_INPUT_PASTE => read_event_text(event).map(|text| Some(TerminalInputEvent::Paste(TerminalPasteEvent { text }))),
        CLEAT_INPUT_KEY => key_event_bytes(event).map(|bytes| bytes.map(TerminalInputEvent::RawBytes)),
        CLEAT_INPUT_MOUSE => Ok(packet_mouse_event(event)),
        CLEAT_INPUT_FOCUS => Ok(Some(TerminalInputEvent::Focus(TerminalFocusEvent { focused: event.focused }))),
        CLEAT_INPUT_RESIZE => Ok(Some(TerminalInputEvent::Resize(crate::provider::TerminalResizeEvent {
            cols: event.cell_col.max(1),
            rows: event.cell_row.max(1),
            cell_width_px: event.x_px,
            cell_height_px: event.y_px,
        }))),
        _ => Ok(None),
    }
}

fn packet_mouse_event(event: &CleatInputEvent) -> Option<TerminalInputEvent> {
    let kind = match event.mouse_kind {
        CLEAT_MOUSE_PRESS => TerminalMouseEventKind::Press,
        CLEAT_MOUSE_RELEASE => TerminalMouseEventKind::Release,
        CLEAT_MOUSE_MOVE => TerminalMouseEventKind::Move,
        CLEAT_MOUSE_WHEEL => TerminalMouseEventKind::Wheel,
        _ => return None,
    };
    let button = match event.mouse_button {
        CLEAT_MOUSE_BUTTON_LEFT => Some(crate::provider::TerminalMouseButton::Left),
        CLEAT_MOUSE_BUTTON_MIDDLE => Some(crate::provider::TerminalMouseButton::Middle),
        CLEAT_MOUSE_BUTTON_RIGHT => Some(crate::provider::TerminalMouseButton::Right),
        CLEAT_MOUSE_BUTTON_BACK => Some(crate::provider::TerminalMouseButton::Back),
        CLEAT_MOUSE_BUTTON_FORWARD => Some(crate::provider::TerminalMouseButton::Forward),
        _ => None,
    };
    Some(TerminalInputEvent::Mouse(TerminalMouseEvent {
        kind,
        button,
        buttons: TerminalMouseButtons::from_bits_truncate(event.mouse_buttons),
        modifiers: TerminalModifiers::from_bits_truncate(event.modifiers),
        cell_col: event.cell_col,
        cell_row: event.cell_row,
        x_px: event.x_px,
        y_px: event.y_px,
        wheel_delta_x: event.wheel_delta_x,
        wheel_delta_y: event.wheel_delta_y,
    }))
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
    fn packet_input_event_maps_ffi_text_named_key_and_mouse_events() {
        let text = b"hello";
        let text_event =
            CleatInputEvent { kind: CLEAT_INPUT_TEXT, text: text.as_ptr(), text_len: text.len(), ..CleatInputEvent::default() };
        assert_eq!(
            packet_input_event(&text_event).expect("text input"),
            Some(TerminalInputEvent::Text(TerminalTextEvent { text: "hello".to_string() }))
        );

        let key_event =
            CleatInputEvent { kind: CLEAT_INPUT_KEY, key_kind: CLEAT_KEY_NAMED, key_code: CLEAT_KEY_ENTER, ..CleatInputEvent::default() };
        assert_eq!(packet_input_event(&key_event).expect("key input"), Some(TerminalInputEvent::RawBytes(b"\r".to_vec())));

        let wheel_event = CleatInputEvent {
            kind: CLEAT_INPUT_MOUSE,
            mouse_kind: CLEAT_MOUSE_WHEEL,
            modifiers: CLEAT_MOD_SHIFT,
            cell_col: 3,
            cell_row: 4,
            wheel_delta_y: -2.5,
            ..CleatInputEvent::default()
        };
        match packet_input_event(&wheel_event).expect("wheel input") {
            Some(TerminalInputEvent::Mouse(mouse)) => {
                assert_eq!(mouse.kind, TerminalMouseEventKind::Wheel);
                assert_eq!(mouse.modifiers, TerminalModifiers::SHIFT);
                assert_eq!((mouse.cell_col, mouse.cell_row), (3, 4));
                assert_eq!(mouse.wheel_delta_y, -2.5);
            }
            other => panic!("expected mouse event, got {other:?}"),
        }
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
                ..CleatProviderDesc::default()
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
                ..CleatProviderDesc::default()
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
