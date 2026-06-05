use std::{path::PathBuf, ptr, slice, str::Utf8Error};

use http::{Method, StatusCode};

use crate::{
    http_uds,
    platform::ipc::connect_session_stream,
    protocol::SignalTarget,
    provider::{
        DirtyState, ProviderFeatures, TerminalCell, TerminalCellFlags, TerminalCellWidth, TerminalCursor, TerminalCursorStyle, TerminalRgb,
        TerminalSnapshot,
    },
    runtime::RuntimeLayout,
    session::{ensure_session_started, session_socket_path},
    session_runtime::SessionRuntime,
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
pub const CLEAT_KEY_ARROW_UP: u32 = 12;
pub const CLEAT_KEY_ARROW_DOWN: u32 = 13;
pub const CLEAT_KEY_ARROW_LEFT: u32 = 14;
pub const CLEAT_KEY_ARROW_RIGHT: u32 = 15;

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
pub struct CleatSnapshot {
    pub cols: u16,
    pub rows: u16,
    pub cells: *const CleatCell,
    pub cell_count: usize,
    pub cursor: CleatCursor,
    pub dirty: CleatDirtyState,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CleatInputEvent {
    pub kind: u32,
    pub modifiers: u16,
    pub key_kind: u32,
    pub key_code: u32,
    pub text: *const u8,
    pub text_len: usize,
    pub cell_col: u16,
    pub cell_row: u16,
    pub x_px: f32,
    pub y_px: f32,
    pub wheel_delta_x: f32,
    pub wheel_delta_y: f32,
}

pub struct CleatProvider {
    features: ProviderFeatures,
    backend: ProviderBackend,
    runtime_root: PathBuf,
}

pub struct CleatSession {
    backend: SessionBackend,
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
    dirty: DirtyState,
    input_count: u64,
}

struct InProcessSession {
    runtime: SessionRuntime,
    dirty: DirtyState,
    exited: bool,
}

struct DaemonSession {
    id: String,
    runtime_root: PathBuf,
    dirty: DirtyState,
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
                cells: ptr::null(),
                cell_count: cells.len(),
                cursor: cursor_to_ffi(snapshot.cursor),
                dirty: snapshot.dirty.into(),
            },
            cells,
            _graphemes: graphemes,
        });
        owned.snapshot.cells = owned.cells.as_ptr();
        owned
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
    Box::into_raw(Box::new(CleatProvider { features, backend, runtime_root }))
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
    let backend = match provider.backend {
        ProviderBackend::Mock => {
            SessionBackend::Mock(MockSession { cols: desc.cols.max(1), rows: desc.rows.max(1), dirty: DirtyState::Full, input_count: 0 })
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
    Box::into_raw(Box::new(CleatSession { backend, last_snapshot: None }))
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
pub unsafe extern "C" fn cleat_session_resize(session: *mut CleatSession, cols: u16, rows: u16, _cell_w_px: f32, _cell_h_px: f32) -> bool {
    let session = match unsafe { session.as_mut() } {
        Some(session) => session,
        None => return false,
    };
    match &mut session.backend {
        SessionBackend::Mock(mock) => {
            mock.cols = cols.max(1);
            mock.rows = rows.max(1);
            mock.dirty = DirtyState::Full;
            true
        }
        SessionBackend::InProcess(in_process) => {
            if in_process.runtime.resize(cols.max(1), rows.max(1)).is_err() {
                return false;
            }
            in_process.dirty = DirtyState::Full;
            true
        }
        SessionBackend::Daemon(daemon) => {
            if daemon_resize(daemon, cols.max(1), rows.max(1)).is_err() {
                return false;
            }
            daemon.dirty = DirtyState::Full;
            true
        }
    }
}

/// # Safety
///
/// `session` must be a valid session pointer. `event`, when non-null, must
/// point to a valid `CleatInputEvent` for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_send_input(session: *mut CleatSession, event: *const CleatInputEvent) -> bool {
    let session = match unsafe { session.as_mut() } {
        Some(session) => session,
        None => return false,
    };
    let event = match unsafe { event.as_ref() } {
        Some(event) => *event,
        None => return false,
    };
    match &mut session.backend {
        SessionBackend::Mock(mock) => {
            mock.input_count = mock.input_count.saturating_add(1);
            mock.dirty = DirtyState::Partial;
            true
        }
        SessionBackend::InProcess(in_process) => match input_event_bytes(&event) {
            Ok(Some(bytes)) => {
                if in_process.runtime.write_input(&bytes).is_err() {
                    return false;
                }
                in_process.dirty = DirtyState::Partial;
                true
            }
            Ok(None) => true,
            Err(_) => false,
        },
        SessionBackend::Daemon(daemon) => match daemon_input_request(&event) {
            Ok(Some(input)) => {
                if daemon_send_input(daemon, input).is_err() {
                    return false;
                }
                daemon.dirty = DirtyState::Partial;
                true
            }
            Ok(None) => true,
            Err(_) => false,
        },
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
            mock.dirty = DirtyState::Partial;
            true
        }
        SessionBackend::InProcess(in_process) => {
            if in_process.runtime.write_input(bytes).is_err() {
                return false;
            }
            in_process.dirty = DirtyState::Partial;
            true
        }
        SessionBackend::Daemon(daemon) => {
            if daemon_write_bytes(daemon, bytes).is_err() {
                return false;
            }
            daemon.dirty = DirtyState::Partial;
            true
        }
    }
}

/// # Safety
///
/// `session` must be a valid session pointer.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_dirty(session: *const CleatSession) -> CleatDirtyState {
    unsafe { session.as_ref() }
        .map(|session| match &session.backend {
            SessionBackend::Mock(mock) => mock.dirty.into(),
            SessionBackend::InProcess(in_process) => in_process.dirty.into(),
            SessionBackend::Daemon(daemon) => daemon.dirty.into(),
        })
        .unwrap_or(CleatDirtyState::Full)
}

/// # Safety
///
/// `session` must be a valid session pointer. `out` must point to writable
/// storage for a `CleatSnapshot`.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_snapshot(session: *mut CleatSession, out: *mut CleatSnapshot) -> bool {
    let session = match unsafe { session.as_mut() } {
        Some(session) => session,
        None => return false,
    };
    let out = match unsafe { out.as_mut() } {
        Some(out) => out,
        None => return false,
    };
    let snapshot = match &mut session.backend {
        SessionBackend::Mock(mock) => {
            let snapshot = mock_snapshot(mock.cols, mock.rows, mock.dirty, mock.input_count);
            mock.dirty = DirtyState::Clean;
            snapshot
        }
        SessionBackend::InProcess(in_process) => {
            let mut exited_now = false;
            if let Ok(Some(exit_code)) = in_process.runtime.exit_code_if_exited() {
                in_process.runtime.record_exit_code(exit_code);
                in_process.exited = true;
                exited_now = true;
            }
            let output = if exited_now {
                in_process.runtime.drain_output_after_exit(false)
            } else {
                in_process.runtime.read_available_output(false)
            };
            if output.is_err() {
                return false;
            }
            let dirty = in_process.dirty;
            let snapshot = match in_process.runtime.snapshot(dirty) {
                Ok(snapshot) => snapshot,
                Err(_) => return false,
            };
            in_process.dirty = DirtyState::Clean;
            snapshot
        }
        SessionBackend::Daemon(daemon) => match daemon_snapshot(daemon) {
            Ok(snapshot) => {
                daemon.dirty = DirtyState::Clean;
                snapshot
            }
            Err(_) => return false,
        },
    };
    let owned = OwnedSnapshot::from_snapshot(snapshot);
    *out = owned.snapshot;
    session.last_snapshot = Some(owned);
    true
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

    Ok(InProcessSession { runtime, dirty: DirtyState::Full, exited: false })
}

fn create_daemon_session(provider: &CleatProvider, desc: CleatSessionDesc) -> Result<DaemonSession, String> {
    let layout = RuntimeLayout::new(provider.runtime_root.clone());
    let vt_engine = vt_engine_from_tag(desc.vt_engine)?;
    let cmd = read_optional_utf8(desc.command, desc.command_len).map_err(|err| format!("command is not valid UTF-8: {err}"))?;
    let cwd = read_optional_utf8(desc.cwd, desc.cwd_len).map_err(|err| format!("cwd is not valid UTF-8: {err}"))?.map(PathBuf::from);
    let metadata = ensure_session_started(&layout, None, Some(vt_engine), cwd, cmd, desc.record)?;
    let mut session = DaemonSession { id: metadata.id, runtime_root: provider.runtime_root.clone(), dirty: DirtyState::Full };
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
    expect_status(response, StatusCode::NO_CONTENT, "resize")
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

fn daemon_input_request(event: &CleatInputEvent) -> Result<Option<http_uds::InputRequest>, Utf8Error> {
    match event.kind {
        CLEAT_INPUT_TEXT => read_event_text(event).map(|text| Some(http_uds::InputRequest::Text { text })),
        CLEAT_INPUT_PASTE => read_event_text(event).map(|text| Some(http_uds::InputRequest::Paste { text })),
        CLEAT_INPUT_KEY => Ok(key_input_request(event).map(|key| http_uds::InputRequest::Key { key })),
        CLEAT_INPUT_RESIZE => Ok(Some(http_uds::InputRequest::Resize { cols: event.cell_col.max(1), rows: event.cell_row.max(1) })),
        CLEAT_INPUT_MOUSE | CLEAT_INPUT_FOCUS => Ok(None),
        _ => Ok(None),
    }
}

fn key_input_request(event: &CleatInputEvent) -> Option<http_uds::KeyRequest> {
    if event.key_kind == CLEAT_KEY_UNICODE_SCALAR {
        return Some(http_uds::KeyRequest::UnicodeScalar { codepoint: event.key_code });
    }

    let key = match event.key_code {
        CLEAT_KEY_ENTER => http_uds::NamedKey::Enter,
        CLEAT_KEY_ESCAPE => http_uds::NamedKey::Escape,
        CLEAT_KEY_BACKSPACE => http_uds::NamedKey::Backspace,
        CLEAT_KEY_TAB => http_uds::NamedKey::Tab,
        CLEAT_KEY_DELETE => http_uds::NamedKey::Delete,
        CLEAT_KEY_ARROW_UP => http_uds::NamedKey::ArrowUp,
        CLEAT_KEY_ARROW_DOWN => http_uds::NamedKey::ArrowDown,
        CLEAT_KEY_ARROW_RIGHT => http_uds::NamedKey::ArrowRight,
        CLEAT_KEY_ARROW_LEFT => http_uds::NamedKey::ArrowLeft,
        _ => return None,
    };
    Some(http_uds::KeyRequest::Named { key })
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
        CLEAT_INPUT_KEY => key_event_bytes(event).map(Some),
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

fn key_event_bytes(event: &CleatInputEvent) -> Result<Vec<u8>, Utf8Error> {
    if event.key_kind == CLEAT_KEY_UNICODE_SCALAR {
        let mut bytes = Vec::new();
        if let Some(ch) = char::from_u32(event.key_code) {
            let mut buf = [0; 4];
            bytes.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
        }
        return Ok(bytes);
    }

    let bytes = match event.key_code {
        CLEAT_KEY_ENTER => b"\r".as_slice(),
        CLEAT_KEY_ESCAPE => b"\x1b".as_slice(),
        CLEAT_KEY_BACKSPACE => b"\x7f".as_slice(),
        CLEAT_KEY_TAB => b"\t".as_slice(),
        CLEAT_KEY_DELETE => b"\x1b[3~".as_slice(),
        CLEAT_KEY_ARROW_UP => b"\x1b[A".as_slice(),
        CLEAT_KEY_ARROW_DOWN => b"\x1b[B".as_slice(),
        CLEAT_KEY_ARROW_RIGHT => b"\x1b[C".as_slice(),
        CLEAT_KEY_ARROW_LEFT => b"\x1b[D".as_slice(),
        _ => b"".as_slice(),
    };
    Ok(bytes.to_vec())
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
        cells,
        cursor: TerminalCursor {
            col: (input_count as u16) % cols,
            row: 0,
            visible: true,
            style: TerminalCursorStyle::Block,
            wide_tail: false,
        },
        dirty,
    }
}

fn cursor_to_ffi(cursor: TerminalCursor) -> CleatCursor {
    CleatCursor {
        col: cursor.col,
        row: cursor.row,
        visible: cursor.visible,
        style: match cursor.style {
            TerminalCursorStyle::Bar => 0,
            TerminalCursorStyle::Block => 1,
            TerminalCursorStyle::Underline => 2,
            TerminalCursorStyle::BlockHollow => 3,
        },
        wide_tail: cursor.wide_tail,
    }
}

fn cell_width_tag(width: TerminalCellWidth) -> u32 {
    match width {
        TerminalCellWidth::Narrow => 0,
        TerminalCellWidth::Wide => 1,
        TerminalCellWidth::SpacerTail => 2,
        TerminalCellWidth::SpacerHead => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn mock_provider_dirty_tracks_input_and_snapshot() {
        unsafe {
            let provider = cleat_provider_open(ptr::null());
            let session = cleat_session_create(provider, ptr::null());
            assert_eq!(cleat_session_dirty(session), CleatDirtyState::Full);

            let event = CleatInputEvent { kind: 1, ..CleatInputEvent::default() };
            assert!(cleat_session_send_input(session, &event));
            assert_eq!(cleat_session_dirty(session), CleatDirtyState::Partial);

            let mut snapshot = CleatSnapshot::default();
            assert!(cleat_session_snapshot(session, &mut snapshot));
            assert_eq!(snapshot.dirty, CleatDirtyState::Partial);
            assert_eq!(cleat_session_dirty(session), CleatDirtyState::Clean);

            cleat_session_release_snapshot(session, &mut snapshot);
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
        assert_eq!(
            daemon_input_request(&key_event).expect("key input"),
            Some(http_uds::InputRequest::Key { key: http_uds::KeyRequest::Named { key: http_uds::NamedKey::Enter } })
        );
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
            assert!(cleat_session_write_bytes(session, b"hello\n".as_ptr(), b"hello\n".len()));

            cleat_session_destroy(session);
            cleat_provider_close(provider);
        }
    }
}
