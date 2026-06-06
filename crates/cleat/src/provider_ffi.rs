use std::{
    ffi::c_void,
    path::PathBuf,
    ptr, slice,
    str::Utf8Error,
    sync::{Arc, Mutex},
};

use http::{Method, StatusCode};

use crate::{
    http_uds, keys,
    platform::ipc::connect_session_stream,
    protocol::SignalTarget,
    provider::{
        DirtyState, ProviderFeatures, TerminalCell, TerminalCellFlags, TerminalCellWidth, TerminalCursor, TerminalCursorStyle,
        TerminalGeometry, TerminalRgb, TerminalScrollbackExtent, TerminalSnapshot, TerminalViewportKind,
    },
    runtime::RuntimeLayout,
    session::{ensure_session_started, session_socket_path},
    session_runtime::{PtyOutput, SessionRuntime},
    vt::{self, VtEngineKind},
};

pub const CLEAT_PROVIDER_ABI_VERSION: u32 = 1;
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

const POSIX_SIGTERM: i32 = 15;

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
    pub record: bool,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CleatRgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
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
pub struct CleatSnapshot {
    pub cols: u16,
    pub rows: u16,
    pub geometry: CleatTerminalGeometry,
    pub viewport_kind: u32,
    pub scrollback_offset_rows: u64,
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
pub struct CleatInputEvent {
    pub kind: u32,
    pub modifiers: u16,
    pub consumed_modifiers: u16,
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
pub struct CleatViewportRequest {
    pub kind: u32,
    pub scrollback_offset_rows: u64,
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
    runtime: SessionRuntime,
    observation: ObservationState,
    exited: bool,
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

#[derive(Clone, Debug)]
struct ObservationState {
    render_generation: u64,
    observed_generation: u64,
    dirty: DirtyState,
    dirty_rows: Vec<u16>,
}

impl ObservationState {
    fn new(rows: u16) -> Self {
        let mut state = Self { render_generation: 0, observed_generation: 0, dirty: DirtyState::Clean, dirty_rows: Vec::new() };
        state.mark_full(rows);
        state
    }

    fn dirty(&self) -> DirtyState {
        if self.render_generation > self.observed_generation {
            self.dirty
        } else {
            DirtyState::Clean
        }
    }

    fn mark_full(&mut self, _rows: u16) -> bool {
        let was_clean = self.dirty() == DirtyState::Clean;
        self.render_generation = self.render_generation.saturating_add(1);
        self.dirty = DirtyState::Full;
        self.dirty_rows.clear();
        was_clean
    }

    fn mark_partial_rows(&mut self, rows: impl IntoIterator<Item = u16>) -> bool {
        let was_clean = self.dirty() == DirtyState::Clean;
        self.render_generation = self.render_generation.saturating_add(1);
        if self.dirty != DirtyState::Full {
            self.dirty = DirtyState::Partial;
            for row in rows {
                if !self.dirty_rows.contains(&row) {
                    self.dirty_rows.push(row);
                }
            }
            self.dirty_rows.sort_unstable();
        }
        was_clean
    }

    fn mark_partial_unknown(&mut self) -> bool {
        let was_clean = self.dirty() == DirtyState::Clean;
        self.render_generation = self.render_generation.saturating_add(1);
        if self.dirty != DirtyState::Full {
            self.dirty = DirtyState::Partial;
            self.dirty_rows.clear();
        }
        was_clean
    }

    fn mark_observed(&mut self, generation: u64) -> bool {
        if generation > self.render_generation {
            return false;
        }
        self.observed_generation = self.observed_generation.max(generation);
        if self.observed_generation >= self.render_generation {
            self.dirty = DirtyState::Clean;
            self.dirty_rows.clear();
        }
        true
    }

    fn annotate_snapshot(&self, snapshot: &mut TerminalSnapshot) {
        snapshot.render_generation = self.render_generation;
        snapshot.dirty = self.dirty();
        snapshot.dirty_rows = if snapshot.dirty == DirtyState::Partial { self.dirty_rows.clone() } else { Vec::new() };
    }
}

impl Drop for InProcessSession {
    fn drop(&mut self) {
        if !self.exited {
            let _ = self.runtime.dispatch_signal(POSIX_SIGTERM, SignalTarget::Leader);
        }
    }
}

struct OwnedSnapshot {
    snapshot: CleatSnapshot,
    cells: Vec<CleatCell>,
    dirty_rows: Vec<u16>,
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

fn viewport_kind_to_ffi(kind: TerminalViewportKind) -> u32 {
    match kind {
        TerminalViewportKind::LiveNormal => CLEAT_VIEWPORT_LIVE_NORMAL,
        TerminalViewportKind::LiveAlternate => CLEAT_VIEWPORT_LIVE_ALTERNATE,
        TerminalViewportKind::NormalScrollback => CLEAT_VIEWPORT_NORMAL_SCROLLBACK,
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
        | ProviderFeatures::STRUCTURED_MOUSE_INPUT;
    Box::into_raw(Box::new(CleatProvider { features, backend, runtime_root, wake: Arc::new(Mutex::new(WakeCallback::default())) }))
}

/// # Safety
///
/// `provider` must be a valid provider pointer. `user_data` is stored
/// opaquely and passed back to `wake` when provider state transitions from
/// clean to dirty.
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
        record: false,
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
    Box::into_raw(Box::new(CleatSession { backend, geometry, next_input_sequence: 1, wake: provider.wake.clone(), last_snapshot: None }))
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
            if in_process.runtime.resize(cols.max(1), rows.max(1)).is_err() {
                return false;
            }
            mark_full_and_wake(&mut in_process.observation, rows.max(1), &session.wake);
            true
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
    session.geometry = geometry;
    match &mut session.backend {
        SessionBackend::Mock(mock) => mark_full_and_wake(&mut mock.observation, mock.rows, &session.wake),
        SessionBackend::InProcess(in_process) => {
            let rows = in_process.runtime.inspect(false).terminal.rows;
            mark_full_and_wake(&mut in_process.observation, rows, &session.wake);
        }
        SessionBackend::Daemon(daemon) => mark_full_and_wake(&mut daemon.observation, 1, &session.wake),
    }
    true
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
    if !send_input_event(session, &event) {
        return false;
    }
    record_input_acceptance(session, 1, out);
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
    for event in events {
        if !send_input_event(session, event) {
            return false;
        }
    }
    record_input_acceptance(session, event_count, out);
    true
}

fn send_input_event(session: &mut CleatSession, event: &CleatInputEvent) -> bool {
    match &mut session.backend {
        SessionBackend::Mock(mock) => {
            mock.input_count = mock.input_count.saturating_add(1);
            mark_partial_rows_and_wake(&mut mock.observation, [0], &session.wake);
            true
        }
        SessionBackend::InProcess(in_process) => match input_event_bytes(event) {
            Ok(Some(bytes)) => {
                if in_process.runtime.write_input(&bytes).is_err() {
                    return false;
                }
                true
            }
            Ok(None) => true,
            Err(_) => false,
        },
        SessionBackend::Daemon(daemon) => match daemon_input_request(event) {
            Ok(Some(input)) => {
                if daemon_send_input(daemon, input).is_err() {
                    return false;
                }
                true
            }
            Ok(None) => true,
            Err(_) => false,
        },
    }
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
            if in_process.runtime.write_input(bytes).is_err() {
                return false;
            }
            true
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
#[no_mangle]
pub unsafe extern "C" fn cleat_session_poll(session: *mut CleatSession) -> CleatDirtyState {
    let session = match unsafe { session.as_mut() } {
        Some(session) => session,
        None => return CleatDirtyState::Full,
    };
    match &mut session.backend {
        SessionBackend::Mock(mock) => mock.observation.dirty().into(),
        SessionBackend::InProcess(in_process) => {
            match pump_in_process_session(in_process) {
                Ok(PumpOutcome::Clean) => {}
                Ok(PumpOutcome::PartialUnknown) => mark_partial_unknown_and_wake(&mut in_process.observation, &session.wake),
                Ok(PumpOutcome::Full) | Err(_) => {
                    let rows = in_process.runtime.inspect(false).terminal.rows;
                    mark_full_and_wake(&mut in_process.observation, rows, &session.wake);
                }
            }
            in_process.observation.dirty().into()
        }
        SessionBackend::Daemon(daemon) => daemon.observation.dirty().into(),
    }
}

/// # Safety
///
/// `session` must be a valid session pointer.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_dirty(session: *const CleatSession) -> CleatDirtyState {
    unsafe { session.as_ref() }
        .map(|session| match &session.backend {
            SessionBackend::Mock(mock) => mock.observation.dirty().into(),
            SessionBackend::InProcess(in_process) => in_process.observation.dirty().into(),
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
        SessionBackend::InProcess(in_process) => in_process.observation.mark_observed(generation),
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
        SessionBackend::InProcess(in_process) => {
            let dirty = in_process.observation.dirty();
            let mut snapshot = match in_process.runtime.snapshot(dirty) {
                Ok(snapshot) => snapshot,
                Err(_) => return false,
            };
            in_process.observation.annotate_snapshot(&mut snapshot);
            snapshot
        }
        SessionBackend::Daemon(daemon) => match daemon_snapshot(daemon) {
            Ok(mut snapshot) => {
                daemon.observation.annotate_snapshot(&mut snapshot);
                snapshot
            }
            Err(_) => return false,
        },
    };
    snapshot.geometry = session.geometry;
    let owned = OwnedSnapshot::from_snapshot(snapshot);
    *out = owned.snapshot;
    session.last_snapshot = Some(owned);
    true
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
    let live_rows = match &mut session.backend {
        SessionBackend::Mock(mock) => mock.rows,
        SessionBackend::InProcess(in_process) => in_process.runtime.inspect(false).terminal.rows,
        SessionBackend::Daemon(daemon) => daemon.rows,
    };
    TerminalScrollbackExtent { normal_scrollback_rows: 0, live_rows, alternate_screen: false }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PumpOutcome {
    Clean,
    PartialUnknown,
    Full,
}

fn pump_in_process_session(in_process: &mut InProcessSession) -> Result<PumpOutcome, String> {
    let mut exited_now = false;
    if !in_process.exited {
        if let Some(exit_code) = in_process.runtime.exit_code_if_exited()? {
            in_process.runtime.record_exit_code(exit_code);
            in_process.exited = true;
            exited_now = true;
        }
    }
    let output = if exited_now {
        in_process.runtime.drain_output_after_exit(false)?
    } else if in_process.exited {
        PtyOutput { chunks: Vec::new() }
    } else {
        in_process.runtime.read_available_output(false)?
    };
    if exited_now {
        Ok(PumpOutcome::Full)
    } else if !output.chunks.is_empty() {
        Ok(PumpOutcome::PartialUnknown)
    } else {
        Ok(PumpOutcome::Clean)
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

fn mark_partial_unknown_and_wake(observation: &mut ObservationState, wake: &Arc<Mutex<WakeCallback>>) {
    if observation.mark_partial_unknown() {
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

fn create_in_process_session(provider: &CleatProvider, desc: CleatSessionDesc) -> Result<InProcessSession, String> {
    let layout = RuntimeLayout::new(provider.runtime_root.clone());
    let vt_engine = vt_engine_from_tag(desc.vt_engine)?;
    vt_engine.ensure_available()?;

    let cmd = read_optional_utf8(desc.command, desc.command_len).map_err(|err| format!("command is not valid UTF-8: {err}"))?;
    let cwd = read_optional_utf8(desc.cwd, desc.cwd_len).map_err(|err| format!("cwd is not valid UTF-8: {err}"))?.map(PathBuf::from);
    let mut metadata = layout.create_session(None, vt_engine, cwd, cmd)?;
    metadata.record = desc.record;
    let session_dir = layout.root().join(&metadata.id);
    let mut runtime = SessionRuntime::spawn(session_dir, &metadata, vt::make_vt_engine(vt_engine, desc.cols.max(1), desc.rows.max(1))?)?;
    runtime.resize(desc.cols.max(1), desc.rows.max(1))?;

    Ok(InProcessSession { runtime, observation: ObservationState::new(desc.rows.max(1)), exited: false })
}

fn create_daemon_session(provider: &CleatProvider, desc: CleatSessionDesc) -> Result<DaemonSession, String> {
    let layout = RuntimeLayout::new(provider.runtime_root.clone());
    let vt_engine = vt_engine_from_tag(desc.vt_engine)?;
    let cmd = read_optional_utf8(desc.command, desc.command_len).map_err(|err| format!("command is not valid UTF-8: {err}"))?;
    let cwd = read_optional_utf8(desc.cwd, desc.cwd_len).map_err(|err| format!("cwd is not valid UTF-8: {err}"))?.map(PathBuf::from);
    let metadata = ensure_session_started(&layout, None, Some(vt_engine), cwd, cmd, desc.record)?;
    let mut session = DaemonSession {
        id: metadata.id,
        runtime_root: provider.runtime_root.clone(),
        rows: desc.rows.max(1),
        observation: ObservationState::new(desc.rows.max(1)),
    };
    daemon_resize(&mut session, desc.cols.max(1), desc.rows.max(1))?;
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
    let socket_path = session_socket_path(&session.runtime_root, &session.id);
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
                })
            })
            .collect::<Result<Vec<_>, String>>()?,
        cursor: TerminalCursor {
            col: snapshot.cursor.col,
            row: snapshot.cursor.row,
            visible: snapshot.cursor.visible,
            style: cursor_style_from_name(&snapshot.cursor.style)?,
            wide_tail: snapshot.cursor.wide_tail,
        },
        dirty: dirty_from_name(&snapshot.dirty)?,
        dirty_rows: Vec::new(),
    })
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
        CLEAT_INPUT_TEXT | CLEAT_INPUT_PASTE => read_event_text_bytes(event).map(Some),
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
            });
        }
    }
    TerminalSnapshot {
        cols,
        rows,
        geometry: TerminalGeometry::default(),
        viewport_kind: TerminalViewportKind::LiveNormal,
        scrollback_offset_rows: 0,
        render_generation: 0,
        cells,
        cursor: TerminalCursor {
            col: (input_count as u16) % cols,
            row: 0,
            visible: true,
            style: TerminalCursorStyle::Block,
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

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
            cleat_session_release_snapshot(session, &mut snapshot);

            let request = CleatViewportRequest { kind: CLEAT_VIEWPORT_NORMAL_SCROLLBACK, scrollback_offset_rows: 1 };
            assert!(!cleat_session_viewport_snapshot(session, &request, &mut snapshot));

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

            let event = CleatInputEvent { kind: 1, ..CleatInputEvent::default() };
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
            assert_eq!(batch, CleatInputResult { first_sequence: 2, count: 3 });

            let mut empty = CleatInputResult::default();
            assert!(cleat_session_send_input_batch(session, ptr::null(), 0, &mut empty));
            assert_eq!(empty, CleatInputResult { first_sequence: 5, count: 0 });

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
            assert_eq!(result, CleatInputResult { first_sequence: 1, count: 1 });
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
            assert_eq!(cleat_session_dirty(session), CleatDirtyState::Clean, "accepted input should not imply completed output");

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while cleat_session_poll(session) == CleatDirtyState::Clean && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            assert_eq!(cleat_session_dirty(session), CleatDirtyState::Partial);

            cleat_session_destroy(session);
            cleat_provider_close(provider);
        }
    }
}
