//! Views: several places showing one daemon attachment. A `cleat_session` is
//! the attachment and keeps its default view; each `cleat_view` is another
//! view with its own role, size, history position and render updates. Live
//! views share the attachment's one render stream.
use super::*;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CleatViewDesc {
    pub cols: u16,
    pub rows: u16,
    pub cell_width_px: f32,
    pub cell_height_px: f32,
    /// CLEAT_ROLE_UNKNOWN / CLEAT_ROLE_CONTROLLER request shared driving (the
    /// daemon may grant watcher while another view holds exclusive control);
    /// CLEAT_ROLE_WATCHER watches without voting on the grid size.
    pub role: u32,
}

pub struct CleatView {
    view: DaemonView,
    geometry: TerminalGeometry,
    next_input_sequence: u64,
    wake: Arc<Mutex<WakeCallback>>,
    last_render_update: Option<Box<OwnedRenderUpdate>>,
    backing_error: String,
    image_backings: std::collections::HashMap<usize, OwnedImageBacking>,
    close_reason: String,
}

impl Drop for CleatView {
    fn drop(&mut self) {
        let _ = self.view.connection.close_view(self.view.channel, self.view.view);
    }
}

/// Open another view on a daemon session's attachment. Null for in-process
/// and mock sessions, for an invalid desc, and once the attachment is closed.
/// On a daemon that predates views the view reports CLEAT_SESSION_CLOSED;
/// `cleat_view_close_reason` says why.
/// # Safety
/// `session` must be a live session handle; `desc` must be null or valid.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_open_view(session: *mut CleatSession, desc: *const CleatViewDesc) -> *mut CleatView {
    let Some(session) = (unsafe { session.as_ref() }) else {
        return ptr::null_mut();
    };
    let SessionBackend::Daemon(daemon) = &session.backend else {
        return ptr::null_mut();
    };
    let desc = unsafe { desc.as_ref() }.copied().unwrap_or(CleatViewDesc { cols: 80, rows: 24, ..Default::default() });
    let Ok(role) = channel_role_from_ffi(desc.role) else {
        return ptr::null_mut();
    };
    let geometry = TerminalGeometry::from_cell_size(desc.cols.max(1), desc.rows.max(1), desc.cell_width_px, desc.cell_height_px);
    let (cell_width_px, cell_height_px) = geometry_cell_size_to_backend(geometry);
    let resize = TerminalResizeEvent {
        cols: desc.cols.max(1),
        rows: desc.rows.max(1),
        cell_width_px: cell_width_px as f32,
        cell_height_px: cell_height_px as f32,
    };
    let Some(view) = daemon.view.connection.open_view(daemon.view.channel, resize, role) else {
        return ptr::null_mut();
    };
    Box::into_raw(Box::new(CleatView {
        view: DaemonView::new(Arc::clone(&daemon.view.connection), daemon.view.channel, view, Arc::clone(&daemon.view.slot)),
        geometry,
        next_input_sequence: 1,
        wake: session.wake.clone(),
        last_render_update: None,
        backing_error: String::new(),
        image_backings: Default::default(),
        close_reason: String::new(),
    }))
}

/// Close the session handle's own (default) view, leaving the attachment to
/// the views opened with `cleat_session_open_view`. Afterwards the session's
/// per-view calls (role, size, input, scrolling, render updates) fail and its
/// connection state reports CLEAT_SESSION_CLOSED; attachment calls (end, fixed
/// size, clipboard, transfer) keep working. A daemon that predates views
/// cannot close it and returns false.
/// # Safety
/// `session` must be a live session handle.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_close_default_view(session: *mut CleatSession) -> bool {
    let Some(session) = (unsafe { session.as_ref() }) else {
        return false;
    };
    let SessionBackend::Daemon(daemon) = &session.backend else {
        return false;
    };
    daemon.view.connection.close_view(daemon.view.channel, crate::packet::DEFAULT_VIEW).is_ok()
}

/// Close the view. Its attachment, other views and session handle stay open.
/// # Safety
/// `view` must be null or a view handle that has not been destroyed.
#[no_mangle]
pub unsafe extern "C" fn cleat_view_destroy(view: *mut CleatView) {
    if !view.is_null() {
        drop(unsafe { Box::from_raw(view) });
    }
}

/// Like `cleat_session_connection_state`, for this view. CLEAT_SESSION_CLOSED
/// once the attachment closed or the view cannot be served.
/// # Safety
/// `view` must be null or a live view handle.
#[no_mangle]
pub unsafe extern "C" fn cleat_view_connection_state(view: *const CleatView) -> u32 {
    unsafe { view.as_ref() }.map_or(CLEAT_SESSION_CLOSED, |view| view.view.connection_state())
}

/// Why the view is closed; empty while it is open. Borrowed until the next
/// call or the view's destruction.
/// # Safety
/// `view` and `out` must be valid.
#[no_mangle]
pub unsafe extern "C" fn cleat_view_close_reason(view: *mut CleatView, out: *mut CleatStr) -> bool {
    let (Some(view), Some(out)) = (unsafe { view.as_mut() }, unsafe { out.as_mut() }) else {
        return false;
    };
    view.close_reason = view.view.slot.lock().map_or_else(
        |_| "session channel lock poisoned".to_string(),
        |slot| slot.view_closed(view.view.view).unwrap_or_default().to_string(),
    );
    *out = CleatStr { ptr: view.close_reason.as_ptr(), len: view.close_reason.len() };
    true
}

/// Granted role of this view (CLEAT_ROLE_*), UNKNOWN until the grant arrives.
/// # Safety
/// `view` must be null or a live view handle.
#[no_mangle]
pub unsafe extern "C" fn cleat_view_role(view: *const CleatView) -> u32 {
    unsafe { view.as_ref() }.map_or(CLEAT_ROLE_UNKNOWN, |view| view.view.role())
}

/// Request shared driving, watching, or exclusive control (`take=true`) for
/// this view. Taking control demotes every other driving view, including the
/// other views of this attachment. The grant is asynchronous.
/// # Safety
/// `view` must be null or a live view handle.
#[no_mangle]
pub unsafe extern "C" fn cleat_view_set_role(view: *mut CleatView, role: u32, take: bool) -> bool {
    let Some(view) = (unsafe { view.as_ref() }) else {
        return false;
    };
    match role {
        CLEAT_ROLE_CONTROLLER => view.view.set_role(crate::packet::ChannelRole::Controller, take),
        CLEAT_ROLE_WATCHER => view.view.set_role(crate::packet::ChannelRole::Watcher, take),
        _ => false,
    }
}

/// `cleat_view_set_role(view, CLEAT_ROLE_CONTROLLER, true)`.
/// # Safety
/// `view` must be null or a live view handle.
#[no_mangle]
pub unsafe extern "C" fn cleat_view_take_control(view: *mut CleatView) -> bool {
    unsafe { cleat_view_set_role(view, CLEAT_ROLE_CONTROLLER, true) }
}

/// The view's window in cells. A controller view votes with it on the shared
/// grid size; every view reconciles its window with the grid.
/// # Safety
/// `view` must be null or a live view handle.
#[no_mangle]
pub unsafe extern "C" fn cleat_view_resize(view: *mut CleatView, cols: u16, rows: u16) -> bool {
    unsafe { view.as_ref() }.is_some_and(|view| view.view.resize(cols, rows))
}

/// # Safety
/// `view` must be null or a live view handle; `geometry` must be null or valid.
#[no_mangle]
pub unsafe extern "C" fn cleat_view_update_geometry(view: *mut CleatView, geometry: *const CleatTerminalGeometry) -> bool {
    let (Some(view), Some(geometry)) = (unsafe { view.as_mut() }, unsafe { geometry.as_ref() }) else {
        return false;
    };
    let geometry = TerminalGeometry::from(*geometry);
    if view.geometry == geometry {
        return true;
    }
    if !view.view.set_cell_size(geometry) {
        return false;
    }
    view.geometry = geometry;
    notify_wake(&view.wake);
    true
}

/// # Safety
/// `view` must be null or a live view handle; `event` must be null or valid.
#[no_mangle]
pub unsafe extern "C" fn cleat_view_send_input(view: *mut CleatView, event: *const CleatInputEvent) -> bool {
    unsafe { cleat_view_send_input_ex(view, event, ptr::null_mut()) }
}

/// Text, key presses, pastes and raw bytes from a controller view return that
/// view, and only that view, to live.
/// # Safety
/// `view` must be null or a live view handle; `event` must be null or valid;
/// `out` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn cleat_view_send_input_ex(view: *mut CleatView, event: *const CleatInputEvent, out: *mut CleatInputResult) -> bool {
    !event.is_null() && unsafe { cleat_view_send_input_batch(view, event, 1, out) }
}

/// # Safety
/// `view` must be null or a live view handle. `events` must be null with
/// `event_count == 0` or point to `event_count` readable events; `out` must be
/// null or writable.
#[no_mangle]
pub unsafe extern "C" fn cleat_view_send_input_batch(
    view: *mut CleatView,
    events: *const CleatInputEvent,
    event_count: usize,
    out: *mut CleatInputResult,
) -> bool {
    let Some(view) = (unsafe { view.as_mut() }) else {
        return false;
    };
    if event_count > 0 && events.is_null() {
        return false;
    }
    let events = if event_count > 0 { unsafe { slice::from_raw_parts(events, event_count) } } else { &[] };
    let mut accepted = 0usize;
    for event in events {
        let Some(count) = view.view.send_input(event) else {
            return false;
        };
        accepted = accepted.saturating_add(count);
    }
    let first_sequence = view.next_input_sequence;
    view.next_input_sequence = first_sequence.saturating_add(u64::try_from(accepted).unwrap_or(u64::MAX));
    if let Some(out) = unsafe { out.as_mut() } {
        *out = CleatInputResult { first_sequence, count: accepted };
    }
    true
}

/// # Safety
/// `view` must be null or a live view handle; `bytes` must be null with
/// `size == 0` or point to `size` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn cleat_view_write_bytes(view: *mut CleatView, bytes: *const u8, size: usize) -> bool {
    let Some(view) = (unsafe { view.as_ref() }) else {
        return false;
    };
    if size > 0 && bytes.is_null() {
        return false;
    }
    let bytes = if size > 0 { unsafe { slice::from_raw_parts(bytes, size) } } else { &[] };
    view.view.write_bytes(bytes)
}

/// Known dirty state of this view.
/// # Safety
/// `view` must be null or a live view handle.
#[no_mangle]
pub unsafe extern "C" fn cleat_view_poll(view: *mut CleatView) -> CleatDirtyState {
    unsafe { cleat_view_dirty(view) }
}

/// # Safety
/// `view` must be null or a live view handle.
#[no_mangle]
pub unsafe extern "C" fn cleat_view_dirty(view: *const CleatView) -> CleatDirtyState {
    unsafe { view.as_ref() }.map_or(CleatDirtyState::Full, |view| view.view.dirty().into())
}

/// # Safety
/// `view` must be null or a live view handle; `out` must be writable.
#[no_mangle]
pub unsafe extern "C" fn cleat_view_scrollback_extent(view: *mut CleatView, out: *mut CleatScrollbackExtent) -> bool {
    let (Some(view), Some(out)) = (unsafe { view.as_ref() }, unsafe { out.as_mut() }) else {
        return false;
    };
    let extent = view.view.scrollback_extent();
    *out = CleatScrollbackExtent {
        normal_scrollback_rows: extent.normal_scrollback_rows,
        live_rows: extent.live_rows,
        alternate_screen: extent.alternate_screen,
    };
    true
}

/// # Safety
/// `view` must be null or a live view handle; `out` must be writable.
#[no_mangle]
pub unsafe extern "C" fn cleat_view_scrollbar_state(view: *mut CleatView, out: *mut CleatTerminalScrollbarState) -> bool {
    let (Some(view), Some(out)) = (unsafe { view.as_ref() }, unsafe { out.as_mut() }) else {
        return false;
    };
    *out = scrollbar_to_ffi(view.view.scrollbar_state());
    true
}

/// Move this view's history position. A view in history gets its own render
/// updates; the attachment's other views stay live.
/// # Safety
/// `view` must be null or a live view handle; `command` must be valid; `out`
/// must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn cleat_view_scroll_viewport(
    view: *mut CleatView,
    command: *const CleatViewportCommand,
    out: *mut CleatViewportCommandResult,
) -> bool {
    let (Some(view), Some(command)) = (unsafe { view.as_ref() }, unsafe { command.as_ref() }.copied()) else {
        return false;
    };
    let outcome = match viewport_command_from_ffi(command) {
        Some(command) => view.view.scroll_viewport(command),
        None => ViewportCommandOutcome::Unsupported,
    };
    if let Some(out) = unsafe { out.as_mut() } {
        out.outcome = viewport_command_outcome_to_ffi(outcome);
    }
    true
}

/// The view's next render update, like `cleat_session_render_update`. A new
/// or returning live view first receives the whole grid. Only one update may
/// be live per view.
/// # Safety
/// `view` must be null or a live view handle; `out` must be writable.
#[no_mangle]
pub unsafe extern "C" fn cleat_view_render_update(view: *mut CleatView, out: *mut CleatRenderUpdate) -> bool {
    let (Some(view), Some(out)) = (unsafe { view.as_mut() }, unsafe { out.as_mut() }) else {
        return false;
    };
    if view.last_render_update.is_some() {
        return false;
    }
    let Some(mut update) = view.view.render_update() else {
        return false;
    };
    update.geometry = view.geometry;
    update.scrollbar = view.view.scrollbar_state();
    let owned = OwnedRenderUpdate::from_update(update);
    *out = owned.update;
    view.last_render_update = Some(owned);
    true
}

/// # Safety
/// `view` must be null or a live view handle; `update` must be null or the
/// update `cleat_view_render_update` filled for this view.
#[no_mangle]
pub unsafe extern "C" fn cleat_view_release_render_update(view: *mut CleatView, update: *mut CleatRenderUpdate) {
    if let Some(view) = unsafe { view.as_mut() } {
        view.last_render_update = None;
    }
    if let Some(update) = unsafe { update.as_mut() } {
        *update = CleatRenderUpdate::default();
    }
}

/// Declarations of the outstanding render update, as for sessions.
/// # Safety
/// `view` must be null or a live view handle; `count` must be writable.
#[no_mangle]
pub unsafe extern "C" fn cleat_view_render_update_virtual_placements(
    view: *mut CleatView,
    count: *mut usize,
) -> *const CleatVirtualPlacement {
    let Some(count) = (unsafe { count.as_mut() }) else {
        return ptr::null();
    };
    *count = 0;
    let Some(owned) = (unsafe { view.as_ref() }).and_then(|view| view.last_render_update.as_ref()) else {
        return ptr::null();
    };
    *count = owned.virtual_placements.len();
    if *count == 0 {
        ptr::null()
    } else {
        owned.virtual_placements.as_ptr()
    }
}

/// Borrow the bytes of an image in the view's current frame.
/// # Safety
/// As `cleat_session_with_image_resource_data`, for a live view handle.
#[no_mangle]
pub unsafe extern "C" fn cleat_view_with_image_resource_data(
    view: *mut CleatView,
    image_id: u32,
    generation: u64,
    callback: CleatImageResourceDataFn,
    user_data: *mut c_void,
) -> bool {
    let (Some(view), Some(callback)) = (unsafe { view.as_ref() }, callback) else {
        return false;
    };
    view.view.with_image_resource_data(image_id, generation, |bytes| unsafe { callback(user_data, bytes.as_ptr(), bytes.len()) })
}

/// As `cleat_session_image_resource_backing`, for the view's current frame.
/// # Safety
/// `view` must be a live view handle and `out` writable. Release each success
/// once on this view.
#[no_mangle]
pub unsafe extern "C" fn cleat_view_image_resource_backing(
    view: *mut CleatView,
    image_id: u32,
    generation: u64,
    kind: u32,
    out: *mut CleatImageBacking,
) -> bool {
    let (Some(view), Some(out)) = (unsafe { view.as_mut() }, unsafe { out.as_mut() }) else {
        return false;
    };
    view.backing_error.clear();
    match view.view.image_resource_backing(image_id, generation, kind) {
        Ok((value, owned)) => {
            view.image_backings.insert(value.name as usize, owned);
            *out = value;
            true
        }
        Err(error) => {
            view.backing_error = error;
            false
        }
    }
}

/// # Safety
/// `view` must be a live view handle; `backing` must be an unreleased result
/// from this view (or a cleared result).
#[no_mangle]
pub unsafe extern "C" fn cleat_view_release_image_resource_backing(view: *mut CleatView, backing: *mut CleatImageBacking) {
    let (Some(view), Some(backing)) = (unsafe { view.as_mut() }, unsafe { backing.as_mut() }) else {
        return;
    };
    if !backing.name.is_null() {
        let removed = view.image_backings.remove(&(backing.name as usize));
        debug_assert!(removed.is_some(), "backing must be released once on its originating view");
    }
    *backing = CleatImageBacking::default();
}

/// # Safety
/// `view` and `out` must be valid. Borrowed until the next backing request or
/// the view's destruction.
#[no_mangle]
pub unsafe extern "C" fn cleat_view_image_resource_backing_error(view: *const CleatView, out: *mut CleatStr) -> bool {
    let (Some(view), Some(out)) = (unsafe { view.as_ref() }, unsafe { out.as_mut() }) else {
        return false;
    };
    *out = CleatStr { ptr: view.backing_error.as_ptr(), len: view.backing_error.len() };
    true
}

/// The view's role state, view status and current frame's links as UTF-8
/// JSON, like `cleat_session_attachment_state_json`.
/// # Safety
/// `view` and `out_len` must be valid; a non-null buffer must hold `capacity` bytes.
#[no_mangle]
pub unsafe extern "C" fn cleat_view_state_json(view: *const CleatView, buffer: *mut u8, capacity: usize, out_len: *mut usize) -> bool {
    let (Some(view), Some(out_len)) = (unsafe { view.as_ref() }, unsafe { out_len.as_mut() }) else {
        return false;
    };
    let Some(bytes) = view.view.state_json() else {
        return false;
    };
    *out_len = bytes.len();
    if buffer.is_null() {
        return true;
    }
    if capacity < bytes.len() {
        return false;
    }
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), buffer, bytes.len());
    }
    true
}
