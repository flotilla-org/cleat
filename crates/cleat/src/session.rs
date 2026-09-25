#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    io::{self, Read, Write},
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

use http::StatusCode;

use crate::{
    host::actor::{RawOutputRecovery, RawOutputReplay, RawOutputTap, SessionActor, SessionMouseEvent, SessionWheelEvent},
    http_uds,
    image_delivery::{ImageReceiver, ImageTransfer, RenderBundle},
    packet::{
        Ack, ActivityEvent, ActivitySession, ActivitySnapshot, ChannelRole, CloseChannel, ControlError, ControlHello, ControllerHolder,
        DirectoryDelta, DirectoryEntry, DirectorySnapshot, Input, OpenChannel, PacketFrame, RenderPacket, Resize, RoleDenialReason,
        RoleRequest, RoleState, ScreenActivity, CHANNEL_CONTROL, MSG_CONTROL_ACTIVITY_EVENT, MSG_CONTROL_ACTIVITY_SNAPSHOT,
        MSG_CONTROL_CLOSE_CHANNEL, MSG_CONTROL_DIRECTORY_DELTA, MSG_CONTROL_DIRECTORY_SNAPSHOT, MSG_CONTROL_ERROR, MSG_CONTROL_HELLO,
        MSG_CONTROL_OPEN_CHANNEL, MSG_SESSION_ACK, MSG_SESSION_INPUT, MSG_SESSION_RENDER, MSG_SESSION_RESIZE, MSG_SESSION_ROLE,
        MSG_SESSION_VIEWPORT,
    },
    platform::{
        daemon::{is_session_daemon_alive, spawn_daemon_process},
        ipc::{
            bind_session_listener, connect_session_stream, set_listener_nonblocking, set_stream_nonblocking, set_stream_write_timeout,
            shutdown_stream, try_connect_session_stream, validate_session_socket_path, SessionStream,
        },
        terminal::{attach_signal_exit_requested, current_terminal_size, stdout_is_tty, AttachSignalHandlers, ForegroundTerminal},
    },
    protocol::{AttachmentIdentity, Frame, SeatState},
    provider::{DirtyState, TerminalInputEvent, TerminalMouseButton, TerminalMouseEventKind, TerminalRenderUpdate},
    runtime::{AmbientSessionCoordinates, RuntimeLayout, SessionMetadata, TerminalSize},
    vt::{self, ScreenGrid, VtEngine, VtEngineKind},
};

const DETACH_CLEANUP_SEQUENCE: &[u8] =
    b"\x1b_Ga=d,d=A,q=2;\x1b\\\x1b[?2026l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1016l\x1b[?2004l\x1b[?1004l\x1b[<u\x1b[r\x1b[0m\x1b[?25h\x1b[2J\x1b[H\x1b[?1049l";
const REATTACH_CLEAR_SEQUENCE: &[u8] = b"\x1b[2J\x1b[H";
const MAX_PENDING_CLIENT_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
// Leave room for control messages without prefetching megabytes of images.
const IMAGE_OUTPUT_HIGH_WATER: usize = 256 * 1024;
const PACKET_OUTPUT_WRITE_BUDGET: usize = 256 * 1024;
const PACKET_OUTPUT_TIME_BUDGET: Duration = Duration::from_millis(1);
const SESSION_DAEMON_SERVICING_TICK: Duration = Duration::from_millis(10);
const SESSION_DAEMON_IDLE_LINGER: Duration = Duration::from_secs(120);
const SESSION_HTTP_HANDSHAKE_DEADLINE: Duration = Duration::from_millis(250);
const SESSION_HTTP_RESPONSE_WRITE_DEADLINE: Duration = Duration::from_millis(250);
const TERMINATE_SIGNAL: i32 = 15;
const SESSION_DAEMON_REGISTRATION_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const SCREEN_STABLE_CHANGED_CELL_TOLERANCE: usize = 16;

#[derive(Debug)]
pub struct ForegroundAttach {
    transport: ForegroundTransport,
}

#[derive(Debug)]
enum ForegroundTransport {
    Legacy(Arc<Mutex<SessionStream>>),
    Packet(Box<PacketForegroundAttach>),
}

#[derive(Debug)]
struct PacketForegroundAttach {
    session_name: String,
    stream: Arc<Mutex<SessionStream>>,
    channel: u32,
    initial_update: TerminalRenderUpdate,
    images: ImageReceiver,
    initial_role: RoleState,
}

#[derive(Debug, Clone, Default)]
pub struct SessionStartOptions {
    pub record: bool,
    pub initial_size: TerminalSize,
    pub colors: vt::TerminalColors,
    pub tags: Vec<String>,
    pub environment: Vec<(String, String)>,
}

impl ForegroundAttach {
    pub fn relay_stdio(self) -> Result<(), String> {
        let signal_handlers = AttachSignalHandlers::install()?;
        self.relay_stdio_with_handlers(signal_handlers)
    }

    /// Relay with handlers the caller installed *before* the attach
    /// handshake. The daemon writes the foreground marker at attach grant;
    /// a signal delivered between that grant and the relay starting must
    /// already be caught, or the process dies with default disposition
    /// (observed as a test race once the daemon got fast enough).
    pub fn relay_stdio_with_handlers(self, signal_handlers: AttachSignalHandlers) -> Result<(), String> {
        match self.transport {
            ForegroundTransport::Legacy(stream) => relay_legacy_stdio(stream, signal_handlers),
            ForegroundTransport::Packet(packet) => relay_packet_stdio(*packet, signal_handlers),
        }
    }
}

fn relay_legacy_stdio(stream: Arc<Mutex<SessionStream>>, signal_handlers: AttachSignalHandlers) -> Result<(), String> {
    let _signal_handlers = signal_handlers;
    let mut cleanup = AttachCleanupGuard::stdout();
    let mut terminal = ForegroundTerminal::enter()?;
    let read_handle = {
        let stream = stream.lock().map_err(|_| "attach stream lock poisoned".to_string())?;
        stream.try_clone().map_err(|err| format!("clone attach stream: {err}"))?
    };
    let mut read_stream = read_handle;
    let alive = Arc::new(AtomicBool::new(true));
    let alive_out = Arc::clone(&alive);
    let relay_out = thread::spawn(move || -> Result<(), String> {
        let mut stdout = std::io::stdout().lock();
        let mut watcher_state = None;
        loop {
            match Frame::read(&mut read_stream) {
                Ok(Frame::Output(bytes)) => {
                    write_attach_output(&mut stdout, &bytes, watcher_state.as_ref())?;
                    stdout.flush().map_err(|err| format!("flush stdout: {err}"))?;
                }
                Ok(Frame::SeatState(state)) => {
                    update_watcher_chrome(&mut stdout, &mut watcher_state, state)?;
                    stdout.flush().map_err(|err| format!("flush stdout: {err}"))?;
                }
                Ok(_) => {}
                Err(err) => {
                    alive_out.store(false, Ordering::SeqCst);
                    if is_graceful_socket_shutdown(&err) {
                        return Ok(());
                    }
                    return Err(format!("read attach frame: {err}"));
                }
            }
        }
    });

    let write_stream = Arc::clone(&stream);
    let alive_resize = Arc::clone(&alive);
    let resize_loop = thread::spawn(move || -> Result<(), String> {
        let mut last = current_terminal_size();
        while alive_resize.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(100));
            let next = current_terminal_size();
            if next != last {
                let mut stream = write_stream.lock().map_err(|_| "attach stream lock poisoned".to_string())?;
                Frame::Resize { cols: next.0, rows: next.1 }.write(&mut *stream).map_err(|err| format!("write resize frame: {err}"))?;
                last = next;
            }
        }
        Ok(())
    });

    let mut buf = [0u8; 4096];
    let stdin_result = loop {
        if !alive.load(Ordering::SeqCst) || attach_signal_exit_requested() {
            break Ok(());
        }
        match terminal.read_input(Duration::from_millis(100), &mut buf) {
            Ok(None) => continue,
            Ok(Some(0)) => break Ok(()),
            Ok(Some(n)) => {
                let mut stream = stream.lock().map_err(|_| "attach stream lock poisoned".to_string())?;
                if let Err(err) = Frame::Input(buf[..n].to_vec()).write(&mut *stream) {
                    if is_graceful_socket_shutdown(&err) {
                        break Ok(());
                    }
                    break Err(format!("write input frame: {err}"));
                }
            }
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::UnexpectedEof
                        | std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                ) =>
            {
                break Ok(())
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => break Err(format!("read stdin: {err}")),
        }
    };

    let signal_exit = attach_signal_exit_requested();
    alive.store(false, Ordering::SeqCst);
    if let Ok(stream) = stream.lock() {
        shutdown_stream(&stream);
    }
    let out_result = relay_out.join().map_err(|_| "stdout relay thread panicked".to_string())?;
    let resize_result = resize_loop.join().map_err(|_| "resize thread panicked".to_string())?;
    cleanup.emit()?;
    if signal_exit {
        return Ok(());
    }
    stdin_result?;
    out_result?;
    resize_result
}

struct AttachChrome {
    session_name: String,
    nested_in: Option<String>,
    renderer: PacketTerminalRenderer,
    panning: bool,
    visible: bool,
    hidden: bool,
    role: RoleState,
    view: crate::provider::ViewState,
    hint: Option<(String, Instant)>,
}

impl AttachChrome {
    fn paint(&mut self, writer: &mut impl Write, update: Option<&TerminalRenderUpdate>) -> Result<(), String> {
        self.paint_at_size(writer, update, current_terminal_size())
    }

    fn paint_at_size(&mut self, writer: &mut impl Write, update: Option<&TerminalRenderUpdate>, size: (u16, u16)) -> Result<(), String> {
        let grid = update.map(|u| (u.cols, u.rows)).unwrap_or((self.renderer.cols, self.renderer.rows));
        if !self.hidden && (grid.0 > size.0 || grid.1 > size.1) {
            self.visible = true;
        }
        self.renderer.set_viewport(self.content_size_for(size));
        if self.renderer.bounds == self.hidden {
            self.renderer.bounds = !self.hidden;
            self.renderer.needs_full_repaint = true;
        }
        if let Some(update) = update {
            self.renderer.apply_and_render(writer, update)?;
        } else {
            self.renderer.repaint(writer)?;
        }
        self.render_at_size(writer, size)
    }

    fn content_size(&self) -> (u16, u16) {
        self.content_size_for(current_terminal_size())
    }

    fn content_size_for(&self, (cols, rows): (u16, u16)) -> (u16, u16) {
        (cols.max(1), rows.saturating_sub(u16::from(self.visible)).max(1))
    }

    fn translate_mouse(
        &self,
        mut mouse: crate::provider::TerminalMouseEvent,
        terminal_rows: u16,
    ) -> Option<crate::provider::TerminalMouseEvent> {
        let strip_row = self.visible || (self.hint.is_some() && !self.hidden);
        let release = mouse.kind == TerminalMouseEventKind::Release;
        if strip_row && mouse.cell_row == terminal_rows.saturating_sub(1) && !release {
            return None;
        }
        let (col, row) = if release {
            (
                self.renderer.geometry.x.saturating_add(mouse.cell_col).min(self.renderer.cols.saturating_sub(1)),
                self.renderer.geometry.y.saturating_add(mouse.cell_row).min(self.renderer.rows.saturating_sub(1)),
            )
        } else {
            self.renderer.geometry.to_grid(mouse.cell_col, mouse.cell_row)?
        };
        let (w, h) = self.renderer.mouse_cell_size;
        // Decoder coordinates are in cell units, retaining the sub-cell offset.
        mouse.x_px = (f32::from(col) + mouse.x_px.fract()) * f32::from(w);
        mouse.y_px = (f32::from(row) + mouse.y_px.fract()) * f32::from(h);
        mouse.cell_col = col;
        mouse.cell_row = row;
        Some(mouse)
    }

    fn render_at_size(&self, writer: &mut impl Write, (cols, rows): (u16, u16)) -> Result<(), String> {
        let message = if self.hidden {
            String::new()
        } else if let Some((hint, _)) = &self.hint {
            hint.clone()
        } else if self.visible {
            let drivers = self.role.participants.iter().filter(|p| p.role == ChannelRole::Controller).count();
            let watchers = self.role.participants.len() - drivers;
            let role = if self.role.role == ChannelRole::Controller { "driving" } else { "watching" };
            let exclusive = self.role.exclusive.as_ref().map(|p| format!(" | exclusive: {}", p.name)).unwrap_or_default();
            let view = match self.view.status {
                crate::provider::ViewStatus::Live => "live",
                crate::provider::ViewStatus::History => "history",
                crate::provider::ViewStatus::Stale => "stale",
                crate::provider::ViewStatus::Unavailable => "unavailable",
            };
            let size = self.role.fixed_size.as_ref().map(|size| format!(" | fixed {}x{}", size.cols, size.rows)).unwrap_or_default();
            let nesting = self.nested_in.as_ref().map(|source| format!("nested in {source} | ")).unwrap_or_default();
            format!(
                "cleat {} | {nesting}{}{}{role} | {drivers} drivers, {watchers} watchers | {view}{exclusive}{size}",
                self.session_name,
                self.renderer.geometry.description(),
                if self.panning { "pan (Esc exits) | " } else { "" }
            )
        } else {
            String::new()
        };
        let mut clipped = String::new();
        let mut width = 0;
        for ch in message.chars().filter(|ch| !ch.is_control()) {
            // Conservative width keeps non-ASCII labels from wrapping the strip.
            let n = if ch.is_ascii() { 1 } else { 2 };
            if width + n > usize::from(cols.saturating_sub(2)) {
                break;
            }
            clipped.push(ch);
            width += n;
        }
        writer.write_all(b"\x1b7").map_err(|e| e.to_string())?;
        if self.visible && rows > 1 {
            write!(writer, "\x1b[1;{}r", rows - 1).map_err(|e| e.to_string())?;
        } else {
            writer.write_all(b"\x1b[r").map_err(|e| e.to_string())?;
        }
        if self.visible || (self.hint.is_some() && !self.hidden) {
            write!(writer, "\x1b[{rows};1H\x1b[2K\x1b[7m {clipped}\x1b[0m").map_err(|e| e.to_string())?;
        }
        writer.write_all(b"\x1b8").map_err(|e| e.to_string())?;
        Ok(())
    }
}

fn relay_packet_stdio(packet: PacketForegroundAttach, signal_handlers: AttachSignalHandlers) -> Result<(), String> {
    use crate::attach_input::{Action, Command, InputDecoder};
    let _signal_handlers = signal_handlers;
    let mut cleanup = AttachCleanupGuard::stdout();
    let keyboard = cleanup.enabled.then(|| Arc::new(Mutex::new(crate::attach_keyboard::KeyboardMode::default())));
    cleanup.keyboard = keyboard.clone();
    let mut terminal = ForegroundTerminal::enter()?;
    let read_handle = packet.stream.lock().map_err(|_| "packet stream poisoned")?.try_clone().map_err(|e| e.to_string())?;
    let alive = Arc::new(AtomicBool::new(true));
    let controller = Arc::new(AtomicBool::new(packet.initial_role.role == ChannelRole::Controller));
    let mut renderer = PacketTerminalRenderer::new(packet.initial_update.cols, packet.initial_update.rows);
    renderer.keyboard = keyboard.clone();
    let nested_in =
        crate::runtime::ambient_session_coordinates()?.map(|source| format!("{}/{}", source.daemon_name(), source.session_id()));
    let chrome = Arc::new(Mutex::new(AttachChrome {
        session_name: packet.session_name,
        renderer,
        panning: false,
        visible: nested_in.is_some() || packet.initial_role.participants.len() > 1,
        nested_in,
        hidden: false,
        role: packet.initial_role,
        view: Default::default(),
        hint: None,
    }));
    let alive_out = Arc::clone(&alive);
    let controller_out = Arc::clone(&controller);
    let chrome_out = Arc::clone(&chrome);
    let write_stream = Arc::clone(&packet.stream);
    let channel = packet.channel;
    let initial_update = packet.initial_update;
    let mut images = packet.images;
    let relay_out = thread::spawn(move || -> Result<(), String> {
        let mut read_stream = read_handle;
        {
            let mut stdout = std::io::stdout().lock();
            let mut chrome = chrome_out.lock().map_err(|_| "chrome poisoned")?;
            chrome.renderer.images.set_assets(images.commit(&initial_update.image_resources)?);
            chrome.paint(&mut stdout, Some(&initial_update))?;
            stdout.write_all(b"\x1b[?2004h\x1b[?1004h").map_err(|e| e.to_string())?;
            if chrome.renderer.keyboard.is_some() {
                stdout.write_all(crate::attach_keyboard::QUERY).map_err(|e| e.to_string())?;
                stdout.write_all(b"\x1b[?1016l\x1b[?1006h").map_err(|e| e.to_string())?;
                stdout.write_all(crate::attach_mouse::QUERY).map_err(|e| e.to_string())?;
            }
            stdout.flush().map_err(|e| e.to_string())?;
        }
        write_packet_frame(
            &write_stream,
            PacketFrame::new(channel, MSG_SESSION_ACK, &Ack { generation: initial_update.render_generation }),
        )?;
        loop {
            let frame = match PacketFrame::read(&mut read_stream) {
                Ok(frame) => frame,
                Err(err) => {
                    alive_out.store(false, Ordering::SeqCst);
                    return if is_graceful_socket_shutdown(&err) { Ok(()) } else { Err(err.to_string()) };
                }
            };
            let mut stdout = std::io::stdout().lock();
            match (frame.channel, frame.msg_type) {
                (id, crate::packet::MSG_SESSION_IMAGE_FILE) if id == channel => {
                    let file = frame.decode::<crate::packet::ImageFile>().map_err(|e| e.to_string())?;
                    let acquired = images.file(&file);
                    write_packet_frame(
                        &write_stream,
                        PacketFrame::new(channel, crate::packet::MSG_SESSION_IMAGE_FILE_RESULT, &crate::packet::ImageFileResult {
                            image_id: file.image_id,
                            generation: file.generation,
                            acquired,
                        }),
                    )?;
                }
                (id, crate::packet::MSG_SESSION_IMAGE) if id == channel => {
                    images.chunk(frame.decode().map_err(|e| e.to_string())?)?;
                }
                (id, MSG_SESSION_RENDER) if id == channel => {
                    let mut packet = frame.decode::<RenderPacket>().map_err(|e| e.to_string())?;
                    if packet.view.status == crate::provider::ViewStatus::History {
                        packet.update.terminal_modes.mouse_tracking_mode = vt::MouseTrackingMode::None;
                    }
                    let mut chrome = chrome_out.lock().map_err(|_| "chrome poisoned")?;
                    if let Some(notice) = &packet.view.notice {
                        chrome.hint = Some((notice.clone(), Instant::now() + Duration::from_secs(3)));
                    }
                    chrome.view = packet.view;
                    chrome.renderer.images.set_assets(images.commit(&packet.update.image_resources)?);
                    chrome.paint(&mut stdout, Some(&packet.update))?;
                    write_packet_frame(
                        &write_stream,
                        PacketFrame::new(channel, MSG_SESSION_ACK, &Ack { generation: packet.update.render_generation }),
                    )?;
                }
                (id, MSG_SESSION_ROLE) if id == channel => {
                    let state = frame.decode::<RoleState>().map_err(|e| e.to_string())?;
                    controller_out.store(state.role == ChannelRole::Controller, Ordering::SeqCst);
                    let mut chrome = chrome_out.lock().map_err(|_| "chrome poisoned")?;
                    if state.participants.len() > 1 && !chrome.hidden {
                        chrome.visible = true;
                    }
                    chrome.role = state;
                    chrome.paint(&mut stdout, None)?;
                }
                (id, crate::packet::MSG_SESSION_VIEW_STATE) if id == channel => {
                    let view = frame.decode::<crate::provider::ViewState>().map_err(|e| e.to_string())?;
                    let mut chrome = chrome_out.lock().map_err(|_| "chrome poisoned")?;
                    if let Some(notice) = &view.notice {
                        chrome.hint = Some((notice.clone(), Instant::now() + Duration::from_secs(3)));
                    }
                    chrome.view = view;
                    chrome.paint(&mut stdout, None)?;
                }
                (CHANNEL_CONTROL, MSG_CONTROL_ERROR) => {
                    let error = frame.decode::<ControlError>().map_err(|e| e.to_string())?;
                    if error.channel == channel {
                        alive_out.store(false, Ordering::SeqCst);
                        return Ok(());
                    }
                }
                _ => {}
            }
            stdout.flush().map_err(|e| e.to_string())?;
        }
    });
    let resize_stream = Arc::clone(&packet.stream);
    let alive_resize = Arc::clone(&alive);
    let chrome_resize = Arc::clone(&chrome);
    let resize_loop = thread::spawn(move || -> Result<(), String> {
        let mut last = None;
        let mut cell_query = Instant::now();
        while alive_resize.load(Ordering::SeqCst) {
            let next = chrome_resize.lock().map_err(|_| "chrome poisoned")?.content_size();
            if last != Some(next) {
                {
                    let mut stdout = std::io::stdout().lock();
                    chrome_resize.lock().map_err(|_| "chrome poisoned")?.paint(&mut stdout, None)?;
                    stdout.flush().map_err(|e| e.to_string())?;
                }
                write_packet_frame(&resize_stream, PacketFrame::new(channel, MSG_SESSION_RESIZE, &Resize { cols: next.0, rows: next.1 }))?;
                write_packet_frame(
                    &resize_stream,
                    PacketFrame::new(channel, MSG_SESSION_VIEWPORT, &crate::packet::Viewport {
                        command: crate::provider::ViewportCommand::DeltaRows(0),
                    }),
                )?;
                last = Some(next);
            }
            if cell_query.elapsed() >= Duration::from_secs(1) && stdout_is_tty()? {
                let mut stdout = std::io::stdout().lock();
                stdout.write_all(b"\x1b[16t").map_err(|e| e.to_string())?;
                stdout.flush().map_err(|e| e.to_string())?;
                cell_query = Instant::now();
            }
            thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    });
    let prefix = std::env::var("CLEAT_COMMAND_PREFIX")
        .ok()
        .and_then(|s| {
            let bytes = s.as_bytes();
            (bytes.len() == 2 && bytes[0] == b'^').then(|| bytes[1].to_ascii_uppercase() & 0x1f)
        })
        .filter(|byte| *byte != 0x1b && *byte != 0)
        .unwrap_or(0x1d);
    let mut decoder = InputDecoder::new(prefix);
    decoder.set_pixel_origin(crate::attach_mouse::pixel_origin(
        &std::env::var("TERM").unwrap_or_default(),
        &std::env::var("TERM_PROGRAM").unwrap_or_default(),
    ));
    let mut buf = [0u8; 4096];
    let stdin_result = 'input: loop {
        if !alive.load(Ordering::SeqCst) || attach_signal_exit_requested() {
            break Ok(());
        }
        let expired = {
            let mut state = chrome.lock().map_err(|_| "chrome poisoned")?;
            if state.hint.as_ref().is_some_and(|(_, until)| Instant::now() >= *until) {
                state.hint = None;
                true
            } else {
                false
            }
        };
        if expired {
            write_packet_frame(
                &packet.stream,
                PacketFrame::new(channel, MSG_SESSION_VIEWPORT, &crate::packet::Viewport {
                    command: crate::provider::ViewportCommand::DeltaRows(0),
                }),
            )?;
        }
        {
            let mut stdout = std::io::stdout().lock();
            let mut chrome = chrome.lock().map_err(|_| "chrome poisoned")?;
            chrome.renderer.images.expire(&mut stdout)?;
            stdout.flush().map_err(|e| e.to_string())?;
        }
        decoder.set_driving(controller.load(Ordering::SeqCst));
        let actions = match terminal.read_input(Duration::from_millis(100), &mut buf) {
            Ok(None) => decoder.idle(),
            Ok(Some(0)) => break Ok(()),
            Ok(Some(n)) => decoder.feed(&buf[..n]),
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) if matches!(err.kind(), std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::BrokenPipe) => break Ok(()),
            Err(err) => break Err(err.to_string()),
        };
        for action in actions {
            let mut event = None;
            let mut command = None;
            let mut hint = None;
            match action {
                Action::GraphicsReply(bytes) => {
                    let mut stdout = std::io::stdout().lock();
                    let mut chrome = chrome.lock().map_err(|_| "chrome poisoned")?;
                    chrome.renderer.images.reply(&mut stdout, &bytes)?;
                    chrome.renderer.refresh_images(&mut stdout)?;
                    stdout.flush().map_err(|e| e.to_string())?;
                }
                Action::KeyboardFlags(_) => {
                    if let Some(keyboard) = &keyboard {
                        let mut stdout = std::io::stdout().lock();
                        if keyboard.lock().map_err(|_| "keyboard mode poisoned")?.enable(&mut stdout).map_err(|e| e.to_string())? {
                            decoder.set_keyboard_flags(crate::attach_keyboard::FLAGS);
                        }
                        stdout.flush().map_err(|e| e.to_string())?;
                    }
                }
                Action::EnablePixelMouse => {
                    let mut stdout = std::io::stdout().lock();
                    stdout.write_all(b"\x1b[?1016h\x1b[?1016$p").map_err(|e| e.to_string())?;
                    stdout.flush().map_err(|e| e.to_string())?;
                }
                Action::MouseCellSize(w, h) => {
                    let mut chrome = chrome.lock().map_err(|_| "chrome poisoned")?;
                    chrome.renderer.mouse_cell_size = (w, h);
                    let (cols, rows) = chrome.content_size();
                    event = Some(TerminalInputEvent::Resize(crate::provider::TerminalResizeEvent {
                        cols,
                        rows,
                        cell_width_px: f32::from(w),
                        cell_height_px: f32::from(h),
                    }));
                }
                Action::Key(key) => event = Some(TerminalInputEvent::Key(key)),
                Action::Raw(bytes) => event = Some(TerminalInputEvent::RawBytes(bytes)),
                Action::Paste(text) => event = Some(TerminalInputEvent::Paste(crate::provider::TerminalPasteEvent { text })),
                Action::Focus(focused) => event = Some(TerminalInputEvent::Focus(crate::provider::TerminalFocusEvent { focused })),
                Action::Mouse(mouse) => {
                    let chrome = chrome.lock().map_err(|_| "chrome poisoned")?;
                    event = chrome.translate_mouse(mouse, current_terminal_size().1).map(TerminalInputEvent::Mouse);
                }
                Action::Command(action @ (Command::Pan(_, _) | Command::RevealCursor)) => {
                    let mut stdout = std::io::stdout().lock();
                    let mut chrome = chrome.lock().map_err(|_| "chrome poisoned")?;
                    let size = chrome.content_size();
                    chrome.renderer.set_viewport(size);
                    chrome.panning = decoder.is_panning();
                    if let Command::Pan(x, y) = action {
                        chrome.renderer.geometry.pan(x, y);
                    } else if chrome.view.status == crate::provider::ViewStatus::Live {
                        let cursor = chrome.renderer.last_update.as_ref().map(|u| (u.cursor.col, u.cursor.row));
                        if let Some((col, row)) = cursor {
                            chrome.renderer.geometry.reveal(col, row);
                        }
                    }
                    chrome.renderer.needs_full_repaint = true;
                    chrome.hint = None;
                    chrome.paint(&mut stdout, None)?;
                    stdout.flush().map_err(|e| e.to_string())?;
                }
                Action::Hint(text) => hint = Some(text.to_string()),
                Action::Command(Command::Detach) => break 'input Ok(()),
                Action::Command(Command::AutoSize) => {
                    if let Err(err) = write_packet_frame(
                        &packet.stream,
                        PacketFrame::new(channel, crate::packet::MSG_SESSION_SIZE_POLICY, &None::<Resize>),
                    ) {
                        break 'input Err(err);
                    }
                    hint = Some(String::new());
                }
                Action::Command(Command::Chrome) => {
                    let mut chrome = chrome.lock().map_err(|_| "chrome poisoned")?;
                    chrome.visible = !chrome.visible;
                    chrome.hidden = !chrome.visible;
                    command = Some(crate::provider::ViewportCommand::DeltaRows(0));
                }
                Action::Command(action @ (Command::Watch | Command::Drive | Command::Exclusive)) => {
                    let role = if action == Command::Watch { ChannelRole::Watcher } else { ChannelRole::Controller };
                    if let Err(err) = write_packet_frame(
                        &packet.stream,
                        PacketFrame::new(channel, MSG_SESSION_ROLE, &RoleRequest { role, take: action == Command::Exclusive }),
                    ) {
                        break 'input Err(err);
                    }
                    hint = Some(String::new());
                }
                Action::Command(action) => {
                    command = Some(match action {
                        Command::Top => crate::provider::ViewportCommand::Top,
                        Command::Bottom => crate::provider::ViewportCommand::Bottom,
                        Command::Up => crate::provider::ViewportCommand::DeltaRows(-10),
                        _ => crate::provider::ViewportCommand::DeltaRows(10),
                    });
                }
            }
            if let Some(event) = event {
                let local = matches!(event, TerminalInputEvent::Focus(_) | TerminalInputEvent::Mouse(_) | TerminalInputEvent::Resize(_));
                if controller.load(Ordering::SeqCst) || local {
                    if let Err(err) = write_packet_frame(&packet.stream, PacketFrame::new(channel, MSG_SESSION_INPUT, &Input { event })) {
                        break 'input Err(err);
                    }
                    hint = Some(String::new());
                } else {
                    hint = Some("Watching; prefix then g to start driving".into());
                }
            }
            if let Some(command) = command {
                if let Err(err) = write_packet_frame(
                    &packet.stream,
                    PacketFrame::new(channel, MSG_SESSION_VIEWPORT, &crate::packet::Viewport { command }),
                ) {
                    break 'input Err(err);
                }
                hint = Some(String::new());
            }
            if let Some(hint) = hint {
                let mut stdout = std::io::stdout().lock();
                let mut chrome = chrome.lock().map_err(|_| "chrome poisoned")?;
                chrome.panning = decoder.is_panning();
                let restore = hint.is_empty() && chrome.hint.is_some() && !chrome.visible;
                chrome.hint = if hint.is_empty() {
                    None
                } else {
                    Some((hint.clone(), Instant::now() + Duration::from_secs(if hint.starts_with("cleat:") { 3600 } else { 3 })))
                };
                chrome.paint(&mut stdout, None)?;
                stdout.flush().map_err(|e| e.to_string())?;
                drop(chrome);
                drop(stdout);
                if restore {
                    write_packet_frame(
                        &packet.stream,
                        PacketFrame::new(channel, MSG_SESSION_VIEWPORT, &crate::packet::Viewport {
                            command: crate::provider::ViewportCommand::DeltaRows(0),
                        }),
                    )?;
                }
            }
        }
    };
    let signal_exit = attach_signal_exit_requested();
    alive.store(false, Ordering::SeqCst);
    if let Ok(stream) = packet.stream.lock() {
        shutdown_stream(&stream);
    }
    let out_result = relay_out.join().map_err(|_| "packet stdout relay panicked")?;
    let resize_result = resize_loop.join().map_err(|_| "packet resize relay panicked")?;
    cleanup.emit()?;
    if signal_exit {
        return Ok(());
    }
    stdin_result?;
    out_result?;
    resize_result
}

fn write_packet_frame(stream: &Arc<Mutex<SessionStream>>, frame: std::io::Result<PacketFrame>) -> Result<(), String> {
    let frame = frame.map_err(|err| format!("encode packet attach frame: {err}"))?;
    let mut stream = stream.lock().map_err(|_| "packet attach stream lock poisoned".to_string())?;
    frame.write(&mut *stream).map_err(|err| format!("write packet attach frame: {err}"))
}

#[derive(Debug)]
struct PacketTerminalRenderer {
    keyboard: Option<Arc<Mutex<crate::attach_keyboard::KeyboardMode>>>,
    mouse_cell_size: (u16, u16),
    images: crate::kitty_output::KittyOutput,
    geometry: crate::attachment_view::AttachmentView,
    bounds: bool,
    last_update: Option<TerminalRenderUpdate>,
    cols: u16,
    rows: u16,
    cells: Vec<Vec<crate::provider::TerminalRenderCell>>,
    terminal_modes: vt::TerminalModeState,
    needs_full_repaint: bool,
    viewport: Option<(u16, u16)>,
}

impl PacketTerminalRenderer {
    fn new(cols: u16, rows: u16) -> Self {
        Self {
            keyboard: None,
            mouse_cell_size: (1, 1),
            images: Default::default(),
            geometry: crate::attachment_view::AttachmentView::new((cols, rows), (cols, rows)),
            bounds: false,
            last_update: None,
            cols,
            rows,
            cells: vec![vec![crate::provider::TerminalRenderCell::default(); cols as usize]; rows as usize],
            terminal_modes: vt::TerminalModeState::default(),
            needs_full_repaint: true,
            viewport: None,
        }
    }

    fn repaint(&mut self, writer: &mut impl Write) -> Result<(), String> {
        if let Some(update) = self.last_update.clone() {
            self.needs_full_repaint = true;
            self.apply_and_render(writer, &update)?;
        }
        Ok(())
    }

    fn refresh_images(&mut self, writer: &mut impl Write) -> Result<(), String> {
        // last_update has metadata but no cell operations. Preserve the cursor
        // and synchronize the placement swap without invalidating cached rows.
        if let Some(update) = self.last_update.clone() {
            self.apply_and_render(writer, &update)?;
        }
        Ok(())
    }

    fn set_viewport(&mut self, size: (u16, u16)) {
        self.geometry.resize((self.cols, self.rows), size);
        if self.viewport != Some(size) {
            self.viewport = Some(size);
            self.needs_full_repaint = true;
        }
    }

    fn apply_and_render(&mut self, writer: &mut impl Write, update: &TerminalRenderUpdate) -> Result<(), String> {
        // Packet rendering reconstructs terminal rows from retained grid
        // state. Keep the synthesized cursor moves invisible just as the
        // source application did with synchronized output: terminals that
        // implement mode 2026 present the repaint atomically, while hiding the
        // cursor also prevents visible thrash on terminals that ignore it.
        writer.write_all(b"\x1b[?2026h\x1b[?25l\x1b[?7l").map_err(|err| format!("begin synchronized packet render: {err}"))?;
        // Always attempt to close the synchronized batch: an early return that
        // leaves mode 2026 set can freeze the attached terminal until
        // something else resets it.
        let rendered = self.render_frame(writer, update);
        let finished = writer.write_all(b"\x1b[?7h\x1b[?2026l").map_err(|err| format!("finish synchronized packet render: {err}"));
        rendered.and(finished)
    }

    fn render_frame(&mut self, writer: &mut impl Write, update: &TerminalRenderUpdate) -> Result<(), String> {
        // Retain only the frame metadata needed to repaint the cached cells.
        self.last_update = Some(TerminalRenderUpdate {
            cols: update.cols,
            rows: update.rows,
            cursor: update.cursor,
            terminal_modes: update.terminal_modes,
            geometry: update.geometry,
            image_resources: update.image_resources.clone(),
            image_placements: update.image_placements.clone(),
            ..Default::default()
        });
        self.geometry.resize((update.cols, update.rows), self.viewport.unwrap_or((update.cols, update.rows)));
        let resized = self.cols != update.cols || self.rows != update.rows;
        let modes_changed = self.terminal_modes != update.terminal_modes;
        if resized {
            self.cols = update.cols;
            self.rows = update.rows;
            self.cells = vec![vec![crate::provider::TerminalRenderCell::default(); self.cols as usize]; self.rows as usize];
        }
        if self.terminal_modes.active_alternate_screen != update.terminal_modes.active_alternate_screen {
            if let Some(keyboard) = &self.keyboard {
                keyboard.lock().map_err(|_| "keyboard mode poisoned")?.leave_screen(writer).map_err(|e| e.to_string())?;
            }
        }
        render_packet_terminal_modes(writer, self.terminal_modes, update.terminal_modes)?;
        self.terminal_modes = update.terminal_modes;
        if let Some(keyboard) = &self.keyboard {
            keyboard.lock().map_err(|_| "keyboard mode poisoned")?.enter_screen(writer).map_err(|e| e.to_string())?;
        }
        let mut dirty_rows = std::collections::BTreeSet::new();
        if self.needs_full_repaint || resized || modes_changed {
            dirty_rows.extend(self.geometry.y..self.geometry.y.saturating_add(self.viewport.map_or(self.rows, |size| size.1)));
        }
        for op in &update.ops {
            match op.kind {
                crate::provider::TerminalRenderUpdateOpKind::FullVisibleReplace => {
                    self.cells = vec![vec![crate::provider::TerminalRenderCell::default(); self.cols as usize]; self.rows as usize];
                    self.replace_rows(&op.rows);
                    dirty_rows.extend(0..self.rows);
                }
                crate::provider::TerminalRenderUpdateOpKind::RowReplace => {
                    self.replace_rows(&op.rows);
                    dirty_rows.extend(op.rows.iter().map(|row| row.row).filter(|row| *row < self.rows));
                }
                crate::provider::TerminalRenderUpdateOpKind::ScrollCopy => {
                    let copied =
                        (0..op.row_count).filter_map(|offset| self.cells.get((op.src_row + offset) as usize).cloned()).collect::<Vec<_>>();
                    for (offset, row) in copied.into_iter().enumerate() {
                        let target_index = op.dst_row as usize + offset;
                        if let Some(target) = self.cells.get_mut(target_index) {
                            *target = row;
                            dirty_rows.insert(target_index as u16);
                        }
                    }
                }
            }
        }
        self.needs_full_repaint = false;
        let (visible_cols, visible_rows) = self.viewport.unwrap_or((self.cols, self.rows));
        for grid_row in dirty_rows.into_iter().filter(|row| *row >= self.geometry.y && *row - self.geometry.y < visible_rows) {
            let row_index = grid_row - self.geometry.y;
            // Overwrite retained cells without first erasing them. An outer
            // console can expose intermediate output despite mode 2026; EL2
            // otherwise flashes the current background across the whole row.
            let row = self.cells.get(grid_row as usize).map(Vec::as_slice).unwrap_or_default();
            let painted_cols = row.len().saturating_sub(self.geometry.x as usize).min(visible_cols as usize);
            for (col, cell) in row.iter().skip(self.geometry.x as usize).take(visible_cols as usize).enumerate() {
                if cell.style.width == crate::provider::TerminalCellWidth::SpacerTail && col > 0 {
                    continue;
                }
                // The host can assign a different width to a grapheme (for
                // example VS16 emoji with mode 2027). Grid coordinates, not
                // the host's advancing cursor, determine the next cell.
                write!(writer, "\x1b[{};{}H", row_index + 1, col + 1).map_err(|err| format!("position packet cell: {err}"))?;
                let clipped_left = col == 0
                    && self.geometry.x > 0
                    && row.get(self.geometry.x as usize - 1).is_some_and(|c| c.style.width == crate::provider::TerminalCellWidth::Wide);
                let clipped_right = cell.style.width == crate::provider::TerminalCellWidth::Wide && col + 1 >= painted_cols;
                if clipped_left || clipped_right {
                    let mut blank = cell.clone();
                    blank.graphemes.clear();
                    blank.style.width = crate::provider::TerminalCellWidth::Narrow;
                    render_packet_cell(writer, &blank)?;
                } else {
                    render_packet_cell(writer, cell)?;
                }
            }
            if painted_cols < visible_cols as usize {
                // Clear only the area outside the session grid (including
                // rows below it after a resize), using the host's background.
                write!(writer, "\x1b[{};{}H\x1b[0m\x1b[K", row_index + 1, painted_cols + 1)
                    .map_err(|err| format!("clear packet row margin: {err}"))?;
            }
        }
        writer.write_all(b"\x1b[0m").map_err(|err| format!("reset packet render style: {err}"))?;
        self.images.render(writer, update, (self.geometry.x, self.geometry.y), (visible_cols, visible_rows))?;
        if self.bounds {
            let right = self.cols.saturating_sub(self.geometry.x);
            let bottom = self.rows.saturating_sub(self.geometry.y);
            if right < visible_cols {
                for row in 0..bottom.min(visible_rows) {
                    write!(writer, "\x1b[{};{}H│", row + 1, right + 1).map_err(|e| e.to_string())?;
                }
            }
            if bottom < visible_rows {
                write!(writer, "\x1b[{};1H{}", bottom + 1, "─".repeat(right.min(visible_cols) as usize)).map_err(|e| e.to_string())?;
                if right < visible_cols {
                    write!(writer, "┘").map_err(|e| e.to_string())?;
                }
            }
        }
        write!(
            writer,
            "\x1b[{};{}H",
            update.cursor.row.saturating_sub(self.geometry.y).min(visible_rows.saturating_sub(1)).saturating_add(1),
            update.cursor.col.saturating_sub(self.geometry.x).min(visible_cols.saturating_sub(1)).saturating_add(1)
        )
        .map_err(|err| format!("position packet cursor: {err}"))?;
        let cursor_style = match update.cursor.style {
            crate::provider::TerminalCursorStyle::Block => 2,
            crate::provider::TerminalCursorStyle::Underline => 4,
            crate::provider::TerminalCursorStyle::Bar => 6,
            crate::provider::TerminalCursorStyle::BlockHollow => 2,
        };
        write!(writer, "\x1b[{cursor_style} q").map_err(|err| format!("set packet cursor style: {err}"))?;
        writer
            .write_all(if update.cursor.visible && self.geometry.contains(update.cursor.col, update.cursor.row) {
                b"\x1b[?25h"
            } else {
                b"\x1b[?25l"
            })
            .map_err(|err| format!("set packet cursor visibility: {err}"))
    }

    fn replace_rows(&mut self, rows: &[crate::provider::TerminalRenderRow]) {
        for row in rows {
            if let Some(target) = self.cells.get_mut(row.row as usize) {
                *target = row.cells.clone();
                target.resize(self.cols as usize, crate::provider::TerminalRenderCell::default());
            }
        }
    }
}

fn render_packet_terminal_modes(
    writer: &mut impl Write,
    previous: vt::TerminalModeState,
    current: vt::TerminalModeState,
) -> Result<(), String> {
    if previous.active_alternate_screen != current.active_alternate_screen {
        writer
            .write_all(if current.active_alternate_screen { b"\x1b[?1049h" } else { b"\x1b[?1049l" })
            .map_err(|err| format!("set packet alternate screen: {err}"))?;
    }
    if previous.application_cursor_keys != current.application_cursor_keys {
        writer
            .write_all(if current.application_cursor_keys { b"\x1b[?1h" } else { b"\x1b[?1l" })
            .map_err(|err| format!("set packet cursor-key mode: {err}"))?;
    }
    if previous.mouse_tracking_mode != current.mouse_tracking_mode {
        writer.write_all(b"\x1b[?9l\x1b[?1000l\x1b[?1002l\x1b[?1003l").map_err(|err| format!("reset packet mouse mode: {err}"))?;
        let enable = match current.mouse_tracking_mode {
            vt::MouseTrackingMode::None => None,
            vt::MouseTrackingMode::X10 | vt::MouseTrackingMode::Normal => Some(&b"\x1b[?1000h"[..]),
            vt::MouseTrackingMode::Button => Some(&b"\x1b[?1002h"[..]),
            vt::MouseTrackingMode::Any => Some(&b"\x1b[?1003h"[..]),
        };
        if let Some(enable) = enable {
            writer.write_all(enable).map_err(|err| format!("set packet mouse mode: {err}"))?;
        }
    }

    Ok(())
}

fn render_packet_cell(writer: &mut impl Write, cell: &crate::provider::TerminalRenderCell) -> Result<(), String> {
    use crate::provider::{TerminalCellFlags as Flags, TerminalCellWidth};
    if cell.style.width == TerminalCellWidth::SpacerTail {
        return Ok(());
    }
    writer.write_all(b"\x1b[0m").map_err(|err| format!("reset packet cell style: {err}"))?;
    let flags = cell.style.flags;
    if flags.contains(Flags::BOLD) {
        writer.write_all(b"\x1b[1m").map_err(|err| format!("write bold style: {err}"))?;
    }
    if flags.contains(Flags::FAINT) {
        writer.write_all(b"\x1b[2m").map_err(|err| format!("write faint style: {err}"))?;
    }
    if flags.contains(Flags::ITALIC) {
        writer.write_all(b"\x1b[3m").map_err(|err| format!("write italic style: {err}"))?;
    }
    if flags.contains(Flags::UNDERLINE) {
        writer.write_all(b"\x1b[4m").map_err(|err| format!("write underline style: {err}"))?;
    }
    if flags.contains(Flags::INVERSE) {
        writer.write_all(b"\x1b[7m").map_err(|err| format!("write inverse style: {err}"))?;
    }
    if flags.contains(Flags::BLINK) {
        writer.write_all(b"\x1b[5m").map_err(|err| format!("write blink style: {err}"))?;
    }
    if flags.contains(Flags::INVISIBLE) {
        writer.write_all(b"\x1b[8m").map_err(|err| format!("write invisible style: {err}"))?;
    }
    if flags.contains(Flags::STRIKETHROUGH) {
        writer.write_all(b"\x1b[9m").map_err(|err| format!("write strikethrough style: {err}"))?;
    }
    if flags.contains(Flags::OVERLINE) {
        writer.write_all(b"\x1b[53m").map_err(|err| format!("write overline style: {err}"))?;
    }
    let fg = cell.style.resolved_fg;
    let bg = cell.style.resolved_bg;
    write!(writer, "\x1b[38;2;{};{};{}m", fg.r, fg.g, fg.b).map_err(|err| format!("write packet cell foreground: {err}"))?;
    // Default backgrounds must stay default: Kitty images with z < -(1 << 30)
    // are drawn behind explicit cell backgrounds. Background-only cells carry
    // their colour in the content (palette = 2, RGB = 3), rather than the style.
    if cell.style.bg_color.tag != crate::provider::TerminalStyleColorTag::None || matches!(cell.style.content_tag, 2 | 3) {
        write!(writer, "\x1b[48;2;{};{};{}m", bg.r, bg.g, bg.b).map_err(|err| format!("write packet cell background: {err}"))?;
    }
    if cell.graphemes.is_empty() || cell.graphemes.contains(&0x10eeee) {
        writer.write_all(b" ").map_err(|err| format!("write packet blank cell: {err}"))?;
    } else {
        for codepoint in &cell.graphemes {
            if let Some(character) = char::from_u32(*codepoint) {
                write!(writer, "{character}").map_err(|err| format!("write packet grapheme: {err}"))?;
            }
        }
    }
    Ok(())
}

fn write_attach_output(writer: &mut impl Write, bytes: &[u8], watcher_state: Option<&SeatState>) -> Result<(), String> {
    writer.write_all(bytes).map_err(|err| format!("write stdout: {err}"))?;
    if let Some(state) = watcher_state {
        render_seat_chrome(writer, state)?;
    }
    Ok(())
}

fn update_watcher_chrome(writer: &mut impl Write, watcher_state: &mut Option<SeatState>, state: SeatState) -> Result<(), String> {
    if state.role == "watcher" {
        render_seat_chrome(writer, &state)?;
        *watcher_state = Some(state);
    } else if watcher_state.take().is_some() {
        render_seat_chrome(writer, &state)?;
    }
    Ok(())
}

fn render_seat_chrome(writer: &mut impl Write, state: &SeatState) -> Result<(), String> {
    let (_, rows) = current_terminal_size();
    render_seat_chrome_at_rows(writer, state, rows)
}

fn render_seat_chrome_at_rows(writer: &mut impl Write, state: &SeatState, rows: u16) -> Result<(), String> {
    if rows == 0 {
        return Ok(());
    }
    if state.role == "watcher" {
        let controller = state
            .controller
            .as_ref()
            .map(|identity| sanitize_attachment_name(identity.display_name()))
            .unwrap_or_else(|| "none".to_string());
        return render_watcher_message_at_rows(writer, rows, &format!("watching — controller: {controller}"));
    }

    writer.write_all(b"\x1b7").map_err(|err| format!("save cursor for watcher banner: {err}"))?;
    write!(writer, "\x1b[r\x1b[{};1H\x1b[2K", rows).map_err(|err| format!("clear watcher banner: {err}"))?;
    writer.write_all(b"\x1b8").map_err(|err| format!("restore cursor after watcher banner: {err}"))
}

fn render_watcher_message_at_rows(writer: &mut impl Write, rows: u16, message: &str) -> Result<(), String> {
    if rows == 0 {
        return Ok(());
    }
    writer.write_all(b"\x1b7").map_err(|err| format!("save cursor for watcher banner: {err}"))?;
    if rows > 1 {
        write!(writer, "\x1b[1;{}r", rows - 1).map_err(|err| format!("reserve watcher banner row: {err}"))?;
    }
    write!(writer, "\x1b[{};1H\x1b[2K\x1b[7m {message} \x1b[0m\x1b8", rows).map_err(|err| format!("write watcher banner: {err}"))
}

fn normalize_attachment_identity(mut identity: AttachmentIdentity) -> AttachmentIdentity {
    identity.name = sanitize_attachment_name(&identity.name);
    identity
}

fn sanitize_attachment_name(name: &str) -> String {
    let name = name.chars().filter(|character| !character.is_control()).take(128).collect::<String>();
    let name = name.trim();
    if name.is_empty() {
        "unknown".to_string()
    } else {
        name.to_string()
    }
}

fn is_graceful_socket_shutdown(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
    )
}

enum AttachCleanupTarget {
    Stdout,
    #[cfg(test)]
    Buffer(Arc<Mutex<Vec<u8>>>),
}

struct AttachCleanupGuard {
    keyboard: Option<Arc<Mutex<crate::attach_keyboard::KeyboardMode>>>,
    target: AttachCleanupTarget,
    enabled: bool,
    emitted: bool,
}

impl AttachCleanupGuard {
    fn stdout() -> Self {
        Self { keyboard: None, target: AttachCleanupTarget::Stdout, enabled: stdout_is_tty().unwrap_or(false), emitted: false }
    }

    #[cfg(test)]
    fn test_buffer(buffer: Arc<Mutex<Vec<u8>>>) -> Self {
        Self { keyboard: None, target: AttachCleanupTarget::Buffer(buffer), enabled: true, emitted: false }
    }

    #[cfg(test)]
    fn test_buffer_disabled(buffer: Arc<Mutex<Vec<u8>>>) -> Self {
        Self { keyboard: None, target: AttachCleanupTarget::Buffer(buffer), enabled: false, emitted: false }
    }

    fn restore_keyboard(&self, writer: &mut impl Write) -> Result<(), String> {
        if let Some(keyboard) = &self.keyboard {
            keyboard.lock().map_err(|_| "keyboard mode poisoned")?.close(writer).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn emit(&mut self) -> Result<(), String> {
        if !self.enabled || self.emitted {
            return Ok(());
        }
        let result = match &self.target {
            AttachCleanupTarget::Stdout => {
                let mut stdout = std::io::stdout().lock();
                self.restore_keyboard(&mut stdout).and_then(|_| write_detach_cleanup(&mut stdout))
            }
            #[cfg(test)]
            AttachCleanupTarget::Buffer(buffer) => {
                if let Ok(mut buffer) = buffer.lock() {
                    self.restore_keyboard(&mut *buffer).and_then(|_| write_detach_cleanup(&mut *buffer))
                } else {
                    Err("cleanup buffer lock poisoned".to_string())
                }
            }
        };
        if result.is_ok() {
            self.emitted = true;
        }
        result
    }
}

impl Drop for AttachCleanupGuard {
    fn drop(&mut self) {
        let _ = self.emit();
    }
}

fn write_detach_cleanup<W: Write>(writer: &mut W) -> Result<(), String> {
    writer.write_all(DETACH_CLEANUP_SEQUENCE).map_err(|err| format!("write detach cleanup: {err}"))?;
    writer.flush().map_err(|err| format!("flush detach cleanup: {err}"))
}

pub fn ensure_session_started(
    layout: &RuntimeLayout,
    id: Option<String>,
    vt_engine: Option<VtEngineKind>,
    cwd: Option<PathBuf>,
    cmd: Option<String>,
    options: SessionStartOptions,
) -> Result<SessionMetadata, String> {
    start_session(layout, id, vt_engine, cwd, cmd, options, DaemonRequirement::AutoStart)
}

pub(crate) fn start_session_in_running_daemon(
    layout: &RuntimeLayout,
    expected_daemon_pid: u32,
    id: Option<String>,
    vt_engine: Option<VtEngineKind>,
    cwd: Option<PathBuf>,
    cmd: Option<String>,
    options: SessionStartOptions,
) -> Result<SessionMetadata, String> {
    start_session(layout, id, vt_engine, cwd, cmd, options, DaemonRequirement::AlreadyRunning(expected_daemon_pid))
}

#[derive(Clone, Copy)]
enum DaemonRequirement {
    AutoStart,
    AlreadyRunning(u32),
}

fn start_session(
    layout: &RuntimeLayout,
    id: Option<String>,
    vt_engine: Option<VtEngineKind>,
    cwd: Option<PathBuf>,
    cmd: Option<String>,
    options: SessionStartOptions,
    daemon_requirement: DaemonRequirement,
) -> Result<SessionMetadata, String> {
    let vt_engine = vt_engine.unwrap_or_else(vt::default_vt_engine_kind);
    vt_engine.ensure_available()?;
    let id = id.unwrap_or_else(|| format!("session-{}", uuid::Uuid::new_v4()));
    crate::runtime::validate_runtime_name(&id)?;
    crate::runtime::validate_environment(&options.environment)?;
    let mut session = layout.session_metadata(id, vt_engine, cwd, cmd);
    session.record = options.record;
    session.initial_size = options.initial_size;
    session.colors = options.colors;
    session.tags = options.tags;
    session.environment = options.environment;
    crate::runtime::normalize_tags(&mut session.tags);

    if matches!(daemon_requirement, DaemonRequirement::AutoStart) {
        ensure_daemon_started(layout)?;
    }
    let mut stream = connect_session_stream(&layout.socket_path()).map_err(|err| match daemon_requirement {
        DaemonRequirement::AutoStart => err,
        DaemonRequirement::AlreadyRunning(_) => format!("source daemon {} is no longer running: {err}", layout.daemon_name()),
    })?;
    let body = serde_json::to_vec(&session).map_err(|err| format!("serialize session create request: {err}"))?;
    match daemon_requirement {
        DaemonRequirement::AutoStart => http_uds::write_request(&mut stream, http::Method::POST, "/sessions", &body),
        DaemonRequirement::AlreadyRunning(expected_pid) => http_uds::write_session_create_request(&mut stream, &body, expected_pid),
    }
    .map_err(|err| format!("write session create request: {err}"))?;
    let response = http_uds::read_response(&mut stream).map_err(|err| format!("read session create response: {err}"))?;
    if response.status != StatusCode::OK {
        return Err(http_error_message(http_uds::HttpResponse { status: response.status, body: response.body }));
    }
    let response: http_uds::CreateSessionResponse =
        serde_json::from_slice(&response.body).map_err(|err| format!("parse session create response: {err}"))?;
    Ok(response.session)
}

pub fn attach_foreground(
    layout: &RuntimeLayout,
    id: &str,
    identity: AttachmentIdentity,
    strict: bool,
    take: bool,
) -> Result<ForegroundAttach, String> {
    connect_foreground_upgrade(layout, id, "attach", identity, strict, take)
}

pub fn attach_packet_foreground(
    layout: &RuntimeLayout,
    id: &str,
    identity: AttachmentIdentity,
    role: ChannelRole,
    strict: bool,
    take: bool,
) -> Result<ForegroundAttach, String> {
    const FOREGROUND_CHANNEL: u32 = 1;
    let (mut stream, directory) = crate::provider_daemon::connect_packet_stream(layout, &[])?;
    if !directory.sessions.iter().any(|entry| entry.session_id == id) {
        return Err(format!("session {id} was not present in packet directory"));
    }
    let (cols, rows) = current_terminal_size();
    PacketFrame::new(CHANNEL_CONTROL, MSG_CONTROL_OPEN_CHANNEL, &OpenChannel {
        channel: FOREGROUND_CHANNEL,
        session_id: id.to_string(),
        role,
        take,
        identity,
    })
    .map_err(|err| format!("encode foreground packet channel: {err}"))?
    .write(&mut stream)
    .map_err(|err| format!("open foreground packet channel: {err}"))?;
    PacketFrame::new(FOREGROUND_CHANNEL, MSG_SESSION_RESIZE, &Resize { cols, rows })
        .map_err(|err| format!("encode foreground packet resize: {err}"))?
        .write(&mut stream)
        .map_err(|err| format!("resize foreground packet channel: {err}"))?;

    let mut images = ImageReceiver::default();
    let mut initial_role = None;
    let mut initial_update = None;
    while initial_role.is_none() || initial_update.is_none() {
        let frame = PacketFrame::read(&mut stream).map_err(|err| format!("read foreground packet handshake: {err}"))?;
        match (frame.channel, frame.msg_type) {
            (FOREGROUND_CHANNEL, MSG_SESSION_ROLE) => {
                initial_role = Some(frame.decode::<RoleState>().map_err(|err| format!("decode foreground role: {err}"))?);
            }
            (FOREGROUND_CHANNEL, crate::packet::MSG_SESSION_IMAGE_FILE) => {
                let file = frame.decode::<crate::packet::ImageFile>().map_err(|e| e.to_string())?;
                let acquired = images.file(&file);
                PacketFrame::new(FOREGROUND_CHANNEL, crate::packet::MSG_SESSION_IMAGE_FILE_RESULT, &crate::packet::ImageFileResult {
                    image_id: file.image_id,
                    generation: file.generation,
                    acquired,
                })
                .map_err(|e| e.to_string())?
                .write(&mut stream)
                .map_err(|e| e.to_string())?;
            }
            (FOREGROUND_CHANNEL, crate::packet::MSG_SESSION_IMAGE) => {
                images.chunk(frame.decode().map_err(|e| e.to_string())?)?;
            }
            (FOREGROUND_CHANNEL, MSG_SESSION_RENDER) => {
                initial_update = Some(frame.decode::<RenderPacket>().map_err(|err| format!("decode foreground render: {err}"))?.update);
            }
            (CHANNEL_CONTROL, MSG_CONTROL_ERROR) => {
                let error = frame.decode::<ControlError>().map_err(|err| format!("decode foreground open error: {err}"))?;
                if error.channel == FOREGROUND_CHANNEL {
                    return Err(error.message);
                }
            }
            _ => {}
        }
    }
    let initial_role = initial_role.expect("role checked above");
    if strict && initial_role.role != ChannelRole::Controller {
        let holder = initial_role
            .controller
            .as_ref()
            .map(|identity| format!("{} ({})", identity.name, identity.kind.as_str()))
            .unwrap_or_else(|| "unknown".to_string());
        return Err(format!("session {id} controller seat is held by {holder}"));
    }
    Ok(ForegroundAttach {
        transport: ForegroundTransport::Packet(Box::new(PacketForegroundAttach {
            session_name: id.to_owned(),
            stream: Arc::new(Mutex::new(stream)),
            channel: FOREGROUND_CHANNEL,
            initial_update: initial_update.expect("render checked above"),
            initial_role,
            images,
        })),
    })
}

pub fn watch_foreground(layout: &RuntimeLayout, id: &str, identity: AttachmentIdentity) -> Result<ForegroundAttach, String> {
    connect_foreground_upgrade(layout, id, "watch", identity, false, false)
}

fn connect_foreground_upgrade(
    layout: &RuntimeLayout,
    id: &str,
    role: &str,
    identity: AttachmentIdentity,
    strict: bool,
    take: bool,
) -> Result<ForegroundAttach, String> {
    let socket_path = layout.socket_path();
    let deadline = Instant::now() + Duration::from_millis(250);
    loop {
        let mut stream = connect_session_stream(&socket_path)?;
        let (cols, rows) = current_terminal_size();
        let body = serde_json::to_vec(&http_uds::AttachRequest {
            cols,
            rows,
            capabilities: attach_capabilities_to_http(attach_init_capabilities()),
            identity: identity.clone(),
            take,
            strict,
        })
        .map_err(|err| format!("serialize attach request: {err}"))?;
        http_uds::write_attach_upgrade_request(&mut stream, &format!("/sessions/{id}/{role}"), &body)
            .map_err(|err| format!("write {role} upgrade request: {err}"))?;
        let response = http_uds::read_response_head(&mut stream).map_err(|err| format!("read {role} upgrade response: {err}"))?;
        match response.status {
            StatusCode::SWITCHING_PROTOCOLS => {
                return Ok(ForegroundAttach { transport: ForegroundTransport::Legacy(Arc::new(Mutex::new(stream))) })
            }
            StatusCode::CONFLICT => {
                let mut body = String::new();
                let _ = stream.read_to_string(&mut body);
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&body) {
                    if let Some(error) = value.get("error").and_then(serde_json::Value::as_str) {
                        return Err(error.to_string());
                    }
                }
            }
            other => return Err(format!("unexpected {role} response: {other}")),
        }
        if Instant::now() >= deadline {
            return Err(format!("session {id} controller seat is held"));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

pub fn session_socket_path(root: &Path, id: &str) -> PathBuf {
    let _ = id;
    RuntimeLayout::new(root.to_path_buf()).socket_path()
}

pub fn daemon_pid_path(root: &Path, id: &str) -> PathBuf {
    let _ = id;
    RuntimeLayout::new(root.to_path_buf()).daemon_pid_path()
}

pub fn foreground_path(root: &Path, id: &str) -> PathBuf {
    RuntimeLayout::new(root.to_path_buf()).foreground_path(id)
}

fn default_vt_engine(session: &SessionMetadata) -> Result<Box<dyn VtEngine>, String> {
    #[cfg(test)]
    if session.vt_engine == VtEngineKind::Ghostty {
        return Ok(Box::new(TestReplayProbeVtEngine::new(session.initial_size.cols, session.initial_size.rows)));
    }

    if std::env::var_os("CARGO_BIN_EXE_cleat").is_some()
        && std::env::var_os("CLEAT_TEST_VT_ENGINE").as_deref() == Some(std::ffi::OsStr::new("replay-probe"))
    {
        return Ok(Box::new(TestReplayProbeVtEngine::new(session.initial_size.cols, session.initial_size.rows)));
    }
    vt::make_vt_engine_with_colors(session.vt_engine, session.initial_size.cols, session.initial_size.rows, session.colors)
}

#[derive(Debug)]
struct TestReplayProbeVtEngine {
    cols: u16,
    rows: u16,
}

impl TestReplayProbeVtEngine {
    fn new(cols: u16, rows: u16) -> Self {
        Self { cols, rows }
    }
}

impl VtEngine for TestReplayProbeVtEngine {
    fn feed(&mut self, _bytes: &[u8]) -> Result<(), String> {
        Ok(())
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<(), String> {
        self.cols = cols;
        self.rows = rows;
        Ok(())
    }

    fn supports_replay(&self) -> bool {
        true
    }

    fn replay_payload(&self, capabilities: &vt::ClientCapabilities) -> Result<Option<Vec<u8>>, String> {
        let payload = format!("{:?}:{}", capabilities.color_level, capabilities.kitty_keyboard);
        Ok(Some(payload.into_bytes()))
    }

    fn screen_text(&self) -> Result<String, String> {
        Ok(format!("probe:{}x{}", self.cols, self.rows))
    }

    fn screen_grid(&mut self) -> Result<ScreenGrid, String> {
        Ok(ScreenGrid::default())
    }

    fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }
}

#[cfg(test)]
fn record_pty_output(engine: &mut dyn VtEngine, bytes: &[u8]) -> Result<(), String> {
    engine.feed(bytes)
}

fn attach_init_capabilities() -> vt::ClientCapabilities {
    vt::ClientCapabilities::detect()
}

fn attach_capabilities_to_http(capabilities: vt::ClientCapabilities) -> http_uds::AttachCapabilitiesRequest {
    http_uds::AttachCapabilitiesRequest {
        color_level: match capabilities.color_level {
            vt::ColorLevel::Sixteen => http_uds::AttachColorLevelRequest::Sixteen,
            vt::ColorLevel::Ansi256 => http_uds::AttachColorLevelRequest::Ansi256,
            vt::ColorLevel::TrueColor => http_uds::AttachColorLevelRequest::TrueColor,
        },
        kitty_keyboard: capabilities.kitty_keyboard,
    }
}

fn attach_capabilities_from_http(capabilities: http_uds::AttachCapabilitiesRequest) -> vt::ClientCapabilities {
    let color_level = match capabilities.color_level {
        http_uds::AttachColorLevelRequest::Sixteen => vt::ColorLevel::Sixteen,
        http_uds::AttachColorLevelRequest::Ansi256 => vt::ColorLevel::Ansi256,
        http_uds::AttachColorLevelRequest::TrueColor => vt::ColorLevel::TrueColor,
    };
    vt::ClientCapabilities::new(color_level, capabilities.kitty_keyboard)
}

#[cfg(test)]
fn apply_attach_state(
    engine: &mut dyn VtEngine,
    cols: u16,
    rows: u16,
    capabilities: &vt::ClientCapabilities,
) -> Result<Option<Vec<u8>>, String> {
    engine.resize(cols, rows)?;
    if engine.supports_replay() {
        engine.replay_payload(capabilities)
    } else {
        Ok(None)
    }
}

struct PendingWait {
    stream: SessionStream,
    conditions: Vec<crate::protocol::WaitCondition>,
    screen_stable: Option<ScreenStableState>,
    timeout_ms: u64,
    registered_at: Instant,
}

#[derive(Clone, Debug, PartialEq)]
struct ScreenStableFingerprint {
    cols: u16,
    rows: u16,
    viewport_kind: crate::provider::TerminalViewportKind,
    scrollback_offset_rows: u64,
    cells: Vec<crate::provider::TerminalCell>,
}

impl ScreenStableFingerprint {
    fn from_snapshot(snapshot: crate::provider::TerminalSnapshot) -> Self {
        Self {
            cols: snapshot.cols,
            rows: snapshot.rows,
            viewport_kind: snapshot.viewport_kind,
            scrollback_offset_rows: snapshot.scrollback_offset_rows,
            cells: snapshot.cells,
        }
    }

    fn significant_change_from(&self, other: &Self) -> bool {
        if self.cols != other.cols
            || self.rows != other.rows
            || self.viewport_kind != other.viewport_kind
            || self.scrollback_offset_rows != other.scrollback_offset_rows
            || self.cells.len() != other.cells.len()
        {
            return true;
        }

        self.cells.iter().zip(&other.cells).filter(|(left, right)| left != right).count() > SCREEN_STABLE_CHANGED_CELL_TOLERANCE
    }
}

#[derive(Clone, Debug)]
struct ScreenStableState {
    fingerprint: ScreenStableFingerprint,
    stable_since: Instant,
}

impl ScreenStableState {
    fn new(fingerprint: ScreenStableFingerprint, stable_since: Instant) -> Self {
        Self { fingerprint, stable_since }
    }

    fn observe(&mut self, fingerprint: ScreenStableFingerprint, observed_at: Instant) {
        if self.fingerprint.significant_change_from(&fingerprint) {
            self.fingerprint = fingerprint;
            self.stable_since = observed_at;
        }
    }
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

struct PendingExpect {
    stream: SessionStream,
    text: String,
    since_offset: u64,
    last_checked_file_size: u64,
    timeout_ms: u64,
    registered_at: Instant,
}

/// Identity of a session channel on a packet connection, stable across the
/// packet_clients vec's swap_remove reordering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct PacketChannelRef {
    client_id: u64,
    channel: u32,
}

impl PacketChannelRef {
    fn view_id(self) -> u128 {
        (u128::from(self.client_id) << 32) | u128::from(self.channel)
    }
}

struct HostedSession {
    metadata: SessionMetadata,
    actor: SessionActor,
    raw_output_tap: RawOutputTap,
    active_client: Option<ActiveClient>,
    watchers: Vec<ActiveClient>,
    packet_control: crate::attachment_control::AttachmentControl<PacketChannelRef>,
    fixed_size: Option<(u16, u16)>,
    focused: bool,
    applied_size: (u16, u16),
    applied_cell_size: (u32, u32),
    packet_render_cache: PacketRenderCache,
    had_foreground_client: bool,
    pending_waits: Vec<PendingWait>,
    pending_expects: Vec<PendingExpect>,
    /// Render generation at the last screen-stable fingerprint snapshot; lets
    /// the servicing loop skip full-grid snapshots while nothing has rendered.
    screen_stable_snapshot_generation: Option<u64>,
    should_keep_session_dir: bool,
}

impl HostedSession {
    fn spawn(session_dir: PathBuf, session: SessionMetadata, coordinates: AmbientSessionCoordinates) -> Result<Self, String> {
        let should_keep_session_dir = session.record;
        let actor_session_dir = session_dir;
        let actor_session = session.clone();
        let actor = SessionActor::spawn(session.initial_size.rows, Arc::new(|| {}), move || {
            crate::session_runtime::SessionRuntime::spawn_in_daemon(
                actor_session_dir,
                &actor_session,
                default_vt_engine(&actor_session)?,
                &coordinates,
            )
        })?;
        let raw_output_tap = actor.subscribe_raw_output()?;
        Ok(Self {
            applied_size: (session.initial_size.cols, session.initial_size.rows),
            applied_cell_size: (1, 1),
            metadata: session,
            actor,
            raw_output_tap,
            active_client: None,
            watchers: Vec::new(),
            packet_control: Default::default(),
            fixed_size: None,
            focused: false,
            packet_render_cache: PacketRenderCache::default(),
            had_foreground_client: false,
            pending_waits: Vec::new(),
            pending_expects: Vec::new(),
            screen_stable_snapshot_generation: None,
            should_keep_session_dir,
        })
    }

    fn activity_session(&self, stable_threshold_ms: u64) -> ActivitySession {
        let activity = self.actor.screen_activity().snapshot(Instant::now(), Duration::from_millis(stable_threshold_ms));
        ActivitySession {
            session_id: self.metadata.id.clone(),
            tags: self.metadata.tags.clone(),
            activity: activity.screen_activity,
            stable_since_unix_ms: activity.quiet_since,
            last_output_at_unix_ms: activity.last_output_at,
        }
    }
}

fn write_http_wait_result(stream: &mut SessionStream, status: crate::protocol::WaitStatus, elapsed_ms: u64) -> std::io::Result<()> {
    http_uds::write_json(stream, StatusCode::OK, &http_uds::WaitResultResponse { status: wait_status_to_http(status), elapsed_ms })
}

fn enqueue_output_chunk(
    layout: &RuntimeLayout,
    id: &str,
    actor: &SessionActor,
    active_client: &mut Option<ActiveClient>,
    watchers: &mut Vec<ActiveClient>,
    chunk: &[u8],
) {
    if let Some(client) = active_client.as_mut() {
        if client.enqueue_output(chunk).is_err() {
            let _ = fs::remove_file(layout.foreground_path(id));
            let _ = actor.record_detach();
            let _ = actor.set_query_passthrough(false);
            *active_client = None;
        }
    }
    watchers.retain_mut(|watcher| watcher.enqueue_output(chunk).is_ok());
}

fn drain_raw_output_tap(
    layout: &RuntimeLayout,
    id: &str,
    actor: &SessionActor,
    raw_output_tap: &mut RawOutputTap,
    active_client: &mut Option<ActiveClient>,
    watchers: &mut Vec<ActiveClient>,
) -> Result<bool, String> {
    let mut drained = false;
    loop {
        match raw_output_tap.try_recv() {
            Ok(chunk) => {
                drained = true;
                enqueue_output_chunk(layout, id, actor, active_client, watchers, &chunk.bytes);
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => return Ok(drained),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                recover_raw_output_tap(layout, id, actor, raw_output_tap, active_client, watchers, None)?;
                return Ok(drained);
            }
        }
    }
}

fn drain_raw_output_tap_before_client_install(
    layout: &RuntimeLayout,
    id: &str,
    hosted: &mut HostedSession,
    new_client: &mut ActiveClient,
    replay: RawOutputReplay,
    replay_mode: ReplayMode,
) -> Result<(), String> {
    let plan = plan_raw_output_client_install(&hosted.raw_output_tap, replay, replay_mode);
    let existing_chunks = match &plan {
        RawOutputClientInstallPlan::Complete { existing_chunks, .. } | RawOutputClientInstallPlan::Disconnected { existing_chunks } => {
            existing_chunks
        }
    };
    for chunk in existing_chunks {
        enqueue_output_chunk(layout, id, &hosted.actor, &mut hosted.active_client, &mut hosted.watchers, chunk);
    }
    match plan {
        RawOutputClientInstallPlan::Complete { new_client_frames, .. } => {
            enqueue_frames(new_client, &new_client_frames)?;
        }
        RawOutputClientInstallPlan::Disconnected { .. } => {
            recover_raw_output_tap(
                layout,
                id,
                &hosted.actor,
                &mut hosted.raw_output_tap,
                &mut hosted.active_client,
                &mut hosted.watchers,
                Some(NewRawOutputClient { client: new_client, replay_mode }),
            )?;
        }
    }
    Ok(())
}

enum RawOutputClientInstallPlan {
    Complete { existing_chunks: Vec<Arc<[u8]>>, new_client_frames: Vec<Frame> },
    Disconnected { existing_chunks: Vec<Arc<[u8]>> },
}

fn plan_raw_output_client_install(
    raw_output_tap: &RawOutputTap,
    replay: RawOutputReplay,
    replay_mode: ReplayMode,
) -> RawOutputClientInstallPlan {
    let mut existing_chunks = Vec::new();
    let mut live_after_replay = Vec::new();
    loop {
        match raw_output_tap.try_recv() {
            Ok(chunk) => {
                if chunk.sequence > replay.through_sequence {
                    live_after_replay.push(Arc::clone(&chunk.bytes));
                }
                existing_chunks.push(chunk.bytes);
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                let mut new_client_frames = replay_frames(replay.payload, replay_mode);
                // Install-time only (never the per-byte path), so copying the
                // few live chunks into owned `Frame` payloads is fine.
                new_client_frames.extend(live_after_replay.into_iter().map(|bytes| Frame::Output(bytes.to_vec())));
                return RawOutputClientInstallPlan::Complete { existing_chunks, new_client_frames };
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                return RawOutputClientInstallPlan::Disconnected { existing_chunks };
            }
        }
    }
}

trait RawOutputRecoverySource {
    fn recover_raw_output(&self, capabilities: Vec<vt::ClientCapabilities>) -> Result<RawOutputRecovery, String>;
}

impl RawOutputRecoverySource for SessionActor {
    fn recover_raw_output(&self, capabilities: Vec<vt::ClientCapabilities>) -> Result<RawOutputRecovery, String> {
        SessionActor::recover_raw_output(self, capabilities)
    }
}

#[derive(Clone, Copy)]
enum ReplayMode {
    FreshTerminal,
    ResetTerminal,
}

impl ReplayMode {
    fn resets_terminal(self) -> bool {
        matches!(self, Self::ResetTerminal)
    }
}

#[derive(Clone, Copy)]
struct RawOutputRecoveryRequest {
    capabilities: vt::ClientCapabilities,
    replay_mode: ReplayMode,
}

struct RawOutputRecoveryPlan {
    tap: RawOutputTap,
    recipient_frames: Vec<Vec<Frame>>,
}

fn plan_raw_output_recovery(
    source: &impl RawOutputRecoverySource,
    requests: &[RawOutputRecoveryRequest],
) -> Result<RawOutputRecoveryPlan, String> {
    let recovery = source.recover_raw_output(requests.iter().map(|request| request.capabilities).collect())?;
    if recovery.payloads.len() != requests.len() {
        return Err(format!("raw output recovery returned {} payloads for {} clients", recovery.payloads.len(), requests.len()));
    }
    let recipient_frames =
        recovery.payloads.into_iter().zip(requests).map(|(payload, request)| replay_frames(payload, request.replay_mode)).collect();
    Ok(RawOutputRecoveryPlan { tap: recovery.tap, recipient_frames })
}

fn replay_frames(payload: Option<Vec<u8>>, replay_mode: ReplayMode) -> Vec<Frame> {
    let Some(payload) = payload.filter(|payload| !payload.is_empty()) else {
        return Vec::new();
    };
    let mut frames = Vec::with_capacity(usize::from(replay_mode.resets_terminal()) + 1);
    if replay_mode.resets_terminal() {
        frames.push(Frame::Output(REATTACH_CLEAR_SEQUENCE.to_vec()));
    }
    frames.push(Frame::Output(payload));
    frames
}

fn enqueue_frames(client: &mut ActiveClient, frames: &[Frame]) -> Result<(), String> {
    for frame in frames {
        client.enqueue_frame(frame)?;
    }
    Ok(())
}

struct NewRawOutputClient<'a> {
    client: &'a mut ActiveClient,
    replay_mode: ReplayMode,
}

fn recover_raw_output_tap(
    layout: &RuntimeLayout,
    id: &str,
    actor: &SessionActor,
    raw_output_tap: &mut RawOutputTap,
    active_client: &mut Option<ActiveClient>,
    watchers: &mut Vec<ActiveClient>,
    new_client: Option<NewRawOutputClient<'_>>,
) -> Result<(), String> {
    let requests: Vec<_> = active_client
        .iter()
        .map(|client| RawOutputRecoveryRequest { capabilities: client.capabilities, replay_mode: ReplayMode::ResetTerminal })
        .chain(
            watchers
                .iter()
                .map(|watcher| RawOutputRecoveryRequest { capabilities: watcher.capabilities, replay_mode: ReplayMode::ResetTerminal }),
        )
        .chain(new_client.as_ref().map(|new_client| RawOutputRecoveryRequest {
            capabilities: new_client.client.capabilities,
            replay_mode: new_client.replay_mode,
        }))
        .collect();
    let recovery = plan_raw_output_recovery(actor, &requests)?;
    *raw_output_tap = recovery.tap;
    let mut recipient_frames = recovery.recipient_frames.into_iter();
    if let Some(client) = active_client.as_mut() {
        if recipient_frames.next().is_some_and(|frames| enqueue_frames(client, &frames).is_err()) {
            let _ = fs::remove_file(layout.foreground_path(id));
            let _ = actor.record_detach();
            let _ = actor.set_query_passthrough(false);
            *active_client = None;
        }
    }
    watchers.retain_mut(|watcher| recipient_frames.next().is_none_or(|frames| enqueue_frames(watcher, &frames).is_ok()));
    if let Some(new_client) = new_client {
        if let Some(frames) = recipient_frames.next() {
            enqueue_frames(new_client.client, &frames)?;
        }
    }
    Ok(())
}

fn drain_watcher_inputs(watchers: &mut Vec<ActiveClient>) {
    let mut ignored = VecDeque::new();
    watchers.retain_mut(|watcher| {
        ignored.clear();
        watcher.drain_input_frames(&mut ignored, Duration::ZERO).unwrap_or_default()
    });
}

fn flush_watchers(watchers: &mut Vec<ActiveClient>) {
    watchers.retain_mut(|watcher| watcher.flush_pending_output().unwrap_or(false));
}

#[cfg(any(unix, windows))]
pub fn run_session_daemon(root: &Path, daemon_name: &str) -> Result<(), String> {
    let layout = RuntimeLayout::new(root.to_path_buf()).with_daemon(daemon_name.to_string())?;
    let socket_path = layout.socket_path();
    validate_session_socket_path(&socket_path)?;
    layout.ensure_daemon_dirs()?;
    let listener = match bind_session_listener(&socket_path) {
        Ok(listener) => listener,
        Err(first_err) => {
            if try_connect_session_stream(&socket_path).is_ok() {
                return Ok(());
            }
            match fs::remove_file(&socket_path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(format!("remove stale daemon socket {}: {err}", socket_path.display())),
            }
            bind_session_listener(&socket_path).map_err(|second_err| format!("{first_err}; after stale cleanup: {second_err}"))?
        }
    };
    set_listener_nonblocking(&listener, true)?;
    let daemon_pid = std::process::id().to_string();
    fs::write(layout.daemon_pid_path(), &daemon_pid).map_err(|err| format!("write daemon pid: {err}"))?;

    let mut sessions: HashMap<String, HostedSession> = HashMap::new();
    let mut packet_clients: Vec<PacketClient> = Vec::new();
    let mut pending_http_handshakes: Vec<PendingHttpHandshake> = Vec::new();
    let mut next_packet_client_id: u64 = 1;
    let mut exited_session: Option<(String, bool)> = None;
    let mut faulted_session: Option<String> = None;
    let mut idle_since = Some(Instant::now());
    let mut last_registration_check = Instant::now();
    let mut registration_was_lost = false;

    loop {
        // Self-fencing watchdog: the pid file registers which process owns
        // this daemon identity. If it no longer names us — the runtime root
        // was deleted (e.g. a test tempdir was cleaned up), or another daemon
        // reclaimed the socket — no client can ever route to us again, so
        // terminate the hosted sessions and exit instead of servicing them
        // forever. Two consecutive misses guard against transient read
        // failures.
        if last_registration_check.elapsed() >= SESSION_DAEMON_REGISTRATION_CHECK_INTERVAL {
            last_registration_check = Instant::now();
            // Only a definitively missing pid file (or one naming another
            // process) counts as deregistration. A transient read failure
            // (EIO, permissions blip) must not self-terminate a healthy
            // daemon — that would drop every hosted session.
            let registered = match fs::read_to_string(layout.daemon_pid_path()) {
                Ok(contents) => contents.trim() == daemon_pid,
                Err(err) => err.kind() != std::io::ErrorKind::NotFound,
            };
            if !registered && registration_was_lost {
                for hosted in sessions.values() {
                    // Tree, not Leader: this path has the same orphaned-child
                    // problem as SessionDelete — a background child in the
                    // leader's process group must not outlive the fenced daemon.
                    let _ = hosted.actor.dispatch_signal(TERMINATE_SIGNAL, crate::protocol::SignalTarget::Tree);
                }
                // Deliberately leave the socket and pid file alone on this
                // path: they are either already gone or owned by a successor.
                return Ok(());
            }
            registration_was_lost = !registered;
        }

        let mut did_work = false;

        loop {
            match retry_interrupted(|| listener.accept()) {
                Ok((stream, _)) => {
                    did_work = true;
                    if let Ok(pending) = PendingHttpHandshake::new(stream) {
                        pending_http_handshakes.push(pending);
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) => return Err(format!("accept client: {err}")),
            }
        }

        let mut handshake_index = 0;
        while handshake_index < pending_http_handshakes.len() {
            match pending_http_handshakes[handshake_index].poll() {
                Ok(HttpHandshakePoll::Pending(handshake_did_work)) => {
                    did_work |= handshake_did_work;
                    handshake_index += 1;
                }
                Ok(HttpHandshakePoll::Ready(request)) => {
                    did_work = true;
                    let pending = pending_http_handshakes.swap_remove(handshake_index);
                    let mut stream = pending.into_stream();
                    if set_stream_nonblocking(&stream, false).is_err() {
                        continue;
                    }
                    let mut http_state = HttpRequestState {
                        layout: &layout,
                        sessions: &mut sessions,
                        packet_clients: &mut packet_clients,
                        next_packet_client_id: &mut next_packet_client_id,
                    };
                    let mut response_committed = false;
                    if let Err(err) =
                        handle_http_request(root, daemon_name, &mut stream, *request, &mut http_state, &mut response_committed)
                    {
                        if !response_committed {
                            let _ = http_uds::write_error(&mut stream, StatusCode::INTERNAL_SERVER_ERROR, &err);
                        }
                    }
                    if !sessions.is_empty() {
                        idle_since = None;
                    }
                }
                Err(err) => {
                    did_work = true;
                    let pending = pending_http_handshakes.swap_remove(handshake_index);
                    if err.kind() == io::ErrorKind::InvalidData {
                        let mut stream = pending.into_stream();
                        let _ = set_stream_nonblocking(&stream, false);
                        let _ = http_uds::write_error(&mut stream, StatusCode::INTERNAL_SERVER_ERROR, &format!("read HTTP request: {err}"));
                    }
                }
            }
        }

        did_work |= service_packet_clients(&layout, &mut sessions, &mut packet_clients)?;
        let session_ids: Vec<String> = sessions.keys().cloned().collect();
        for session_id in session_ids {
            let Some(hosted) = sessions.get_mut(&session_id) else {
                continue;
            };
            let service_result = contain_session_unwind(&session_id, || -> Result<(bool, Option<bool>), String> {
                maybe_panic_for_containment_test(&session_id);
                let session_did_work = service_hosted_session(&layout, &session_id, hosted, &mut packet_clients)?;
                // Exit state is read from the observation mirror — never a
                // blocking round-trip into a possibly-busy actor.
                let exit_code = hosted.actor.observation().exit_code();
                if exit_code.is_none() && hosted.actor.worker_finished() {
                    return Err("session actor stopped without reporting an exit".to_string());
                }
                let should_keep_session_dir = if exit_code.is_some() {
                    Some(finish_exited_session(&layout, &session_id, hosted, &mut packet_clients)?)
                } else {
                    None
                };
                Ok((session_did_work, should_keep_session_dir))
            });
            match service_result {
                Some((session_did_work, Some(should_keep_session_dir))) => {
                    did_work |= session_did_work;
                    exited_session = Some((session_id, should_keep_session_dir));
                    break;
                }
                Some((session_did_work, None)) => {
                    did_work |= session_did_work;
                }
                None => {
                    did_work = true;
                    faulted_session = Some(session_id);
                    break;
                }
            }
        }

        if let Some(session_id) = faulted_session.take() {
            cleanup_exited_session(&layout, &session_id, true);
            broadcast_directory_remove(&session_id, &mut packet_clients)?;
            sessions.remove(&session_id);
            remove_packet_channels_for_session(&session_id, &mut packet_clients);
        }

        if let Some((session_id, should_keep_session_dir)) = exited_session.take() {
            cleanup_exited_session(&layout, &session_id, should_keep_session_dir);
            broadcast_directory_remove(&session_id, &mut packet_clients)?;
            sessions.remove(&session_id);
            remove_packet_channels_for_session(&session_id, &mut packet_clients);
        }

        service_activity_subscriptions(&sessions, &mut packet_clients)?;
        flush_packet_clients(&mut packet_clients);

        if sessions.is_empty() {
            let idle_started = idle_since.get_or_insert_with(Instant::now);
            if idle_started.elapsed() >= SESSION_DAEMON_IDLE_LINGER {
                break;
            }
        } else {
            idle_since = None;
        }

        if !did_work {
            wait_packet_output(&packet_clients);
        }
    }

    let _ = fs::remove_file(&socket_path);
    let _ = fs::remove_file(layout.daemon_pid_path());
    Ok(())
}

/// Contains both panics and errors from servicing one session: either way
/// the caller faults that session and the daemon keeps serving the others.
/// `None` means the session must be faulted.
fn contain_session_unwind<T, F>(id: &str, action: F) -> Option<T>
where
    F: FnOnce() -> Result<T, String>,
{
    match panic::catch_unwind(AssertUnwindSafe(action)) {
        Ok(Ok(result)) => Some(result),
        Ok(Err(err)) => {
            eprintln!("contained error while servicing session {id}: {err}");
            None
        }
        Err(payload) => {
            eprintln!("contained panic while servicing session {id}: {}", panic_payload_message(payload.as_ref()));
            None
        }
    }
}

fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

fn remove_packet_channels_for_session(session_id: &str, packet_clients: &mut [PacketClient]) {
    for client in packet_clients {
        // Tell the client each affected channel is gone before dropping it.
        // Directory deltas are selector-filtered, so an attached client may
        // never see the remove delta; the channel-scoped error is the only
        // guaranteed close signal. Enqueue failures mean the client transport
        // is already dead and its teardown path will reap it.
        let closed: Vec<u32> = client.channels.iter().filter(|(_, ch)| ch.session_id == session_id).map(|(id, _)| *id).collect();
        for channel in closed {
            let _ = client.enqueue_control(MSG_CONTROL_ERROR, &ControlError { channel, message: format!("session {session_id} exited") });
            client.channels.remove(&channel);
        }
    }
}

/// O(clients x channels) scan, run on every directory-entry build. Fine at
/// expected client counts; cache per-session counters if that ever grows.
fn packet_role_counts(session_id: &str, packet_clients: &[PacketClient]) -> (u32, u32) {
    let mut controllers = 0u32;
    let mut watchers = 0u32;
    for client in packet_clients {
        for session_channel in client.channels.values() {
            if session_channel.session_id == session_id {
                match session_channel.role {
                    ChannelRole::Controller => controllers = controllers.saturating_add(1),
                    ChannelRole::Watcher => watchers = watchers.saturating_add(1),
                }
            }
        }
    }
    (controllers, watchers)
}

fn packet_controller_identity(hosted: &HostedSession, packet_clients: &[PacketClient]) -> Option<AttachmentIdentity> {
    let holder = hosted.packet_control.exclusive().or_else(|| hosted.packet_control.controllers().next())?;
    packet_clients
        .iter()
        .find(|client| client.id == holder.client_id)
        .and_then(|client| client.channels.get(&holder.channel))
        .map(|channel| channel.identity.clone())
}

fn controller_identity(hosted: &HostedSession, packet_clients: &[PacketClient]) -> Option<AttachmentIdentity> {
    hosted.active_client.as_ref().map(|client| client.identity.clone()).or_else(|| packet_controller_identity(hosted, packet_clients))
}

fn sync_packet_geometry(hosted: &mut HostedSession) -> Result<(), String> {
    let focused = hosted.active_client.is_some() || hosted.packet_control.focused();
    if focused != hosted.focused {
        hosted.actor.request_result(|reply| crate::host::actor::SessionCommand::Focus { focused, reply })?;
        hosted.focused = focused;
    }
    if let Some(size) = hosted.packet_control.application_cell_size() {
        if size != hosted.applied_cell_size {
            hosted.actor.set_cell_size(size.0, size.1)?;
            hosted.applied_cell_size = size;
        }
    }
    if let Some(size) = hosted.fixed_size.or_else(|| hosted.packet_control.geometry()) {
        if size != hosted.applied_size {
            hosted.actor.resize(size.0, size.1)?;
            hosted.applied_size = size;
        }
    }
    Ok(())
}

fn sync_packet_controller_presence(layout: &RuntimeLayout, hosted: &HostedSession, previously_had_controller: bool) -> Result<(), String> {
    hosted.actor.retain_input_sources(hosted.packet_control.controllers().map(PacketChannelRef::view_id).collect())?;
    let has_controller = hosted.active_client.is_some() || hosted.packet_control.has_controllers();
    // Query authority follows the transport, not the number of drivers.
    // Also update on raw-to-packet takeover, when controller presence stays true.
    hosted.actor.set_query_passthrough(hosted.active_client.is_some())?;
    if has_controller == previously_had_controller {
        return Ok(());
    }
    if has_controller {
        let _ = fs::write(layout.foreground_path(&hosted.metadata.id), b"1");
        hosted.actor.record_attach()
    } else {
        let _ = fs::remove_file(layout.foreground_path(&hosted.metadata.id));
        hosted.actor.record_detach()
    }
}

fn packet_presence(hosted: &HostedSession, clients: &[PacketClient]) -> (Vec<crate::packet::Participant>, Option<AttachmentIdentity>) {
    let mut participants: Vec<_> = clients
        .iter()
        .filter(|client| !client.dead)
        .flat_map(|client| {
            client.channels.iter().filter(|(_, channel)| channel.session_id == hosted.metadata.id).map(move |(id, channel)| {
                crate::packet::Participant { connection: client.id, channel: *id, identity: channel.identity.clone(), role: channel.role }
            })
        })
        .collect();
    if let Some(client) = &hosted.active_client {
        participants.push(crate::packet::Participant {
            connection: 0,
            channel: 0,
            identity: client.identity.clone(),
            role: ChannelRole::Controller,
        });
    }
    for (index, client) in hosted.watchers.iter().enumerate() {
        participants.push(crate::packet::Participant {
            connection: 0,
            channel: index as u32 + 1,
            identity: client.identity.clone(),
            role: ChannelRole::Watcher,
        });
    }
    participants.sort_by_key(|p| (p.connection, p.channel));
    let exclusive = if hosted.active_client.is_some() || hosted.packet_control.exclusive().is_some() {
        controller_identity(hosted, clients)
    } else {
        None
    };
    (participants, exclusive)
}

fn announce_seat_state(hosted: &mut HostedSession, packet_clients: &mut [PacketClient]) -> Result<(), String> {
    announce_seat_state_except(hosted, packet_clients, None)
}

fn announce_seat_state_except(
    hosted: &mut HostedSession,
    packet_clients: &mut [PacketClient],
    excluded: Option<PacketChannelRef>,
) -> Result<(), String> {
    let controller = controller_identity(hosted, packet_clients);
    let (participants, exclusive) = packet_presence(hosted, packet_clients);
    let holder_kind = if hosted.active_client.is_some() { ControllerHolder::Stream } else { ControllerHolder::Packet };
    for watcher in &mut hosted.watchers {
        if watcher.denial_reason.is_some() {
            watcher.denial_reason = Some(RoleDenialReason { held_by: holder_kind });
        }
        watcher.enqueue_frame(&Frame::SeatState(SeatState { role: "watcher".to_string(), controller: controller.clone() }))?;
    }
    for client in packet_clients {
        let channels: Vec<(u32, ChannelRole, ChannelRole)> = client
            .channels
            .iter()
            .filter(|(_, channel)| channel.session_id == hosted.metadata.id)
            .map(|(id, channel)| (*id, channel.role, channel.requested_role))
            .collect();
        for (channel, role, requested_role) in channels {
            if excluded == Some(PacketChannelRef { client_id: client.id, channel }) {
                continue;
            }
            let denial_reason = (exclusive.is_some() && role == ChannelRole::Watcher && requested_role == ChannelRole::Controller)
                .then_some(RoleDenialReason { held_by: holder_kind });
            if let Some(session_channel) = client.channels.get_mut(&channel) {
                session_channel.denial_reason = denial_reason;
            }
            client.enqueue_frame(
                &PacketFrame::new(channel, MSG_SESSION_ROLE, &RoleState {
                    role,
                    controller: controller.clone(),
                    denial_reason,
                    participants: participants.clone(),
                    exclusive: exclusive.clone(),
                    fixed_size: hosted.fixed_size.map(|(cols, rows)| Resize { cols, rows }),
                })
                .map_err(|err| format!("encode seat state packet: {err}"))?,
            )?;
        }
    }
    Ok(())
}

fn inspect_hosted_session(hosted: &HostedSession, packet_clients: &[PacketClient]) -> Result<crate::protocol::InspectResult, String> {
    let mut result = hosted.actor.inspect(false, 0)?;
    if let Some(client) = hosted.active_client.as_ref() {
        result.attachments.push(crate::protocol::AttachmentInspect {
            role: "controller".to_string(),
            identity: client.identity.clone(),
            denial_reason: None,
        });
    }
    result.attachments.extend(hosted.watchers.iter().map(|client| crate::protocol::AttachmentInspect {
        role: "watcher".to_string(),
        identity: client.identity.clone(),
        denial_reason: client.denial_reason,
    }));
    for client in packet_clients {
        result.attachments.extend(client.channels.values().filter(|channel| channel.session_id == result.session.id).map(|channel| {
            crate::protocol::AttachmentInspect {
                role: match channel.role {
                    ChannelRole::Controller => "controller",
                    ChannelRole::Watcher => "watcher",
                }
                .to_string(),
                identity: channel.identity.clone(),
                denial_reason: channel.denial_reason,
            }
        }));
    }
    Ok(result)
}

fn directory_entry_for_session(
    layout: &RuntimeLayout,
    hosted: &HostedSession,
    packet_clients: &[PacketClient],
) -> Result<DirectoryEntry, String> {
    let inspect = inspect_hosted_session(hosted, packet_clients)?;
    let (packet_controllers, packet_watchers) = packet_role_counts(&inspect.session.id, packet_clients);
    Ok(DirectoryEntry {
        session_id: inspect.session.id.clone(),
        tags: inspect.session.tags,
        state: inspect.session.state,
        controller_count: (if hosted.active_client.is_some() { 1 } else { 0 }) + packet_controllers,
        watcher_count: u32::try_from(hosted.watchers.len()).unwrap_or(u32::MAX).saturating_add(packet_watchers),
        controller: controller_identity(hosted, packet_clients),
        recreatable: crate::recreate::session_is_recreatable(&layout.session_dir(&inspect.session.id)),
        cols: inspect.terminal.cols,
        rows: inspect.terminal.rows,
    })
}

fn directory_snapshot_for_sessions(
    layout: &RuntimeLayout,
    sessions: &HashMap<String, HostedSession>,
    packet_clients: &[PacketClient],
    selectors: &[String],
) -> Result<DirectorySnapshot, String> {
    let mut entries = Vec::new();
    for hosted in sessions.values() {
        let entry = directory_entry_for_session(layout, hosted, packet_clients)?;
        if directory_entry_matches_selectors(&entry, selectors) {
            entries.push(entry);
        }
    }
    entries.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    Ok(DirectorySnapshot { sessions: entries })
}

fn activity_snapshot_for_sessions(
    sessions: &HashMap<String, HostedSession>,
    selectors: &[String],
    stable_threshold_ms: u64,
) -> ActivitySnapshot {
    ActivitySnapshot { stable_threshold_ms, sessions: matching_activity_sessions(sessions, selectors, stable_threshold_ms) }
}

fn matching_activity_sessions(
    sessions: &HashMap<String, HostedSession>,
    selectors: &[String],
    stable_threshold_ms: u64,
) -> Vec<ActivitySession> {
    let mut activity_sessions = sessions
        .values()
        .filter(|hosted| selectors.iter().all(|selector| hosted.metadata.tags.contains(selector)))
        .map(|hosted| hosted.activity_session(stable_threshold_ms))
        .collect::<Vec<_>>();
    activity_sessions.sort_by(|left, right| left.session_id.cmp(&right.session_id));
    activity_sessions
}

fn service_activity_subscriptions(sessions: &HashMap<String, HostedSession>, packet_clients: &mut Vec<PacketClient>) -> Result<(), String> {
    for client in packet_clients {
        if client.dead {
            continue;
        }
        let Some(stable_threshold_ms) = client.screen_activity_stable_ms else {
            continue;
        };
        let current = matching_activity_sessions(sessions, &client.selectors, stable_threshold_ms);

        let current_ids = current.iter().map(|session| session.session_id.clone()).collect::<HashSet<_>>();
        for session in current {
            let prior = client.known_activity_sessions.get(&session.session_id);
            let added = prior.is_none();
            let changed = prior.is_some_and(|known| known.activity != session.activity);
            client.known_activity_sessions.insert(session.session_id.clone(), session.clone());
            if added {
                client.enqueue_control(MSG_CONTROL_ACTIVITY_EVENT, &ActivityEvent::MembershipAdded {
                    session,
                    changed_at_unix_ms: unix_time_ms(),
                })?;
            } else if changed {
                let changed_at_unix_ms = match session.activity {
                    ScreenActivity::Active => session.last_output_at_unix_ms.unwrap_or(session.stable_since_unix_ms),
                    ScreenActivity::Stable => session.stable_since_unix_ms.saturating_add(stable_threshold_ms),
                };
                client.enqueue_control(MSG_CONTROL_ACTIVITY_EVENT, &ActivityEvent::ActivityChanged { session, changed_at_unix_ms })?;
            }
        }

        let mut removed_ids =
            client.known_activity_sessions.keys().filter(|session_id| !current_ids.contains(*session_id)).cloned().collect::<Vec<_>>();
        removed_ids.sort();
        for session_id in removed_ids {
            let Some(removed) = client.known_activity_sessions.remove(&session_id) else {
                continue;
            };
            client.enqueue_control(MSG_CONTROL_ACTIVITY_EVENT, &ActivityEvent::MembershipRemoved {
                session_id,
                tags: removed.tags,
                changed_at_unix_ms: unix_time_ms(),
            })?;
        }
    }
    Ok(())
}

fn directory_entry_matches_selectors(entry: &DirectoryEntry, selectors: &[String]) -> bool {
    selectors.iter().all(|selector| entry.tags.contains(selector))
}

fn broadcast_directory_upsert(entry: DirectoryEntry, packet_clients: &mut Vec<PacketClient>) -> Result<(), String> {
    for client in packet_clients {
        if directory_entry_matches_selectors(&entry, &client.selectors) {
            client.known_directory_sessions.insert(entry.session_id.clone());
            client.enqueue_control(MSG_CONTROL_DIRECTORY_DELTA, &DirectoryDelta {
                upserted: vec![entry.clone()],
                removed_session_ids: Vec::new(),
            })?;
        } else if client.known_directory_sessions.remove(&entry.session_id) {
            client.enqueue_control(MSG_CONTROL_DIRECTORY_DELTA, &DirectoryDelta {
                upserted: Vec::new(),
                removed_session_ids: vec![entry.session_id.clone()],
            })?;
        }
    }
    Ok(())
}

fn broadcast_directory_remove(session_id: &str, packet_clients: &mut Vec<PacketClient>) -> Result<(), String> {
    for client in packet_clients {
        if client.known_directory_sessions.remove(session_id) {
            client.enqueue_control(MSG_CONTROL_DIRECTORY_DELTA, &DirectoryDelta {
                upserted: Vec::new(),
                removed_session_ids: vec![session_id.to_string()],
            })?;
        }
    }
    Ok(())
}

#[cfg(not(debug_assertions))]
fn maybe_panic_for_containment_test(_session_id: &str) {}

#[cfg(debug_assertions)]
fn maybe_panic_for_containment_test(session_id: &str) {
    if std::env::var("CLEAT_TEST_PANIC_SESSION_TICK").as_deref() == Ok(session_id) {
        panic!("test-requested panic for session {session_id}");
    }
}

#[cfg(not(debug_assertions))]
fn maybe_fail_after_http_upgrade(_route: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(debug_assertions)]
fn maybe_fail_after_http_upgrade(route: &str) -> Result<(), String> {
    if std::env::var("CLEAT_TEST_FAIL_AFTER_HTTP_UPGRADE").as_deref() == Ok(route) {
        return Err(format!("test-requested failure after {route} upgrade"));
    }
    Ok(())
}

fn cleanup_exited_session(layout: &RuntimeLayout, id: &str, should_keep_session_dir: bool) {
    let session_dir = layout.session_dir(id);
    let _ = fs::remove_file(layout.foreground_path(id));
    if !should_keep_session_dir {
        let _ = fs::remove_dir_all(&session_dir);
    }
}

fn service_hosted_session(
    layout: &RuntimeLayout,
    id: &str,
    hosted: &mut HostedSession,
    packet_clients: &mut Vec<PacketClient>,
) -> Result<bool, String> {
    let mut did_work = false;
    let previous_controller_count = hosted.active_client.is_some();
    let previous_watcher_count = hosted.watchers.len();
    let mut resized = false;

    let output_drained =
        drain_raw_output_tap(layout, id, &hosted.actor, &mut hosted.raw_output_tap, &mut hosted.active_client, &mut hosted.watchers)?;
    did_work |= output_drained;
    drain_watcher_inputs(&mut hosted.watchers);

    if hosted.active_client.is_some() {
        let mut client_disconnected = false;
        let mut pending = VecDeque::new();
        if let Some(client) = hosted.active_client.as_mut() {
            match client.drain_input_frames(&mut pending, Duration::ZERO) {
                Ok(true) => {}
                Ok(false) => client_disconnected = true,
                Err(err) => return Err(format!("read client frame: {err}")),
            }
        }

        coalesce_resize_bursts(&mut pending);

        while let Some(frame) = pending.pop_front() {
            did_work = true;
            match frame {
                Frame::Input(bytes) => {
                    hosted.actor.write_input(bytes)?;
                }
                Frame::Resize { cols, rows } => {
                    let size = hosted.fixed_size.unwrap_or((cols, rows));
                    hosted.actor.resize(size.0, size.1)?;
                    hosted.applied_size = size;
                    resized = true;
                }
                _ => {}
            }
        }

        if client_disconnected && hosted.active_client.is_some() {
            let _ = fs::remove_file(layout.foreground_path(id));
            hosted.actor.record_detach()?;
            hosted.actor.set_query_passthrough(false)?;
            hosted.active_client = None;
            did_work = true;
        }
    }

    let client_writable = match hosted.active_client.as_mut() {
        Some(client) => client.flush_pending_output()?,
        None => true,
    };
    if !client_writable {
        let _ = fs::remove_file(layout.foreground_path(id));
        hosted.actor.record_detach()?;
        hosted.actor.set_query_passthrough(false)?;
        hosted.active_client = None;
        did_work = true;
    }
    flush_watchers(&mut hosted.watchers);
    push_due_packet_renders(id, &hosted.actor, packet_clients, &mut hosted.packet_render_cache)?;
    if output_drained {
        hosted.actor.enqueue_screen_activity_flush()?;
    }
    if resized || previous_controller_count != hosted.active_client.is_some() || previous_watcher_count != hosted.watchers.len() {
        announce_seat_state(hosted, packet_clients)?;
        broadcast_directory_upsert(directory_entry_for_session(layout, hosted, packet_clients)?, packet_clients)?;
    }

    service_pending_waits(&hosted.actor, &mut hosted.pending_waits, &mut hosted.screen_stable_snapshot_generation);
    service_pending_expects(layout, id, &hosted.actor, &mut hosted.pending_expects)?;
    // Recording flush happens actor-side after each pump slice; no per-tick
    // round-trip here (ADR 0004: the servicing side is never blocked).

    Ok(did_work)
}

/// A terminal drag generates a run of resize frames, but only the final size
/// is meaningful until an input frame establishes an ordering boundary. Each
/// resize synchronously pumps the PTY actor, so applying every intermediate
/// size can fill the raw-output tap before the daemon returns to drain it.
fn coalesce_resize_bursts(pending: &mut VecDeque<Frame>) {
    let mut coalesced = VecDeque::with_capacity(pending.len());
    let mut latest_resize = None;

    while let Some(frame) = pending.pop_front() {
        match frame {
            Frame::Resize { .. } => latest_resize = Some(frame),
            frame => {
                if let Some(resize) = latest_resize.take() {
                    coalesced.push_back(resize);
                }
                coalesced.push_back(frame);
            }
        }
    }
    if let Some(resize) = latest_resize {
        coalesced.push_back(resize);
    }
    *pending = coalesced;
}

/// Whether a screen-stable wait needs a fresh full-grid fingerprint. An
/// unchanged render generation means the actor has fed nothing to the engine
/// since the last fingerprint, so the screen cannot have changed and the
/// existing stability window simply keeps aging without a snapshot.
fn screen_stable_needs_snapshot(current_generation: u64, last_snapshot_generation: Option<u64>) -> bool {
    last_snapshot_generation != Some(current_generation)
}

fn service_pending_waits(actor: &SessionActor, pending_waits: &mut Vec<PendingWait>, last_snapshot_generation: &mut Option<u64>) {
    let screen_stable_fingerprint = if pending_waits.iter().any(|wait| wait.screen_stable.is_some()) {
        // Read the generation before snapshotting: a pump landing in between
        // costs one redundant snapshot next tick instead of a missed change.
        let generation = actor.observation().render_generation();
        if screen_stable_needs_snapshot(generation, *last_snapshot_generation) {
            let fingerprint = actor.full_snapshot().ok().map(ScreenStableFingerprint::from_snapshot);
            if fingerprint.is_some() {
                *last_snapshot_generation = Some(generation);
            }
            fingerprint
        } else {
            None
        }
    } else {
        *last_snapshot_generation = None;
        None
    };
    let now = Instant::now();
    pending_waits.retain_mut(|wait| {
        let elapsed = wait.registered_at.elapsed();
        let elapsed_ms = elapsed.as_millis() as u64;

        if elapsed_ms >= wait.timeout_ms {
            let _ = write_http_wait_result(&mut wait.stream, crate::protocol::WaitStatus::Timeout, elapsed_ms);
            return false;
        }

        if let (Some(state), Some(fingerprint)) = (wait.screen_stable.as_mut(), screen_stable_fingerprint.as_ref()) {
            state.observe(fingerprint.clone(), now);
        }

        for condition in &wait.conditions {
            match condition {
                crate::protocol::WaitCondition::OutputIdle { quiet_ms } => {
                    let silence_since = match actor.last_pty_output_at().ok().flatten() {
                        Some(t) if t > wait.registered_at => t,
                        _ => wait.registered_at,
                    };
                    let quiet_duration = silence_since.elapsed().as_millis() as u64;
                    if quiet_duration >= *quiet_ms {
                        let _ = write_http_wait_result(&mut wait.stream, crate::protocol::WaitStatus::Ready, elapsed_ms);
                        return false;
                    }
                }
                crate::protocol::WaitCondition::TextMatch { text } => {
                    if actor.screen_contains(text.clone()).unwrap_or(false) {
                        let _ = write_http_wait_result(&mut wait.stream, crate::protocol::WaitStatus::Ready, elapsed_ms);
                        return false;
                    }
                }
                crate::protocol::WaitCondition::ScreenStable { stable_ms } => {
                    if let Some(state) = wait.screen_stable.as_ref() {
                        let stable_duration_ms = state.stable_since.elapsed().as_millis() as u64;
                        if stable_duration_ms >= *stable_ms {
                            let _ = write_http_wait_result(&mut wait.stream, crate::protocol::WaitStatus::Ready, elapsed_ms);
                            return false;
                        }
                    }
                }
            }
        }

        true
    });
}

fn service_pending_expects(
    layout: &RuntimeLayout,
    id: &str,
    actor: &SessionActor,
    pending_expects: &mut Vec<PendingExpect>,
) -> Result<(), String> {
    if pending_expects.is_empty() {
        return Ok(());
    }

    actor.flush_recording()?;
    let cast_path = layout.session_dir(id).join(crate::recording::CAST_FILE_NAME);
    pending_expects.retain_mut(|expect| {
        let elapsed = expect.registered_at.elapsed();
        let elapsed_ms = elapsed.as_millis() as u64;

        if elapsed_ms >= expect.timeout_ms {
            let _ = write_http_wait_result(&mut expect.stream, crate::protocol::WaitStatus::Timeout, elapsed_ms);
            return false;
        }

        if cast_path.exists() {
            let file_size = std::fs::metadata(&cast_path).map(|m| m.len()).unwrap_or(0);
            if file_size > expect.last_checked_file_size {
                expect.last_checked_file_size = file_size;
                if let Ok(events) = crate::cast_reader::read_output_since(&cast_path, expect.since_offset) {
                    let output: String = events.iter().map(|e| e.data.as_str()).collect();
                    if output.contains(&expect.text) {
                        let _ = write_http_wait_result(&mut expect.stream, crate::protocol::WaitStatus::Ready, elapsed_ms);
                        return false;
                    }
                }
            }
        }

        true
    });
    Ok(())
}

fn finish_exited_session(
    layout: &RuntimeLayout,
    id: &str,
    hosted: &mut HostedSession,
    packet_clients: &mut [PacketClient],
) -> Result<bool, String> {
    drain_raw_output_tap(layout, id, &hosted.actor, &mut hosted.raw_output_tap, &mut hosted.active_client, &mut hosted.watchers)?;
    for mut wait in hosted.pending_waits.drain(..) {
        let elapsed_ms = wait.registered_at.elapsed().as_millis() as u64;
        let _ = write_http_wait_result(&mut wait.stream, crate::protocol::WaitStatus::SessionGone, elapsed_ms);
    }
    for mut expect in hosted.pending_expects.drain(..) {
        let elapsed_ms = expect.registered_at.elapsed().as_millis() as u64;
        let _ = write_http_wait_result(&mut expect.stream, crate::protocol::WaitStatus::SessionGone, elapsed_ms);
    }
    if let Some(client) = hosted.active_client.as_mut() {
        let _ = client.flush_pending_output();
    }
    flush_watchers(&mut hosted.watchers);
    flush_packet_clients(packet_clients);
    Ok(hosted.actor.should_keep_session_dir().unwrap_or(hosted.should_keep_session_dir))
}

#[cfg(not(any(unix, windows)))]
pub fn run_session_daemon(_root: &Path, _session: &SessionMetadata) -> Result<(), String> {
    Err("session daemon is only supported on unix".into())
}

struct HttpRequestState<'a> {
    layout: &'a RuntimeLayout,
    sessions: &'a mut HashMap<String, HostedSession>,
    packet_clients: &'a mut Vec<PacketClient>,
    next_packet_client_id: &'a mut u64,
}

#[cfg(any(unix, windows))]
struct PendingHttpHandshake {
    stream: SessionStream,
    buffer: Vec<u8>,
    deadline: Instant,
    #[cfg(windows)]
    reader: crate::platform::ipc::OverlappedRead,
}

enum HttpHandshakePoll {
    Pending(bool),
    Ready(Box<http_uds::HttpRequest>),
}

#[cfg(any(unix, windows))]
impl PendingHttpHandshake {
    fn new(stream: SessionStream) -> Result<Self, String> {
        set_stream_nonblocking(&stream, true)?;
        set_stream_write_timeout(&stream, Some(SESSION_HTTP_RESPONSE_WRITE_DEADLINE))?;
        #[cfg(windows)]
        let reader = stream.overlapped_reader(8192).map_err(|err| format!("create HTTP handshake reader: {err}"))?;
        Ok(Self {
            stream,
            buffer: Vec::new(),
            deadline: Instant::now() + SESSION_HTTP_HANDSHAKE_DEADLINE,
            #[cfg(windows)]
            reader,
        })
    }

    fn poll(&mut self) -> io::Result<HttpHandshakePoll> {
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "HTTP request handshake deadline exceeded"));
        }

        let mut did_work = false;
        loop {
            if self.buffer.len() >= 5 && !http_uds::looks_like_http_prefix(&self.buffer[..5]) {
                return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "connection did not start with HTTP"));
            }
            if let Some(request) = http_uds::try_parse_request(&self.buffer)? {
                return Ok(HttpHandshakePoll::Ready(Box::new(request)));
            }

            let Some(chunk) = self.read_available()? else {
                return Ok(HttpHandshakePoll::Pending(did_work));
            };
            if chunk.is_empty() {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed before HTTP request completed"));
            }
            self.buffer.extend_from_slice(&chunk);
            did_work = true;
        }
    }

    #[cfg(unix)]
    fn read_available(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut chunk = vec![0; 8192];
        match retry_interrupted(|| self.stream.read(&mut chunk)) {
            Ok(read) => {
                chunk.truncate(read);
                Ok(Some(chunk))
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(err) => Err(err),
        }
    }

    #[cfg(windows)]
    fn read_available(&mut self) -> io::Result<Option<Vec<u8>>> {
        self.reader.poll()
    }

    fn into_stream(self) -> SessionStream {
        self.stream
    }
}

fn handle_http_request(
    _root: &Path,
    daemon_id: &str,
    stream: &mut SessionStream,
    request: http_uds::HttpRequest,
    state: &mut HttpRequestState<'_>,
    response_committed: &mut bool,
) -> Result<(), String> {
    match http_uds::route(&request) {
        http_uds::Route::Root | http_uds::Route::Health => http_uds::write_json(
            stream,
            StatusCode::OK,
            &serde_json::json!({
                "service": "cleat-session",
                "build": crate::build_info::BuildInfo::current(),
                "session": daemon_id,
                "ok": true,
            }),
        )
        .map_err(|err| format!("write HTTP response: {err}")),
        http_uds::Route::Sessions => {
            let mut sessions = Vec::new();
            for hosted in state.sessions.values() {
                sessions.push(inspect_hosted_session(hosted, state.packet_clients)?);
            }
            sessions.sort_by(|a, b| a.session.id.cmp(&b.session.id));
            http_uds::write_json(stream, StatusCode::OK, &http_uds::SessionListResponse { sessions })
                .map_err(|err| format!("write HTTP sessions response: {err}"))
        }
        http_uds::Route::SessionCreate => {
            if let Some(expected_pid) = request.headers().get(http_uds::DAEMON_INSTANCE_HEADER) {
                let Some(expected_pid) = expected_pid.to_str().ok().and_then(|value| value.parse::<u32>().ok()) else {
                    return http_uds::write_error(stream, StatusCode::BAD_REQUEST, "invalid daemon instance header")
                        .map_err(|err| format!("write HTTP error response: {err}"));
                };
                if expected_pid != std::process::id() {
                    return http_uds::write_error(stream, StatusCode::CONFLICT, "source daemon instance changed")
                        .map_err(|err| format!("write HTTP error response: {err}"));
                }
            }
            let session: SessionMetadata =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP session create request: {err}"))?;
            crate::runtime::validate_runtime_name(&session.id)?;
            crate::runtime::validate_environment(&session.environment)?;
            session.vt_engine.ensure_available()?;
            let mut created = false;
            if !state.sessions.contains_key(&session.id) {
                let session_dir = state.layout.session_dir(&session.id);
                fs::create_dir_all(&session_dir).map_err(|err| format!("create session dir {}: {err}", session_dir.display()))?;
                let coordinates = state.layout.session_coordinates(&session.id)?;
                let hosted = HostedSession::spawn(session_dir, session.clone(), coordinates)?;
                state.sessions.insert(session.id.clone(), hosted);
                created = true;
            }
            if created {
                if let Some(hosted) = state.sessions.get(&session.id) {
                    broadcast_directory_upsert(
                        directory_entry_for_session(state.layout, hosted, state.packet_clients)?,
                        state.packet_clients,
                    )?;
                }
            }
            let session =
                state.sessions.get(&session.id).ok_or_else(|| "created session disappeared before response".to_string())?.metadata.clone();
            http_uds::write_json(stream, StatusCode::OK, &http_uds::CreateSessionResponse { session })
                .map_err(|err| format!("write HTTP session create response: {err}"))
        }
        http_uds::Route::PacketConnect => {
            if !http_uds::request_has_upgrade_token(&request, "cleat-packet/1") {
                http_uds::write_error(stream, StatusCode::BAD_REQUEST, "missing Upgrade: cleat-packet/1")
                    .map_err(|err| format!("write HTTP packet upgrade error: {err}"))?;
                return Ok(());
            }

            let subscribe = if request.body().is_empty() {
                http_uds::PacketSubscribeRequest::default()
            } else {
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP directory subscribe request: {err}"))?
            };
            if subscribe.screen_activity_stable_ms == Some(0) {
                http_uds::write_error(stream, StatusCode::BAD_REQUEST, "screen activity stability threshold must be greater than zero")
                    .map_err(|err| format!("write HTTP activity subscribe error: {err}"))?;
                return Ok(());
            }
            let mut selectors = subscribe.selectors;
            crate::runtime::normalize_tags(&mut selectors);
            let directory = directory_snapshot_for_sessions(state.layout, state.sessions, state.packet_clients, &selectors)?;
            let activity = subscribe
                .screen_activity_stable_ms
                .map(|stable_threshold_ms| activity_snapshot_for_sessions(state.sessions, &selectors, stable_threshold_ms));
            let packet_stream = stream.try_clone().map_err(|err| format!("clone HTTP packet stream: {err}"))?;
            #[cfg(unix)]
            set_stream_nonblocking(&packet_stream, true).map_err(|err| format!("set HTTP packet stream nonblocking: {err}"))?;
            let client_id = *state.next_packet_client_id;
            let mut client =
                PacketClient::new(client_id, packet_stream, selectors, subscribe.screen_activity_stable_ms, &directory, activity.as_ref())?;
            client.enqueue_control(MSG_CONTROL_HELLO, &ControlHello::current())?;
            client.enqueue_control(MSG_CONTROL_DIRECTORY_SNAPSHOT, &directory)?;
            if let Some(activity) = activity {
                client.enqueue_control(MSG_CONTROL_ACTIVITY_SNAPSHOT, &activity)?;
            }
            *response_committed = true;
            http_uds::write_packet_switching_protocols(stream).map_err(|err| format!("write HTTP packet upgrade response: {err}"))?;
            maybe_fail_after_http_upgrade("packet")?;
            *state.next_packet_client_id += 1;
            state.packet_clients.push(client);
            Ok(())
        }
        http_uds::Route::SessionInspect { id } => {
            let Some(hosted) = state.sessions.get(&id) else {
                return write_http_not_found(stream);
            };
            let result = inspect_hosted_session(hosted, state.packet_clients)?;
            http_uds::write_json(stream, StatusCode::OK, &result).map_err(|err| format!("write HTTP inspect response: {err}"))
        }
        http_uds::Route::SessionDelete { id } => {
            let Some(hosted) = state.sessions.get(&id) else {
                return write_http_not_found(stream);
            };
            hosted.actor.dispatch_signal(TERMINATE_SIGNAL, crate::protocol::SignalTarget::Tree)?;
            http_uds::write_no_content(stream).map_err(|err| format!("write HTTP delete response: {err}"))
        }
        http_uds::Route::SessionAttach { id } => 'attach: {
            let Some(hosted) = state.sessions.get_mut(&id) else {
                break 'attach write_http_not_found(stream);
            };
            let body: http_uds::AttachRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP attach request: {err}"))?;
            vacate_dead_packet_controller(hosted, state.packet_clients);
            let seat_is_held = hosted.active_client.is_some() || hosted.packet_control.has_controllers();
            if seat_is_held && body.strict {
                let holder = controller_identity(hosted, state.packet_clients)
                    .map(|identity| format!("{} ({})", identity.name, identity.kind.as_str()))
                    .unwrap_or_else(|| "unknown".to_string());
                http_uds::write_error(stream, StatusCode::CONFLICT, &format!("session {id} controller seat is held by {holder}"))
                    .map_err(|err| format!("write HTTP attach busy response: {err}"))?;
                break 'attach Ok(());
            }
            if seat_is_held && body.take {
                if let Some(controller) = hosted.active_client.take() {
                    let mut controller = controller;
                    controller.denial_reason = Some(RoleDenialReason { held_by: ControllerHolder::Stream });
                    hosted.watchers.push(controller);
                }
                hosted.packet_control.demote_all();
                hosted.actor.retain_input_sources(vec![])?;
                for client in state.packet_clients.iter_mut() {
                    for channel in client.channels.values_mut().filter(|c| c.session_id == id) {
                        channel.role = ChannelRole::Watcher;
                        channel.denial_reason = Some(RoleDenialReason { held_by: ControllerHolder::Stream });
                    }
                }
            }
            let grant_controller = !seat_is_held || body.take;

            let capabilities = attach_capabilities_from_http(body.capabilities);
            let replay = if grant_controller {
                let size = hosted.fixed_size.unwrap_or((body.cols, body.rows));
                let replay = hosted.actor.apply_attach_state(size.0, size.1, capabilities)?;
                hosted.applied_size = size;
                replay
            } else {
                hosted.actor.replay_payload(capabilities)?
            };
            let attach_stream = stream.try_clone().map_err(|err| format!("clone HTTP attach stream: {err}"))?;
            let mut client = ActiveClient::new(attach_stream, capabilities, normalize_attachment_identity(body.identity))?;
            if !grant_controller {
                client.denial_reason = Some(RoleDenialReason {
                    held_by: if hosted.active_client.is_some() { ControllerHolder::Stream } else { ControllerHolder::Packet },
                });
            }
            let replay_mode = if hosted.had_foreground_client { ReplayMode::ResetTerminal } else { ReplayMode::FreshTerminal };
            let role = if grant_controller { "controller" } else { "watcher" };
            let seat_controller =
                if grant_controller { Some(client.identity.clone()) } else { controller_identity(hosted, state.packet_clients) };
            if !grant_controller {
                client.enqueue_frame(&Frame::SeatState(SeatState { role: role.to_string(), controller: seat_controller }))?;
            }
            drain_raw_output_tap_before_client_install(state.layout, &id, hosted, &mut client, replay, replay_mode)?;
            *response_committed = true;
            http_uds::write_switching_protocols(stream).map_err(|err| format!("write HTTP attach upgrade response: {err}"))?;
            maybe_fail_after_http_upgrade("attach")?;
            #[cfg(unix)]
            set_stream_nonblocking(&client.stream, true).map_err(|err| format!("set HTTP attach stream nonblocking: {err}"))?;
            if grant_controller {
                let _ = fs::write(state.layout.foreground_path(&id), b"1");
                hosted.active_client = Some(client);
            } else {
                hosted.watchers.push(client);
            }
            let activation = (|| {
                if grant_controller {
                    hosted.actor.set_query_passthrough(true)?;
                    hosted.actor.record_attach()?;
                }
                announce_seat_state(hosted, state.packet_clients)?;
                broadcast_directory_upsert(directory_entry_for_session(state.layout, hosted, state.packet_clients)?, state.packet_clients)
            })();
            if let Err(err) = activation {
                if grant_controller {
                    hosted.active_client = None;
                    let _ = hosted.actor.set_query_passthrough(false);
                    let _ = fs::remove_file(state.layout.foreground_path(&id));
                } else {
                    hosted.watchers.pop();
                }
                return Err(err);
            }
            hosted.had_foreground_client |= grant_controller;
            Ok(())
        }
        http_uds::Route::SessionWatch { id } => {
            let Some(hosted) = state.sessions.get_mut(&id) else {
                return write_http_not_found(stream);
            };
            let body: http_uds::AttachRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP watch request: {err}"))?;
            let capabilities = attach_capabilities_from_http(body.capabilities);
            let replay = hosted.actor.replay_payload(capabilities)?;
            let watch_stream = stream.try_clone().map_err(|err| format!("clone HTTP watch stream: {err}"))?;
            let mut watcher = ActiveClient::new(watch_stream, capabilities, normalize_attachment_identity(body.identity))?;
            watcher.enqueue_frame(&Frame::SeatState(SeatState {
                role: "watcher".to_string(),
                controller: controller_identity(hosted, state.packet_clients),
            }))?;
            drain_raw_output_tap_before_client_install(state.layout, &id, hosted, &mut watcher, replay, ReplayMode::FreshTerminal)?;
            *response_committed = true;
            http_uds::write_switching_protocols(stream).map_err(|err| format!("write HTTP watch upgrade response: {err}"))?;
            maybe_fail_after_http_upgrade("watch")?;
            #[cfg(unix)]
            set_stream_nonblocking(&watcher.stream, true).map_err(|err| format!("set HTTP watch stream nonblocking: {err}"))?;
            hosted.watchers.push(watcher);
            let activation = (|| {
                announce_seat_state(hosted, state.packet_clients)?;
                broadcast_directory_upsert(directory_entry_for_session(state.layout, hosted, state.packet_clients)?, state.packet_clients)
            })();
            if let Err(err) = activation {
                hosted.watchers.pop();
                return Err(err);
            }
            Ok(())
        }
        http_uds::Route::SessionDetach { id } => {
            let Some(hosted) = state.sessions.get_mut(&id) else {
                return write_http_not_found(stream);
            };
            let _ = fs::remove_file(state.layout.foreground_path(&id));
            if hosted.active_client.is_some() {
                hosted.actor.record_detach()?;
            }
            let controllers: Vec<_> = hosted.packet_control.controllers().collect();
            for controller in controllers {
                hosted.packet_control.remove(controller);
                hosted.actor.release_input(controller.view_id())?;
                hosted.actor.release_attachment_view(controller.view_id());
                if let Some(client) = state.packet_clients.iter_mut().find(|client| client.id == controller.client_id) {
                    client.enqueue_control(MSG_CONTROL_ERROR, &ControlError {
                        channel: controller.channel,
                        message: format!("session {id} detached"),
                    })?;
                    client.channels.remove(&controller.channel);
                }
            }
            hosted.actor.set_query_passthrough(false)?;
            hosted.active_client = None;
            announce_seat_state(hosted, state.packet_clients)?;
            broadcast_directory_upsert(directory_entry_for_session(state.layout, hosted, state.packet_clients)?, state.packet_clients)?;
            http_uds::write_no_content(stream).map_err(|err| format!("write HTTP detach response: {err}"))
        }
        http_uds::Route::SessionExpect { id } => 'expect: {
            let Some(hosted) = state.sessions.get_mut(&id) else {
                break 'expect write_http_not_found(stream);
            };
            let body: http_uds::ExpectRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP expect request: {err}"))?;
            if !hosted.actor.recording_active()? {
                http_uds::write_error(stream, StatusCode::CONFLICT, "recording not active")
                    .map_err(|err| format!("write HTTP expect error: {err}"))?;
                break 'expect Ok(());
            }

            let cast_path = state.layout.session_dir(&id).join(crate::recording::CAST_FILE_NAME);
            hosted.actor.flush_recording()?;
            if cast_path.exists() {
                if let Ok(events) = crate::cast_reader::read_output_since(&cast_path, body.since_offset) {
                    let output: String = events.iter().map(|event| event.data.as_str()).collect();
                    if output.contains(&body.text) {
                        http_uds::write_json(stream, StatusCode::OK, &http_uds::WaitResultResponse {
                            status: http_uds::WaitStatusResponse::Ready,
                            elapsed_ms: 0,
                        })
                        .map_err(|err| format!("write HTTP expect response: {err}"))?;
                        break 'expect Ok(());
                    }
                }
            }

            let pending_stream = stream.try_clone().map_err(|err| format!("clone HTTP expect stream: {err}"))?;
            if let Err(err) = set_stream_nonblocking(&pending_stream, true) {
                http_uds::write_error(stream, StatusCode::INTERNAL_SERVER_ERROR, &format!("set nonblocking: {err}"))
                    .map_err(|err| format!("write HTTP expect error: {err}"))?;
                break 'expect Ok(());
            }
            let initial_file_size = std::fs::metadata(&cast_path).map(|metadata| metadata.len()).unwrap_or(0);
            hosted.pending_expects.push(PendingExpect {
                stream: pending_stream,
                text: body.text,
                since_offset: body.since_offset,
                last_checked_file_size: initial_file_size,
                timeout_ms: body.timeout_ms,
                registered_at: Instant::now(),
            });
            Ok(())
        }
        http_uds::Route::SessionInput { id } => {
            let Some(hosted) = state.sessions.get_mut(&id) else {
                return write_http_not_found(stream);
            };
            let body: http_uds::InputRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP input request: {err}"))?;
            match body {
                http_uds::InputRequest::Text { text } => {
                    hosted.actor.write_input(text.into_bytes())?;
                }
                http_uds::InputRequest::Paste { text } => {
                    hosted.actor.paste(text.into_bytes())?;
                }
                http_uds::InputRequest::Key { key } => {
                    let bytes = http_input_key_bytes(key);
                    hosted.actor.write_input(bytes)?;
                }
                http_uds::InputRequest::RawBytes { bytes } => {
                    hosted.actor.write_input(bytes)?;
                }
                http_uds::InputRequest::Resize { cols, rows } => {
                    hosted.actor.resize(cols, rows)?;
                    broadcast_directory_upsert(
                        directory_entry_for_session(state.layout, hosted, state.packet_clients)?,
                        state.packet_clients,
                    )?;
                }
            }
            http_uds::write_no_content(stream).map_err(|err| format!("write HTTP input response: {err}"))
        }
        http_uds::Route::SessionKeys { id } => {
            let Some(hosted) = state.sessions.get(&id) else {
                return write_http_not_found(stream);
            };
            let body: http_uds::KeysRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP keys request: {err}"))?;
            hosted.actor.write_input(body.bytes)?;
            http_uds::write_no_content(stream).map_err(|err| format!("write HTTP keys response: {err}"))
        }
        http_uds::Route::SessionKeysWithMark { id } => {
            let Some(hosted) = state.sessions.get(&id) else {
                return write_http_not_found(stream);
            };
            let body: http_uds::KeysWithMarkRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP keys-with-mark request: {err}"))?;
            let offset = hosted.actor.write_input_with_mark(body.bytes, body.marker_name)?;
            http_uds::write_json(stream, StatusCode::OK, &http_uds::MarkResponse { offset })
                .map_err(|err| format!("write HTTP keys-with-mark response: {err}"))
        }
        http_uds::Route::SessionPasteWithMark { id } => {
            let Some(hosted) = state.sessions.get(&id) else {
                return write_http_not_found(stream);
            };
            let body: http_uds::PasteWithMarkRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP paste-with-mark request: {err}"))?;
            let offset = hosted.actor.paste_with_mark(body.text.into_bytes(), body.marker_name)?;
            http_uds::write_json(stream, StatusCode::OK, &http_uds::MarkResponse { offset })
                .map_err(|err| format!("write HTTP paste-with-mark response: {err}"))
        }
        http_uds::Route::SessionRecord { id } => {
            let Some(hosted) = state.sessions.get_mut(&id) else {
                return write_http_not_found(stream);
            };
            let body: http_uds::RecordRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP record request: {err}"))?;
            hosted.actor.set_recording(body.enable)?;
            hosted.metadata.record = body.enable;
            http_uds::write_no_content(stream).map_err(|err| format!("write HTTP record response: {err}"))
        }
        http_uds::Route::SessionTags { id } => {
            let Some(hosted) = state.sessions.get_mut(&id) else {
                return write_http_not_found(stream);
            };
            let body: http_uds::TagRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP tag request: {err}"))?;
            let tags = hosted.actor.update_tags(body.add, body.remove)?;
            hosted.metadata.tags.clone_from(&tags);
            broadcast_directory_upsert(directory_entry_for_session(state.layout, hosted, state.packet_clients)?, state.packet_clients)?;
            http_uds::write_json(stream, StatusCode::OK, &http_uds::TagResponse { tags })
                .map_err(|err| format!("write HTTP tag response: {err}"))
        }
        http_uds::Route::SessionMark { id } => {
            let Some(hosted) = state.sessions.get(&id) else {
                return write_http_not_found(stream);
            };
            let body: http_uds::MarkRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP mark request: {err}"))?;
            let offset = hosted.actor.mark(body.name)?;
            http_uds::write_json(stream, StatusCode::OK, &http_uds::MarkResponse { offset })
                .map_err(|err| format!("write HTTP mark response: {err}"))
        }
        http_uds::Route::SessionResolveMarker { id } => {
            let Some(hosted) = state.sessions.get(&id) else {
                return write_http_not_found(stream);
            };
            let body: http_uds::ResolveMarkerRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP resolve-marker request: {err}"))?;
            let marker_name = body.name;
            match hosted.actor.resolve_marker(marker_name.clone())? {
                Some(offset) => http_uds::write_json(stream, StatusCode::OK, &http_uds::MarkResponse { offset })
                    .map_err(|err| format!("write HTTP resolve-marker response: {err}")),
                None => http_uds::write_error(stream, StatusCode::NOT_FOUND, &format!("marker not found: {marker_name}"))
                    .map_err(|err| format!("write HTTP resolve-marker error: {err}")),
            }
        }
        http_uds::Route::SessionResolveNextMarker { id } => {
            let Some(hosted) = state.sessions.get(&id) else {
                return write_http_not_found(stream);
            };
            let body: http_uds::ResolveNextMarkerRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP resolve-next-marker request: {err}"))?;
            let offset = hosted.actor.resolve_next_marker_after(body.after)?;
            http_uds::write_json(stream, StatusCode::OK, &http_uds::ResolveNextMarkerResponse { offset })
                .map_err(|err| format!("write HTTP resolve-next-marker response: {err}"))
        }
        http_uds::Route::SessionResize { id } => {
            let Some(hosted) = state.sessions.get_mut(&id) else {
                return write_http_not_found(stream);
            };
            let body: http_uds::ResizeRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP resize request: {err}"))?;
            hosted.fixed_size = Some((body.cols.max(1), body.rows.max(1)));
            sync_packet_geometry(hosted)?;
            announce_seat_state(hosted, state.packet_clients)?;
            broadcast_directory_upsert(directory_entry_for_session(state.layout, hosted, state.packet_clients)?, state.packet_clients)?;
            http_uds::write_no_content(stream).map_err(|err| format!("write HTTP resize response: {err}"))
        }
        http_uds::Route::SessionScreen { id } => {
            let Some(hosted) = state.sessions.get(&id) else {
                return write_http_not_found(stream);
            };
            match hosted.actor.capture_text() {
                Ok(text) => http_uds::write_json(stream, StatusCode::OK, &http_uds::ScreenResponse { text })
                    .map_err(|err| format!("write HTTP screen response: {err}")),
                Err(err) => {
                    http_uds::write_error(stream, StatusCode::CONFLICT, &err).map_err(|err| format!("write HTTP screen error: {err}"))
                }
            }
        }
        http_uds::Route::SessionSignal { id } => {
            let Some(hosted) = state.sessions.get(&id) else {
                return write_http_not_found(stream);
            };
            let body: http_uds::SignalRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP signal request: {err}"))?;
            hosted.actor.dispatch_signal(body.signal, signal_target_from_http(body.target))?;
            http_uds::write_no_content(stream).map_err(|err| format!("write HTTP signal response: {err}"))
        }
        http_uds::Route::SessionSnapshot { id } => {
            let Some(hosted) = state.sessions.get(&id) else {
                return write_http_not_found(stream);
            };
            match hosted.actor.full_snapshot() {
                Ok(snapshot) => http_uds::write_json(stream, StatusCode::OK, &http_uds::snapshot_response(snapshot))
                    .map_err(|err| format!("write HTTP snapshot response: {err}")),
                Err(err) => {
                    http_uds::write_error(stream, StatusCode::CONFLICT, &err).map_err(|err| format!("write HTTP snapshot error: {err}"))
                }
            }
        }
        http_uds::Route::SessionWait { id } => 'wait: {
            let Some(hosted) = state.sessions.get_mut(&id) else {
                break 'wait write_http_not_found(stream);
            };
            let body: http_uds::WaitRequest =
                serde_json::from_slice(request.body()).map_err(|err| format!("parse HTTP wait request: {err}"))?;
            let conditions: Vec<_> = body.conditions.into_iter().map(wait_condition_from_http).collect();
            if conditions.is_empty() {
                http_uds::write_error(stream, StatusCode::BAD_REQUEST, "at least one wait condition is required")
                    .map_err(|err| format!("write HTTP wait error: {err}"))?;
                break 'wait Ok(());
            }

            let has_text_match = conditions.iter().any(|condition| matches!(condition, crate::protocol::WaitCondition::TextMatch { .. }));
            let has_screen_stable =
                conditions.iter().any(|condition| matches!(condition, crate::protocol::WaitCondition::ScreenStable { .. }));
            if has_text_match {
                if let Err(err) = hosted.actor.validate_text_matching() {
                    http_uds::write_error(stream, StatusCode::CONFLICT, &format!("text matching not supported: {err}"))
                        .map_err(|err| format!("write HTTP wait error: {err}"))?;
                    break 'wait Ok(());
                }
            }
            let registered_at = Instant::now();
            let screen_stable = if has_screen_stable {
                // Read the generation before snapshotting (same conservative
                // ordering as service_pending_waits) and seed the snapshot
                // memo, so the next servicing tick doesn't immediately take a
                // second full-grid snapshot right after this one.
                let generation = hosted.actor.observation().render_generation();
                match hosted.actor.full_snapshot() {
                    Ok(snapshot) => {
                        hosted.screen_stable_snapshot_generation = Some(generation);
                        Some(ScreenStableState::new(ScreenStableFingerprint::from_snapshot(snapshot), registered_at))
                    }
                    Err(err) => {
                        http_uds::write_error(stream, StatusCode::CONFLICT, &format!("screen stability not supported: {err}"))
                            .map_err(|err| format!("write HTTP wait error: {err}"))?;
                        break 'wait Ok(());
                    }
                }
            } else {
                None
            };

            if has_text_match {
                for condition in &conditions {
                    if let crate::protocol::WaitCondition::TextMatch { text } = condition {
                        if hosted.actor.screen_contains(text.clone())? {
                            http_uds::write_json(stream, StatusCode::OK, &http_uds::WaitResultResponse {
                                status: http_uds::WaitStatusResponse::Ready,
                                elapsed_ms: 0,
                            })
                            .map_err(|err| format!("write HTTP wait response: {err}"))?;
                            break 'wait Ok(());
                        }
                    }
                }
            }

            let pending_stream = stream.try_clone().map_err(|err| format!("clone HTTP wait stream: {err}"))?;
            if let Err(err) = set_stream_nonblocking(&pending_stream, true) {
                http_uds::write_error(stream, StatusCode::INTERNAL_SERVER_ERROR, &format!("set nonblocking: {err}"))
                    .map_err(|err| format!("write HTTP wait error: {err}"))?;
                break 'wait Ok(());
            }
            hosted.pending_waits.push(PendingWait {
                stream: pending_stream,
                conditions,
                screen_stable,
                timeout_ms: body.timeout_ms,
                registered_at,
            });
            Ok(())
        }
        _ => {
            http_uds::write_error(stream, StatusCode::NOT_FOUND, "not found").map_err(|err| format!("write HTTP not found response: {err}"))
        }
    }
}

fn write_http_not_found(stream: &mut SessionStream) -> Result<(), String> {
    http_uds::write_error(stream, StatusCode::NOT_FOUND, "not found").map_err(|err| format!("write HTTP not found response: {err}"))
}

fn signal_target_from_http(target: http_uds::SignalTargetRequest) -> crate::protocol::SignalTarget {
    match target {
        http_uds::SignalTargetRequest::Foreground => crate::protocol::SignalTarget::Foreground,
        http_uds::SignalTargetRequest::Leader => crate::protocol::SignalTarget::Leader,
        http_uds::SignalTargetRequest::Tree => crate::protocol::SignalTarget::Tree,
    }
}

fn wait_condition_from_http(condition: http_uds::WaitConditionRequest) -> crate::protocol::WaitCondition {
    match condition {
        http_uds::WaitConditionRequest::OutputIdle { quiet_ms } => crate::protocol::WaitCondition::OutputIdle { quiet_ms },
        http_uds::WaitConditionRequest::TextMatch { text } => crate::protocol::WaitCondition::TextMatch { text },
        http_uds::WaitConditionRequest::ScreenStable { stable_ms } => crate::protocol::WaitCondition::ScreenStable { stable_ms },
    }
}

fn wait_status_to_http(status: crate::protocol::WaitStatus) -> http_uds::WaitStatusResponse {
    match status {
        crate::protocol::WaitStatus::Ready => http_uds::WaitStatusResponse::Ready,
        crate::protocol::WaitStatus::Timeout => http_uds::WaitStatusResponse::Timeout,
        crate::protocol::WaitStatus::SessionGone => http_uds::WaitStatusResponse::SessionGone,
    }
}

fn http_input_key_bytes(key: http_uds::KeyRequest) -> Vec<u8> {
    match key {
        http_uds::KeyRequest::UnicodeScalar { codepoint } => {
            let mut bytes = Vec::new();
            if let Some(ch) = char::from_u32(codepoint) {
                let mut buf = [0; 4];
                bytes.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            }
            bytes
        }
        http_uds::KeyRequest::Named { key } => match key {
            http_uds::NamedKey::Enter => b"\r".to_vec(),
            http_uds::NamedKey::Escape => b"\x1b".to_vec(),
            http_uds::NamedKey::Backspace => b"\x7f".to_vec(),
            http_uds::NamedKey::Tab => b"\t".to_vec(),
            http_uds::NamedKey::Delete => b"\x1b[3~".to_vec(),
            http_uds::NamedKey::ArrowUp => b"\x1b[A".to_vec(),
            http_uds::NamedKey::ArrowDown => b"\x1b[B".to_vec(),
            http_uds::NamedKey::ArrowRight => b"\x1b[C".to_vec(),
            http_uds::NamedKey::ArrowLeft => b"\x1b[D".to_vec(),
        },
    }
}

struct PacketClient {
    image_output_cursor: u32,
    id: u64,
    stream: SessionStream,
    pending_output: PendingOutput,
    input_reader: ActiveClientReader,
    input_buffer: Vec<u8>,
    channels: HashMap<u32, PacketSessionChannel>,
    selectors: Vec<String>,
    screen_activity_stable_ms: Option<u64>,
    known_activity_sessions: HashMap<String, ActivitySession>,
    known_directory_sessions: HashSet<String>,
    /// Marked when this client's transport fails or its output backlog
    /// overflows. Client failures are never daemon-fatal: the client is
    /// reaped (with role release) on the next servicing pass.
    dead: bool,
}

struct PacketSessionChannel {
    session_id: String,
    role: ChannelRole,
    requested_role: ChannelRole,
    identity: AttachmentIdentity,
    denial_reason: Option<RoleDenialReason>,
    in_flight_generation: Option<u64>,
    last_sent_generation: u64,
    last_source_generation: u64,
    history: bool,
    view_changed: bool,
    view_state: crate::provider::ViewState,
    next_capture: Instant,
    local_images: bool,
    image_resident: HashSet<crate::image_delivery::ImageKey>,
    image_transfer: Option<ImageTransfer>,
}

#[derive(Default)]
struct PacketRenderCache {
    latest: Option<RenderBundle>,
    history_cursor: u128,
    rows: Vec<crate::provider::TerminalRenderRow>,
    row_generations: Vec<u64>,
    reset_generation: u64,
}

impl PacketRenderCache {
    fn store(&mut self, mut bundle: RenderBundle) {
        use crate::provider::TerminalRenderUpdateOpKind as Kind;
        let update = &mut bundle.packet.update;
        let generation = update.render_generation;
        if self.latest.as_ref().is_none_or(|previous| {
            let previous = &previous.packet.update;
            previous.cols != update.cols
                || previous.rows != update.rows
                || previous.terminal_modes.active_alternate_screen != update.terminal_modes.active_alternate_screen
        }) {
            self.rows = (0..update.rows)
                .map(|row| crate::provider::TerminalRenderRow {
                    row,
                    col_count: update.cols,
                    cells: vec![Default::default(); update.cols as usize],
                    ..Default::default()
                })
                .collect();
            self.row_generations = vec![generation; update.rows as usize];
            self.reset_generation = generation;
        }
        for op in std::mem::take(&mut update.ops) {
            if op.kind == Kind::FullVisibleReplace {
                self.reset_generation = generation;
            }
            if op.kind == Kind::ScrollCopy {
                // Snapshot before copying so overlapping scrolls are lossless.
                let copied: Vec<_> =
                    (0..op.row_count).filter_map(|offset| self.rows.get(usize::from(op.src_row + offset)).cloned()).collect();
                for (offset, mut row) in copied.into_iter().enumerate() {
                    let index = usize::from(op.dst_row) + offset;
                    if let Some(target) = self.rows.get_mut(index) {
                        row.row = index as u16;
                        *target = row;
                        self.row_generations[index] = generation;
                    }
                }
            } else {
                for row in op.rows {
                    let index = usize::from(row.row);
                    if let Some(target) = self.rows.get_mut(index) {
                        *target = row;
                        self.row_generations[index] = generation;
                    }
                }
            }
        }
        self.latest = Some(bundle);
    }

    fn since(&self, generation: u64) -> Option<RenderBundle> {
        use crate::provider::{TerminalRenderUpdateOp, TerminalRenderUpdateOpKind as Kind};
        let mut bundle = self.latest.clone()?;
        let update = &mut bundle.packet.update;
        if generation < self.reset_generation {
            update.dirty = DirtyState::Full;
            update.ops.push(TerminalRenderUpdateOp {
                kind: Kind::FullVisibleReplace,
                row_count: update.rows,
                col_count: update.cols,
                rows: self.rows.clone(),
                ..Default::default()
            });
        } else {
            update.ops = self
                .rows
                .iter()
                .zip(&self.row_generations)
                .filter(|(_, changed)| **changed > generation)
                .map(|(row, _)| TerminalRenderUpdateOp {
                    kind: Kind::RowReplace,
                    first_row: row.row,
                    row_count: 1,
                    col_count: update.cols,
                    rows: vec![row.clone()],
                    ..Default::default()
                })
                .collect();
            update.dirty = if update.ops.is_empty() { DirtyState::Clean } else { DirtyState::Partial };
        }
        Some(bundle)
    }

    fn latest_generation(&self) -> Option<u64> {
        self.latest.as_ref().map(|update| update.packet.update.render_generation)
    }

    fn latest(&self) -> Option<&RenderBundle> {
        self.latest.as_ref()
    }
}

impl PacketClient {
    fn new(
        id: u64,
        stream: SessionStream,
        selectors: Vec<String>,
        screen_activity_stable_ms: Option<u64>,
        initial_directory: &DirectorySnapshot,
        initial_activity: Option<&ActivitySnapshot>,
    ) -> Result<Self, String> {
        let input_reader = ActiveClientReader::new(&stream)?;
        Ok(Self {
            id,
            stream,
            pending_output: PendingOutput::new(),
            image_output_cursor: 0,
            input_reader,
            input_buffer: Vec::new(),
            channels: HashMap::new(),
            selectors,
            screen_activity_stable_ms,
            known_activity_sessions: initial_activity
                .into_iter()
                .flat_map(|snapshot| &snapshot.sessions)
                .map(|session| (session.session_id.clone(), session.clone()))
                .collect(),
            known_directory_sessions: initial_directory.sessions.iter().map(|entry| entry.session_id.clone()).collect(),
            dead: false,
        })
    }

    fn enqueue_control<T: serde::Serialize>(&mut self, msg_type: u8, value: &T) -> Result<(), String> {
        let frame = PacketFrame::new(CHANNEL_CONTROL, msg_type, value).map_err(|err| format!("encode packet control frame: {err}"))?;
        self.enqueue_frame(&frame)
    }

    /// Errors only on encode failures (a programming error in frame
    /// construction). A reader that stalls long enough to overflow its
    /// backlog is a client-scoped failure: the client is marked dead and
    /// reaped later, never surfaced as a daemon-level error.
    fn enqueue_frame(&mut self, frame: &PacketFrame) -> Result<(), String> {
        if self.dead {
            return Ok(());
        }
        if self.pending_output.len().saturating_add(frame.encoded_len()) > MAX_PENDING_CLIENT_OUTPUT_BYTES {
            self.dead = true;
            self.pending_output = PendingOutput::new();
            return Ok(());
        }
        frame.write(&mut self.pending_output).map_err(|err| format!("buffer packet frame: {err}"))
    }

    fn drain_input_frames(&mut self, pending: &mut VecDeque<PacketFrame>, timeout: Duration) -> Result<bool, std::io::Error> {
        let mut first_poll = true;
        loop {
            let chunk = if first_poll {
                first_poll = false;
                self.input_reader.poll_timeout(&mut self.stream, timeout)?
            } else {
                self.input_reader.poll(&mut self.stream)?
            };
            match chunk {
                Some(bytes) if bytes.is_empty() => return Ok(false),
                Some(bytes) => self.input_buffer.extend_from_slice(&bytes),
                None => break,
            }
        }

        while let Some(frame) = PacketFrame::read_from_buffer(&mut self.input_buffer)? {
            pending.push_back(frame);
        }
        Ok(true)
    }

    #[cfg(unix)]
    fn has_pending_output(&self) -> bool {
        !self.pending_output.is_empty()
            || self.channels.values().any(|channel| channel.image_transfer.as_ref().is_some_and(ImageTransfer::ready))
    }

    /// Rotate after every frame, including across calls when backpressure or a
    /// time budget stops a round. HashMap iteration order must not pick winners.
    fn queue_image_frames(&mut self, mut elapsed: impl FnMut() -> Duration) -> Result<(), String> {
        if self.pending_output.len() >= IMAGE_OUTPUT_HIGH_WATER {
            return Ok(());
        }
        let mut channels: Vec<_> = self
            .channels
            .iter()
            .filter(|(_, channel)| channel.image_transfer.as_ref().is_some_and(ImageTransfer::ready))
            .map(|(id, _)| *id)
            .collect();
        channels.sort_unstable();
        if channels.is_empty() {
            return Ok(());
        }
        let mut index = channels.partition_point(|id| *id <= self.image_output_cursor) % channels.len();
        let mut idle = 0;
        let mut visited = false;
        // Always allow one frame, even if scheduling or sorting exhausted the
        // soft time budget, so repeated preemption cannot prevent progress.
        while idle < channels.len()
            && self.pending_output.len() < IMAGE_OUTPUT_HIGH_WATER
            && (!visited || elapsed() < PACKET_OUTPUT_TIME_BUDGET)
        {
            visited = true;
            let id = channels[index];
            index = (index + 1) % channels.len();
            self.image_output_cursor = id;
            let channel = self.channels.get_mut(&id).expect("channel exists");
            let frame = match &mut channel.image_transfer {
                Some(transfer) => {
                    let frame = transfer.next(id)?;
                    if transfer.complete() {
                        channel.image_transfer = None;
                    }
                    frame
                }
                None => None,
            };
            if let Some(frame) = frame {
                self.enqueue_frame(&frame)?;
                idle = 0;
            } else {
                idle += 1;
            }
        }
        Ok(())
    }

    fn flush_pending_output(&mut self) -> Result<bool, String> {
        let started = Instant::now();
        self.queue_image_frames(|| started.elapsed())?;
        let mut written = 0;
        while !self.pending_output.is_empty()
            && written < PACKET_OUTPUT_WRITE_BUDGET
            && (written == 0 || started.elapsed() < PACKET_OUTPUT_TIME_BUDGET)
        {
            let count = self.pending_output.len().min(PACKET_OUTPUT_WRITE_BUDGET - written);
            match retry_interrupted(|| self.stream.write(&self.pending_output.as_slice()[..count])) {
                Ok(0) => return Ok(false),
                Ok(n) => {
                    self.pending_output.consume(n);
                    written += n;
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) if is_graceful_socket_shutdown(&err) => return Ok(false),
                Err(err) => return Err(format!("flush packet client output: {err}")),
            }
        }
        Ok(true)
    }
}

fn service_packet_clients(
    layout: &RuntimeLayout,
    sessions: &mut HashMap<String, HostedSession>,
    packet_clients: &mut Vec<PacketClient>,
) -> Result<bool, String> {
    let mut did_work = false;
    let mut index = 0;
    while index < packet_clients.len() {
        let mut pending = VecDeque::new();
        // A transport read error is a client-scoped failure, exactly like a
        // clean disconnect: drop the one client, never the daemon.
        let connected =
            !packet_clients[index].dead && packet_clients[index].drain_input_frames(&mut pending, Duration::ZERO).unwrap_or(false);
        if !connected {
            let removed = packet_clients.swap_remove(index);
            release_packet_client_roles(layout, sessions, &removed, packet_clients)?;
            did_work = true;
            continue;
        }

        while let Some(frame) = pending.pop_front() {
            did_work = true;
            // A frame-handling error is scoped to the client that sent the
            // frame (its channel bookkeeping is suspect from here on): drop
            // that client, keep the daemon and its sessions running.
            match handle_packet_frame(layout, sessions, packet_clients, index, frame) {
                Ok(updates) => {
                    for entry in updates {
                        broadcast_directory_upsert(entry, packet_clients)?;
                    }
                }
                Err(err) => {
                    eprintln!("dropping packet client after frame error: {err}");
                    packet_clients[index].dead = true;
                    break;
                }
            }
        }
        index += 1;
    }
    Ok(did_work)
}

/// Free controller slots held by a disconnected packet client and re-announce
/// the affected sessions' attachment counts.
fn release_packet_client_roles(
    layout: &RuntimeLayout,
    sessions: &mut HashMap<String, HostedSession>,
    removed: &PacketClient,
    packet_clients: &mut Vec<PacketClient>,
) -> Result<(), String> {
    let mut affected: Vec<&String> = Vec::new();
    for (channel, session_channel) in &removed.channels {
        if let Some(hosted) = sessions.get_mut(&session_channel.session_id) {
            let previously_had_controller = hosted.active_client.is_some() || hosted.packet_control.has_controllers();
            hosted.packet_control.remove(PacketChannelRef { client_id: removed.id, channel: *channel });
            let id = PacketChannelRef { client_id: removed.id, channel: *channel }.view_id();
            let _ = hosted.actor.release_input(id);
            hosted.actor.release_attachment_view(id);
            let _ = sync_packet_geometry(hosted);
            let _ = sync_packet_controller_presence(layout, hosted, previously_had_controller);
        }
        affected.push(&session_channel.session_id);
    }
    affected.sort();
    affected.dedup();
    for session_id in affected {
        if let Some(hosted) = sessions.get_mut(session_id) {
            // A failing session actor must not turn role release into a
            // daemon-fatal error; the session faults on its own servicing
            // pass and broadcasts its removal there.
            let _ = announce_seat_state(hosted, packet_clients);
            match directory_entry_for_session(layout, hosted, packet_clients) {
                Ok(entry) => broadcast_directory_upsert(entry, packet_clients)?,
                Err(err) => eprintln!("skipping directory update for session {session_id}: {err}"),
            }
        }
    }
    Ok(())
}

/// A dead client awaiting reaping must not hold the controller slot against
/// a live requester (packet role grants and legacy attach alike).
fn vacate_dead_packet_controller(hosted: &mut HostedSession, packet_clients: &[PacketClient]) {
    hosted.packet_control.retain(|current| packet_controller_holder_is_live(current, packet_clients));
}

fn packet_controller_holder_is_live(current: PacketChannelRef, packet_clients: &[PacketClient]) -> bool {
    packet_clients.iter().any(|client| client.id == current.client_id && !client.dead)
}

/// Resolve a controller/watcher request for `requester`, demoting the current
/// controller when `take` is set. Legacy stream controllers are moved into the
/// watcher set so their connection and output stream remain alive.
fn grant_packet_role(
    hosted: &mut HostedSession,
    packet_clients: &mut [PacketClient],
    requester: PacketChannelRef,
    role: ChannelRole,
    take: bool,
) -> Result<(ChannelRole, Option<RoleDenialReason>), String> {
    vacate_dead_packet_controller(hosted, packet_clients);
    if role == ChannelRole::Controller && hosted.active_client.is_some() {
        if !take {
            hosted.packet_control.request(requester, false, false);
            return Ok((ChannelRole::Watcher, Some(RoleDenialReason { held_by: ControllerHolder::Stream })));
        }
        if let Some(mut controller) = hosted.active_client.take() {
            controller.denial_reason = Some(RoleDenialReason { held_by: ControllerHolder::Packet });
            hosted.watchers.push(controller);
        }
    }
    let granted = hosted.packet_control.request(requester, role == ChannelRole::Controller, take);
    if granted && take {
        for client in packet_clients.iter_mut() {
            for (channel, session) in &mut client.channels {
                if session.session_id == hosted.metadata.id && (PacketChannelRef { client_id: client.id, channel: *channel }) != requester {
                    session.role = ChannelRole::Watcher;
                    session.denial_reason = Some(RoleDenialReason { held_by: ControllerHolder::Packet });
                }
            }
        }
    }
    let denial = (role == ChannelRole::Controller && !granted).then_some(RoleDenialReason { held_by: ControllerHolder::Packet });
    Ok((if granted { ChannelRole::Controller } else { ChannelRole::Watcher }, denial))
}

fn handle_packet_frame(
    layout: &RuntimeLayout,
    sessions: &mut HashMap<String, HostedSession>,
    packet_clients: &mut [PacketClient],
    index: usize,
    frame: PacketFrame,
) -> Result<Vec<DirectoryEntry>, String> {
    let mut updates = Vec::new();
    match (frame.channel, frame.msg_type) {
        (CHANNEL_CONTROL, MSG_CONTROL_OPEN_CHANNEL) => {
            let open = frame.decode::<OpenChannel>().map_err(|err| format!("decode open-channel packet: {err}"))?;
            open_packet_channel(layout, sessions, packet_clients, index, open, &mut updates)?;
        }
        (CHANNEL_CONTROL, MSG_CONTROL_CLOSE_CHANNEL) => {
            let close = frame.decode::<CloseChannel>().map_err(|err| format!("decode close-channel packet: {err}"))?;
            let client_id = packet_clients[index].id;
            if let Some(removed) = packet_clients[index].channels.remove(&close.channel) {
                if let Some(hosted) = sessions.get_mut(&removed.session_id) {
                    let previously_had_controller = hosted.active_client.is_some() || hosted.packet_control.has_controllers();
                    hosted.packet_control.remove(PacketChannelRef { client_id, channel: close.channel });
                    hosted.actor.release_attachment_view(PacketChannelRef { client_id, channel: close.channel }.view_id());
                    sync_packet_geometry(hosted)?;
                    sync_packet_controller_presence(layout, hosted, previously_had_controller)?;
                    announce_seat_state(hosted, packet_clients)?;
                    updates.push(directory_entry_for_session(layout, hosted, packet_clients)?);
                }
            }
        }
        (channel, crate::packet::MSG_SESSION_IMAGE_FILE_RESULT) if channel != CHANNEL_CONTROL => {
            if let Some(session) = packet_clients[index].channels.get_mut(&channel) {
                if let Some(transfer) = &mut session.image_transfer {
                    let result = frame.decode::<crate::packet::ImageFileResult>().map_err(|e| e.to_string())?;
                    if !result.acquired {
                        session.local_images = false;
                    }
                    transfer.file_result(result)?;
                }
            }
        }
        (channel, MSG_SESSION_ACK) if channel != CHANNEL_CONTROL => {
            let ack = frame.decode::<Ack>().map_err(|err| format!("decode ack packet: {err}"))?;
            let client = &mut packet_clients[index];
            if let Some(session_channel) = client.channels.get_mut(&channel) {
                if session_channel.in_flight_generation == Some(ack.generation) {
                    session_channel.in_flight_generation = None;
                    // ACKs release channel backpressure only. Packet capture
                    // already consumed actor damage into the daemon cache.
                }
            }
        }
        (channel, MSG_SESSION_INPUT) if channel != CHANNEL_CONTROL => {
            let input = frame.decode::<Input>().map_err(|err| format!("decode input packet: {err}"))?;
            if let Some(session) = packet_clients[index].channels.get(&channel) {
                if let Some(hosted) = sessions.get_mut(&session.session_id) {
                    let key = PacketChannelRef { client_id: packet_clients[index].id, channel };
                    match input.event {
                        TerminalInputEvent::Resize(event) => {
                            hosted.packet_control.resize(key, event.cols, event.rows);
                            if event.cell_width_px.is_finite()
                                && event.cell_height_px.is_finite()
                                && event.cell_width_px > 0.0
                                && event.cell_height_px > 0.0
                            {
                                hosted.packet_control.set_cell_size(
                                    key,
                                    event.cell_width_px.round() as u32,
                                    event.cell_height_px.round() as u32,
                                );
                            }
                            sync_packet_geometry(hosted)?;
                            updates.push(directory_entry_for_session(layout, hosted, packet_clients)?);
                        }
                        TerminalInputEvent::Focus(event) => {
                            hosted.packet_control.focus(key, event.focused);
                            sync_packet_geometry(hosted)?;
                        }
                        TerminalInputEvent::Mouse(mut event) => {
                            let modes = hosted.packet_render_cache.latest().map(|u| u.packet.update.terminal_modes).unwrap_or_default();
                            let local_wheel = event.kind == TerminalMouseEventKind::Wheel
                                && (session.history
                                    || session.role == ChannelRole::Watcher
                                    || (!modes.mouse_tracking && !(modes.active_alternate_screen && modes.alternate_scroll)));
                            if local_wheel {
                                let delta =
                                    if event.wheel_delta_y.is_finite() { (event.wheel_delta_y.round() as i64).saturating_neg() } else { 0 };
                                if delta != 0 {
                                    match hosted.actor.request_result(|reply| crate::host::actor::SessionCommand::SetAttachmentView {
                                        id: key.view_id(),
                                        command: crate::provider::ViewportCommand::DeltaRows(delta),
                                        reply,
                                    }) {
                                        Ok(history) => {
                                            let session = packet_clients[index].channels.get_mut(&channel).expect("channel exists");
                                            session.history = history;
                                            session.view_changed = true;
                                        }
                                        Err(err) => packet_clients[index].enqueue_frame(
                                            &PacketFrame::new(
                                                channel,
                                                crate::packet::MSG_SESSION_VIEW_STATE,
                                                &crate::provider::ViewState {
                                                    status: crate::provider::ViewStatus::Stale,
                                                    notice: Some(err),
                                                },
                                            )
                                            .map_err(|e| e.to_string())?,
                                        )?,
                                    }
                                }
                            } else if !session.history
                                && session.role == ChannelRole::Controller
                                && ((event.cell_col < hosted.applied_size.0 && event.cell_row < hosted.applied_size.1)
                                    || event.kind == TerminalMouseEventKind::Release)
                            {
                                event.cell_col = event.cell_col.min(hosted.applied_size.0.saturating_sub(1));
                                event.cell_row = event.cell_row.min(hosted.applied_size.1.saturating_sub(1));
                                let source = hosted.packet_control.cell_size(key);
                                event.x_px = event.x_px / source.0 as f32 * hosted.applied_cell_size.0 as f32;
                                event.y_px = event.y_px / source.1 as f32 * hosted.applied_cell_size.1 as f32;
                                event.x_px = event
                                    .x_px
                                    .clamp(0.0, (f32::from(hosted.applied_size.0) * hosted.applied_cell_size.0 as f32 - 1.0).max(0.0));
                                event.y_px = event
                                    .y_px
                                    .clamp(0.0, (f32::from(hosted.applied_size.1) * hosted.applied_cell_size.1 as f32 - 1.0).max(0.0));
                                route_packet_mouse_event(&hosted.actor, key.view_id(), event)?;
                            }
                        }
                        event if session.role == ChannelRole::Controller => {
                            if matches!(
                                event,
                                TerminalInputEvent::Text(_)
                                    | TerminalInputEvent::Paste(_)
                                    | TerminalInputEvent::Key(crate::provider::TerminalKeyEvent {
                                        action: crate::provider::TerminalKeyAction::Press,
                                        ..
                                    })
                                    | TerminalInputEvent::RawBytes(_)
                            ) {
                                hosted.actor.release_attachment_view(key.view_id());
                                let session = packet_clients[index].channels.get_mut(&channel).expect("channel exists");
                                if session.history {
                                    session.history = false;
                                    session.view_changed = true;
                                }
                            }
                            route_packet_input_event(&hosted.actor, key.view_id(), event)?;
                        }
                        _ => {}
                    }
                }
            }
        }
        (channel, MSG_SESSION_RESIZE) if channel != CHANNEL_CONTROL => {
            let resize = frame.decode::<Resize>().map_err(|err| format!("decode resize packet: {err}"))?;
            if let Some(session) = packet_clients[index].channels.get(&channel) {
                if let Some(hosted) = sessions.get_mut(&session.session_id) {
                    hosted.packet_control.resize(
                        PacketChannelRef { client_id: packet_clients[index].id, channel },
                        resize.cols,
                        resize.rows,
                    );
                    sync_packet_geometry(hosted)?;
                    updates.push(directory_entry_for_session(layout, hosted, packet_clients)?);
                }
            }
        }
        (channel, crate::packet::MSG_SESSION_SIZE_POLICY) if channel != CHANNEL_CONTROL => {
            let fixed = frame.decode::<Option<Resize>>().map_err(|e| e.to_string())?;
            if let Some(session) = packet_clients[index].channels.get(&channel) {
                if session.role == ChannelRole::Controller {
                    if let Some(hosted) = sessions.get_mut(&session.session_id) {
                        hosted.fixed_size = fixed.map(|size| (size.cols.max(1), size.rows.max(1)));
                        sync_packet_geometry(hosted)?;
                        announce_seat_state(hosted, packet_clients)?;
                        updates.push(directory_entry_for_session(layout, hosted, packet_clients)?);
                    }
                }
            }
        }
        (channel, MSG_SESSION_VIEWPORT) if channel != CHANNEL_CONTROL => {
            let viewport = frame.decode::<crate::packet::Viewport>().map_err(|err| format!("decode viewport packet: {err}"))?;
            if let Some(session) = packet_clients[index].channels.get_mut(&channel) {
                if let Some(hosted) = sessions.get(&session.session_id) {
                    let id = PacketChannelRef { client_id: packet_clients[index].id, channel }.view_id();
                    match hosted.actor.request_result(|reply| crate::host::actor::SessionCommand::SetAttachmentView {
                        id,
                        command: viewport.command,
                        reply,
                    }) {
                        Ok(history) => {
                            session.history = history;
                            session.view_changed = true;
                        }
                        Err(err) => {
                            packet_clients[index].enqueue_frame(
                                &PacketFrame::new(channel, crate::packet::MSG_SESSION_VIEW_STATE, &crate::provider::ViewState {
                                    status: crate::provider::ViewStatus::Stale,
                                    notice: Some(err),
                                })
                                .map_err(|e| e.to_string())?,
                            )?;
                        }
                    }
                }
            }
        }
        (channel, MSG_SESSION_ROLE) if channel != CHANNEL_CONTROL => {
            let request = frame.decode::<RoleRequest>().map_err(|err| format!("decode role request packet: {err}"))?;
            apply_packet_role_request(layout, sessions, packet_clients, index, channel, request, &mut updates)?;
        }
        _ => {}
    }
    Ok(updates)
}

fn apply_packet_role_request(
    layout: &RuntimeLayout,
    sessions: &mut HashMap<String, HostedSession>,
    packet_clients: &mut [PacketClient],
    index: usize,
    channel: u32,
    request: RoleRequest,
    updates: &mut Vec<DirectoryEntry>,
) -> Result<(), String> {
    let Some(session_id) = packet_clients[index].channels.get(&channel).map(|session| session.session_id.clone()) else {
        return Ok(());
    };
    let Some(hosted) = sessions.get_mut(&session_id) else {
        return Ok(());
    };
    let previously_had_controller = hosted.active_client.is_some() || hosted.packet_control.has_controllers();
    let requester = PacketChannelRef { client_id: packet_clients[index].id, channel };
    let (granted, denial_reason) = grant_packet_role(hosted, packet_clients, requester, request.role, request.take)?;
    let controller = controller_identity(hosted, packet_clients);
    let client = &mut packet_clients[index];
    if let Some(session_channel) = client.channels.get_mut(&channel) {
        session_channel.role = granted;
        session_channel.requested_role = request.role;
        session_channel.denial_reason = denial_reason;
    }
    let (participants, exclusive) = packet_presence(hosted, packet_clients);
    packet_clients[index].enqueue_frame(
        &PacketFrame::new(channel, MSG_SESSION_ROLE, &RoleState {
            role: granted,
            controller,
            denial_reason,
            participants,
            exclusive,
            fixed_size: hosted.fixed_size.map(|(cols, rows)| Resize { cols, rows }),
        })
        .map_err(|err| format!("encode role state packet: {err}"))?,
    )?;
    sync_packet_geometry(hosted)?;
    sync_packet_controller_presence(layout, hosted, previously_had_controller)?;
    if let Some(hosted) = sessions.get(&session_id) {
        updates.push(directory_entry_for_session(layout, hosted, packet_clients)?);
    }
    if let Some(hosted) = sessions.get_mut(&session_id) {
        announce_seat_state_except(hosted, packet_clients, Some(requester))?;
    }
    Ok(())
}

fn open_packet_channel(
    layout: &RuntimeLayout,
    sessions: &mut HashMap<String, HostedSession>,
    packet_clients: &mut [PacketClient],
    index: usize,
    open: OpenChannel,
    updates: &mut Vec<DirectoryEntry>,
) -> Result<(), String> {
    if open.channel == CHANNEL_CONTROL {
        packet_clients[index].enqueue_control(MSG_CONTROL_ERROR, &ControlError {
            channel: open.channel,
            message: "session channel must be non-zero".to_string(),
        })?;
        return Ok(());
    }
    if packet_clients[index].channels.contains_key(&open.channel) {
        return Err("session channel is already open".into());
    }
    let mut open = open;
    open.identity = normalize_attachment_identity(open.identity);
    let Some(hosted) = sessions.get_mut(&open.session_id) else {
        packet_clients[index].enqueue_control(MSG_CONTROL_ERROR, &ControlError {
            channel: open.channel,
            message: format!("unknown session {}", open.session_id),
        })?;
        return Ok(());
    };

    // Probe render state before granting a role: a session whose VT engine
    // cannot serve it (e.g. the passthrough placeholder) must fail this one
    // channel, not demote the current controller or tear down the daemon.
    let update = match hosted.actor.packet_render(true) {
        Ok(update) => update,
        Err(err) => {
            packet_clients[index].enqueue_control(MSG_CONTROL_ERROR, &ControlError {
                channel: open.channel,
                message: format!("open channel for session {}: {err}", open.session_id),
            })?;
            return Ok(());
        }
    };
    let previously_had_controller = hosted.active_client.is_some() || hosted.packet_control.has_controllers();
    let requester = PacketChannelRef { client_id: packet_clients[index].id, channel: open.channel };
    let (granted, denial_reason) = grant_packet_role(hosted, packet_clients, requester, open.role, open.take)?;
    let session_id = open.session_id;
    let generation = update.packet.update.render_generation;
    let controller =
        if granted == ChannelRole::Controller { Some(open.identity.clone()) } else { controller_identity(hosted, packet_clients) };
    hosted.packet_render_cache.store(update.clone());
    packet_clients[index].channels.insert(open.channel, PacketSessionChannel {
        session_id: session_id.clone(),
        role: granted,
        requested_role: open.role,
        identity: open.identity,
        denial_reason,
        in_flight_generation: Some(generation),
        last_sent_generation: generation,
        last_source_generation: generation,
        history: false,
        view_changed: false,
        view_state: Default::default(),
        next_capture: Instant::now(),
        local_images: true,
        image_resident: HashSet::new(),
        image_transfer: None,
    });
    let (participants, exclusive) = packet_presence(hosted, packet_clients);
    let client = &mut packet_clients[index];
    client.enqueue_frame(
        &PacketFrame::new(open.channel, MSG_SESSION_ROLE, &RoleState {
            role: granted,
            controller,
            denial_reason,
            participants,
            exclusive,
            fixed_size: hosted.fixed_size.map(|(cols, rows)| Resize { cols, rows }),
        })
        .map_err(|err| format!("encode role state packet: {err}"))?,
    )?;
    let channel = client.channels.get_mut(&open.channel).expect("opened channel");
    channel.image_transfer = Some(ImageTransfer::new(open.channel, update, &mut channel.image_resident)?);
    sync_packet_geometry(hosted)?;
    sync_packet_controller_presence(layout, hosted, previously_had_controller)?;
    if let Some(hosted) = sessions.get(&session_id) {
        updates.push(directory_entry_for_session(layout, hosted, packet_clients)?);
    }
    if let Some(hosted) = sessions.get_mut(&session_id) {
        announce_seat_state_except(hosted, packet_clients, Some(requester))?;
    }
    Ok(())
}

fn push_due_packet_renders(
    session_id: &str,
    actor: &SessionActor,
    packet_clients: &mut [PacketClient],
    render_cache: &mut PacketRenderCache,
) -> Result<(), String> {
    if !packet_clients.iter().any(|client| client.channels.values().any(|channel| channel.session_id == session_id)) {
        return Ok(());
    }

    if actor.observation().dirty() != DirtyState::Clean {
        let result = actor.packet_render(false);
        match result {
            Ok(update) => render_cache.store(update),
            Err(error) => {
                for client in packet_clients.iter_mut() {
                    let channels: Vec<_> = client.channels.iter().filter(|(_, c)| c.session_id == session_id).map(|(id, _)| *id).collect();
                    for id in channels {
                        let channel = client.channels.get_mut(&id).expect("channel exists");
                        let state = crate::provider::ViewState { status: crate::provider::ViewStatus::Stale, notice: Some(error.clone()) };
                        if channel.view_state != state {
                            channel.view_state = state.clone();
                            client.enqueue_frame(
                                &PacketFrame::new(id, crate::packet::MSG_SESSION_VIEW_STATE, &state).map_err(|e| e.to_string())?,
                            )?;
                        }
                    }
                }
                return Ok(());
            }
        }
    }

    let Some(latest_generation) = render_cache.latest_generation() else {
        return Ok(());
    };

    let now = Instant::now();
    let mut due: Vec<_> = packet_clients
        .iter()
        .enumerate()
        .flat_map(|(index, client)| {
            client.channels.iter().filter_map(move |(channel, session)| {
                (session.session_id == session_id
                    && session.in_flight_generation.is_none()
                    && (session.view_changed || session.last_source_generation < latest_generation)
                    && (now >= session.next_capture
                        || (!session.history && session.view_state.status != crate::provider::ViewStatus::Stale)))
                    .then_some((PacketChannelRef { client_id: client.id, channel: *channel }, index))
            })
        })
        .collect();
    due.sort_by_key(|(key, _)| (key.view_id() <= render_cache.history_cursor, key.view_id()));
    let mut captures = 0;
    for (key, index) in due {
        let session = &packet_clients[index].channels[&key.channel];
        if session.history && captures >= 2 {
            continue;
        }
        let result = if session.history {
            captures += 1;
            render_cache.history_cursor = key.view_id();
            actor.request_result(|reply| crate::host::actor::SessionCommand::CaptureAttachmentView { id: key.view_id(), reply }).map(
                |capture| match capture {
                    Some(frame) => RenderBundle {
                        images: frame.images.into_iter().map(crate::image_backing::RetainedImage::from_owned).collect(),
                        packet: RenderPacket {
                            update: frame.update,
                            links: frame.links,
                            view: crate::provider::ViewState {
                                status: crate::provider::ViewStatus::History,
                                notice: frame.discarded.then(|| "Earlier history was discarded".into()),
                            },
                        },
                    },
                    None => {
                        let mut packet = render_cache.since(0).expect("cache generation checked above");
                        packet.packet.view.notice = Some("History was cleared; returned to live".into());
                        packet
                    }
                },
            )
        } else if session.view_changed {
            Ok(render_cache.since(0).expect("cache generation checked above"))
        } else {
            Ok(render_cache.since(session.last_source_generation).expect("cache generation checked above"))
        };
        let next_generation = session.last_sent_generation.saturating_add(1);
        let result = result.and_then(|mut packet| {
            packet.packet.update.render_generation = next_generation;
            let view = packet.packet.view.clone();
            let session = packet_clients[index].channels.get_mut(&key.channel).expect("due channel");
            let transfer = ImageTransfer::new(key.channel, packet, &mut session.image_resident)?.local_files(session.local_images);
            Ok((transfer, view))
        });
        match result {
            Ok((transfer, view)) => {
                let client = &mut packet_clients[index];
                let session = client.channels.get_mut(&key.channel).expect("due channel");
                session.image_transfer = Some(transfer);
                session.history = view.status == crate::provider::ViewStatus::History;
                session.view_state = view;
                session.view_changed = false;
                session.in_flight_generation = Some(next_generation);
                session.last_sent_generation = next_generation;
                session.last_source_generation = latest_generation;
                session.next_capture = now + Duration::from_millis(34);
            }
            Err(err) => {
                let client = &mut packet_clients[index];
                let session = client.channels.get_mut(&key.channel).expect("due channel");
                session.next_capture = now + Duration::from_millis(250);
                let state = crate::provider::ViewState { status: crate::provider::ViewStatus::Stale, notice: Some(err) };
                if session.view_state != state {
                    session.view_state = state.clone();
                    client.enqueue_frame(
                        &PacketFrame::new(key.channel, crate::packet::MSG_SESSION_VIEW_STATE, &state).map_err(|e| e.to_string())?,
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn route_packet_input_event(actor: &SessionActor, source: u128, event: TerminalInputEvent) -> Result<(), String> {
    match event {
        TerminalInputEvent::Text(event) => actor.write_input(event.text.into_bytes()),
        TerminalInputEvent::Paste(event) => actor.paste(event.text.into_bytes()).map(|_| ()),
        TerminalInputEvent::RawBytes(bytes) => actor.write_input(bytes),
        TerminalInputEvent::Resize(event) => {
            actor.resize(event.cols, event.rows)?;
            if event.cell_width_px.is_finite()
                && event.cell_height_px.is_finite()
                && event.cell_width_px > 0.0
                && event.cell_height_px > 0.0
            {
                actor.set_cell_size(event.cell_width_px.round() as u32, event.cell_height_px.round() as u32)?;
            }
            Ok(())
        }
        TerminalInputEvent::Mouse(event) => route_packet_mouse_event(actor, source, event),
        TerminalInputEvent::Key(event) => actor.key(source, event).map(|_| ()),
        TerminalInputEvent::Focus(_) => Ok(()),
    }
}

fn route_packet_mouse_event(actor: &SessionActor, source: u128, event: crate::provider::TerminalMouseEvent) -> Result<(), String> {
    let modifiers = vt::MouseModifiers {
        shift: event.modifiers.contains(crate::provider::TerminalModifiers::SHIFT),
        ctrl: event.modifiers.contains(crate::provider::TerminalModifiers::CTRL),
        alt: event.modifiers.contains(crate::provider::TerminalModifiers::ALT),
    };
    if event.kind == TerminalMouseEventKind::Wheel {
        actor.application_wheel(SessionWheelEvent {
            modifiers,
            cell_col: event.cell_col,
            cell_row: event.cell_row,
            x_px: event.x_px,
            y_px: event.y_px,
            wheel_delta_x: event.wheel_delta_x,
            wheel_delta_y: event.wheel_delta_y,
        })?;
        return Ok(());
    }

    let action = match event.kind {
        TerminalMouseEventKind::Press => vt::MouseAction::Press,
        TerminalMouseEventKind::Release => vt::MouseAction::Release,
        TerminalMouseEventKind::Move => vt::MouseAction::Motion,
        TerminalMouseEventKind::Wheel => unreachable!("wheel handled above"),
    };
    actor.mouse(source, SessionMouseEvent {
        action,
        button: event.button.and_then(packet_mouse_button),
        any_button_pressed: !event.buttons.is_empty(),
        modifiers,
        x_px: event.x_px,
        y_px: event.y_px,
    })?;
    Ok(())
}

fn packet_mouse_button(button: TerminalMouseButton) -> Option<vt::MouseButton> {
    match button {
        TerminalMouseButton::Left => Some(vt::MouseButton::Left),
        TerminalMouseButton::Middle => Some(vt::MouseButton::Middle),
        TerminalMouseButton::Right => Some(vt::MouseButton::Right),
        TerminalMouseButton::Back => Some(vt::MouseButton::Eight),
        TerminalMouseButton::Forward => Some(vt::MouseButton::Nine),
    }
}

/// Flush failures mark the client dead rather than dropping it here:
/// removal happens in `service_packet_clients`, which also releases any
/// controller role the client held.
/// Large history frames can fill a Unix socket's send buffer. Sleeping a
/// whole servicing tick between each partial write adds latency to every
/// channel on that connection. Include ready, not-yet-encoded asset frames:
/// draining the byte buffer must not impose a 10ms sleep between image batches.
/// File offers awaiting acquisition replies are deliberately excluded.
fn wait_packet_output(clients: &[PacketClient]) {
    #[cfg(unix)]
    {
        use std::os::fd::AsFd;

        use nix::poll::{poll, PollFd, PollFlags};
        let mut pending: Vec<_> = clients
            .iter()
            .filter(|client| !client.dead && client.has_pending_output())
            .map(|client| PollFd::new(client.stream.as_fd(), PollFlags::POLLOUT))
            .collect();
        if !pending.is_empty() && poll(&mut pending, SESSION_DAEMON_SERVICING_TICK.as_millis() as u16).is_ok() {
            return;
        }
    }
    #[cfg(not(unix))]
    let _ = clients;
    thread::sleep(SESSION_DAEMON_SERVICING_TICK);
}

fn flush_packet_clients(packet_clients: &mut [PacketClient]) {
    for client in packet_clients {
        if client.dead {
            continue;
        }
        if !client.flush_pending_output().unwrap_or(false) {
            client.dead = true;
        }
    }
}

/// Bytes queued for a client socket, consumed from the front on partial
/// writes. A start cursor makes consumption O(1) instead of `Vec::drain`'s
/// per-write memmove of the whole backlog (issue #135). The consumed prefix
/// is compacted away once it outgrows the live remainder, so total memmove
/// work is bounded by total bytes queued (amortized O(1) per byte).
struct PendingOutput {
    buf: Vec<u8>,
    start: usize,
}

impl PendingOutput {
    fn new() -> Self {
        Self { buf: Vec::new(), start: 0 }
    }

    /// Bytes not yet written to the socket — the quantity capped by
    /// `MAX_PENDING_CLIENT_OUTPUT_BYTES`, exactly what `Vec::len` measured
    /// before the cursor existed.
    fn len(&self) -> usize {
        self.buf.len() - self.start
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn as_slice(&self) -> &[u8] {
        &self.buf[self.start..]
    }

    fn consume(&mut self, n: usize) {
        self.start += n;
        debug_assert!(self.start <= self.buf.len());
        if self.start >= self.buf.len() {
            self.buf.clear();
            self.start = 0;
        } else if self.start > self.len() {
            // The dead prefix outgrew the live remainder: one memmove that
            // touches fewer bytes than were consumed since the last
            // compaction keeps append targets (and memory) bounded.
            self.buf.copy_within(self.start.., 0);
            let remaining = self.buf.len() - self.start;
            self.buf.truncate(remaining);
            self.start = 0;
        }
    }
}

/// Frames append their wire encoding straight into the backlog through
/// `io::Write`, so enqueueing copies each payload exactly once — no
/// intermediate encode buffer.
impl Write for PendingOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
impl From<Vec<u8>> for PendingOutput {
    fn from(buf: Vec<u8>) -> Self {
        Self { buf, start: 0 }
    }
}

struct ActiveClient {
    stream: SessionStream,
    pending_output: PendingOutput,
    input_reader: ActiveClientReader,
    input_buffer: Vec<u8>,
    capabilities: vt::ClientCapabilities,
    identity: AttachmentIdentity,
    denial_reason: Option<RoleDenialReason>,
}

impl ActiveClient {
    fn new(stream: SessionStream, capabilities: vt::ClientCapabilities, identity: AttachmentIdentity) -> Result<Self, String> {
        let input_reader = ActiveClientReader::new(&stream)?;
        Ok(Self {
            stream,
            pending_output: PendingOutput::new(),
            input_reader,
            input_buffer: Vec::new(),
            capabilities,
            identity,
            denial_reason: None,
        })
    }

    fn drain_input_frames(&mut self, pending: &mut VecDeque<Frame>, timeout: Duration) -> Result<bool, std::io::Error> {
        let mut first_poll = true;
        loop {
            let chunk = if first_poll {
                first_poll = false;
                self.input_reader.poll_timeout(&mut self.stream, timeout)?
            } else {
                self.input_reader.poll(&mut self.stream)?
            };
            match chunk {
                Some(bytes) if bytes.is_empty() => return Ok(false),
                Some(bytes) => self.input_buffer.extend_from_slice(&bytes),
                None => break,
            }
        }

        while let Some(frame) = Frame::read_from_buffer(&mut self.input_buffer)? {
            pending.push_back(frame);
        }
        Ok(true)
    }

    fn enqueue_frame(&mut self, frame: &Frame) -> Result<(), String> {
        if self.pending_output.len().saturating_add(frame.encoded_len()) > MAX_PENDING_CLIENT_OUTPUT_BYTES {
            return Err(format!("client output backlog exceeded {} bytes", MAX_PENDING_CLIENT_OUTPUT_BYTES));
        }
        frame.write(&mut self.pending_output).map_err(|err| format!("buffer client frame: {err}"))
    }

    /// Frames a raw PTY output chunk straight into the backlog — header plus
    /// one payload copy, with no intermediate `Frame` allocation. This is the
    /// per-byte fan-out hot path (issue #135).
    fn enqueue_output(&mut self, payload: &[u8]) -> Result<(), String> {
        if self.pending_output.len().saturating_add(Frame::output_encoded_len(payload.len())) > MAX_PENDING_CLIENT_OUTPUT_BYTES {
            return Err(format!("client output backlog exceeded {} bytes", MAX_PENDING_CLIENT_OUTPUT_BYTES));
        }
        Frame::write_output(&mut self.pending_output, payload).map_err(|err| format!("buffer client frame: {err}"))
    }

    fn flush_pending_output(&mut self) -> Result<bool, String> {
        while !self.pending_output.is_empty() {
            match retry_interrupted(|| self.stream.write(self.pending_output.as_slice())) {
                Ok(0) => return Ok(false),
                Ok(n) => {
                    self.pending_output.consume(n);
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) if is_graceful_socket_shutdown(&err) => return Ok(false),
                Err(err) => return Err(format!("flush client output: {err}")),
            }
        }
        Ok(true)
    }
}

#[cfg(windows)]
struct ActiveClientReader {
    reader: crate::platform::ipc::OverlappedRead,
}

#[cfg(windows)]
impl ActiveClientReader {
    fn new(stream: &SessionStream) -> Result<Self, String> {
        let reader = stream.overlapped_reader(64 * 1024).map_err(|err| format!("create foreground client reader: {err}"))?;
        Ok(Self { reader })
    }

    fn poll(&mut self, _stream: &mut SessionStream) -> Result<Option<Vec<u8>>, std::io::Error> {
        self.reader.poll()
    }

    fn poll_timeout(&mut self, _stream: &mut SessionStream, timeout: Duration) -> Result<Option<Vec<u8>>, std::io::Error> {
        self.reader.poll_timeout(timeout)
    }
}

#[cfg(not(windows))]
struct ActiveClientReader;

#[cfg(not(windows))]
impl ActiveClientReader {
    fn new(_stream: &SessionStream) -> Result<Self, String> {
        Ok(Self)
    }

    fn poll(&mut self, stream: &mut SessionStream) -> Result<Option<Vec<u8>>, std::io::Error> {
        self.poll_timeout(stream, Duration::ZERO)
    }

    fn poll_timeout(&mut self, stream: &mut SessionStream, _timeout: Duration) -> Result<Option<Vec<u8>>, std::io::Error> {
        let mut buf = vec![0; 64 * 1024];
        match retry_interrupted(|| stream.read(&mut buf)) {
            Ok(0) => Ok(Some(Vec::new())),
            Ok(n) => {
                buf.truncate(n);
                Ok(Some(buf))
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(err) => Err(err),
        }
    }
}

fn retry_interrupted<T>(mut operation: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    loop {
        match operation() {
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            result => return result,
        }
    }
}

fn wait_for_socket(path: &Path) -> Result<(), String> {
    // Feature builds can spend several seconds loading the Ghostty VT library
    // and starting the daemon process under CI load before the socket is bound.
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if try_connect_session_stream(path).is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
    Err(format!("timed out waiting for socket {}", path.display()))
}

pub(crate) fn ensure_daemon_started(layout: &RuntimeLayout) -> Result<(), String> {
    validate_session_socket_path(&layout.socket_path())?;
    if try_connect_session_stream(&layout.socket_path()).is_ok() && is_session_daemon_alive(layout.root(), layout.daemon_name()) {
        return Ok(());
    }

    if layout.socket_path().exists() && !is_session_daemon_alive(layout.root(), layout.daemon_name()) {
        match fs::remove_file(layout.socket_path()) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(format!("remove stale daemon socket {}: {err}", layout.socket_path().display())),
        }
    }

    layout.ensure_daemon_dirs()?;
    spawn_daemon_process(layout.root(), layout.daemon_name())?;
    wait_for_socket(&layout.socket_path())
}

fn http_error_message(response: http_uds::HttpResponse) -> String {
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&response.body) {
        if let Some(message) = value.get("error").and_then(|value| value.as_str()) {
            return message.to_string();
        }
    }
    format!("HTTP request returned {}", response.status)
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        collections::VecDeque,
        io::{self, Write},
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };

    use super::{
        apply_attach_state, attach_foreground, attach_init_capabilities, default_vt_engine, record_pty_output, session_socket_path,
        AttachCleanupGuard, PacketTerminalRenderer, ScreenStableFingerprint, ScreenStableState, TestReplayProbeVtEngine,
        SCREEN_STABLE_CHANGED_CELL_TOLERANCE,
    };
    use crate::{
        http_uds::read_http_request_for_test,
        protocol::AttachmentIdentity,
        provider::{TerminalCursor, TerminalRenderRow, TerminalRenderUpdate, TerminalRenderUpdateOp, TerminalRenderUpdateOpKind},
        runtime::{RuntimeLayout, SessionMetadata, TerminalSize},
        vt::{self, VtEngine},
    };

    fn cached_row(row: u16, text: &str) -> TerminalRenderRow {
        TerminalRenderRow {
            row,
            col_count: 1,
            cells: vec![crate::provider::TerminalRenderCell { graphemes: text.chars().map(u32::from).collect(), ..Default::default() }],
            ..Default::default()
        }
    }

    fn cached_update(
        generation: u64,
        kind: TerminalRenderUpdateOpKind,
        rows: Vec<TerminalRenderRow>,
    ) -> crate::image_delivery::RenderBundle {
        crate::image_delivery::RenderBundle::live(
            TerminalRenderUpdate {
                render_generation: generation,
                cols: 1,
                rows: 3,
                ops: vec![TerminalRenderUpdateOp { kind, row_count: rows.len() as u16, col_count: 1, rows, ..Default::default() }],
                ..Default::default()
            },
            vec![],
        )
    }

    #[test]
    fn packet_cache_catches_up_each_client_without_cells_for_image_only_changes() {
        use TerminalRenderUpdateOpKind::{FullVisibleReplace, RowReplace};
        let mut cache = super::PacketRenderCache::default();
        cache.store(cached_update(1, FullVisibleReplace, vec![cached_row(0, "a"), cached_row(1, "b"), cached_row(2, "c")]));
        cache.store(cached_update(2, RowReplace, vec![cached_row(0, "A")]));
        cache.store(cached_update(3, RowReplace, vec![cached_row(2, "C")]));
        let mut image_only = cached_update(4, RowReplace, vec![]);
        image_only.packet.update.image_resources.push(crate::provider::TerminalImageResource {
            image_id: 42,
            generation: 4,
            ..Default::default()
        });
        cache.store(image_only);
        let slow = cache.since(1).unwrap().packet.update;
        assert_eq!(slow.ops.len(), 2);
        assert_eq!(slow.ops[0].rows[0], cached_row(0, "A"));
        assert_eq!(slow.ops[1].rows[0], cached_row(2, "C"));
        assert_eq!(cache.since(2).unwrap().packet.update.ops.len(), 1);
        let current = cache.since(3).unwrap().packet.update;
        assert!(current.ops.is_empty());
        assert_eq!(current.image_resources[0].image_id, 42);
        let initial = cache.since(0).unwrap().packet.update;
        assert_eq!(initial.ops[0].kind, FullVisibleReplace);
        assert_eq!(initial.ops[0].rows, vec![cached_row(0, "A"), cached_row(1, "b"), cached_row(2, "C")]);
    }

    #[test]
    fn packet_cache_materializes_overlapping_scrolls_and_resets_on_screen_change() {
        use TerminalRenderUpdateOpKind::{FullVisibleReplace, ScrollCopy};
        let mut cache = super::PacketRenderCache::default();
        cache.store(cached_update(1, FullVisibleReplace, vec![cached_row(0, "a"), cached_row(1, "b"), cached_row(2, "c")]));
        let mut scroll = cached_update(2, ScrollCopy, vec![]);
        scroll.packet.update.ops[0].src_row = 0;
        scroll.packet.update.ops[0].dst_row = 1;
        scroll.packet.update.ops[0].row_count = 2;
        cache.store(scroll);
        let caught_up = cache.since(1).unwrap().packet.update;
        assert_eq!(caught_up.ops[0].rows[0], cached_row(1, "a"));
        assert_eq!(caught_up.ops[1].rows[0], cached_row(2, "b"));
        let mut other_screen = cached_update(3, FullVisibleReplace, vec![cached_row(0, "x"), cached_row(1, "y"), cached_row(2, "z")]);
        other_screen.packet.update.terminal_modes.active_alternate_screen = true;
        cache.store(other_screen);
        let caught_up = cache.since(2).unwrap().packet.update;
        assert_eq!(caught_up.ops[0].kind, FullVisibleReplace);
        assert_eq!(caught_up.ops[0].rows[0], cached_row(0, "x"));
    }

    #[cfg(feature = "ghostty-vt")]
    #[test]
    fn packet_katzensteg_background_image_keeps_default_cells_transparent() {
        use crate::{
            image_delivery::CaptureImages,
            provider::{DirtyState, TerminalStyleColorTag},
            vt::ghostty::GhosttyVtEngine,
        };
        let mut source = GhosttyVtEngine::new(4, 2);
        source.set_cell_size(10, 20).unwrap();
        // Katzensteg places frames below non-default cell backgrounds.
        source.feed(b"\x1b_Ga=T,C=1,i=7,p=1,f=32,s=1,v=1,c=4,r=2,z=-1610612636;ESIz/w==\x1b\\\x1b[H \x1b[48;2;0;0;0m \x1b[44m \x1b[0m\x1b[2;1H\x1b[48;2;1;2;3m\x1b[K\x1b[0m").unwrap();
        let update = source.render_update(DirtyState::Full).unwrap();
        assert_eq!(update.ops[0].rows[0].cells[0].style.bg_color.tag, TerminalStyleColorTag::None);
        let images = CaptureImages::default()
            .capture(&update.image_resources, |id, generation, copy| source.with_image_resource_data(id, generation, copy))
            .unwrap();
        let mut renderer = PacketTerminalRenderer::new(4, 2);
        renderer.images.set_assets(images);
        let mut host = GhosttyVtEngine::new(4, 2);
        host.set_cell_size(10, 20).unwrap();
        let mut output = Vec::new();
        renderer.apply_and_render(&mut output, &update).unwrap();
        host.feed(&output).unwrap();
        let mut decoder = crate::attach_input::InputDecoder::new(0x1d);
        for action in decoder.feed(&host.drain_replies()) {
            if let crate::attach_input::Action::GraphicsReply(reply) = action {
                renderer.images.reply(&mut Vec::new(), &reply).unwrap();
            }
        }
        output.clear();
        renderer.repaint(&mut output).unwrap();
        host.feed(&output).unwrap();
        let displayed = host.render_update(DirtyState::Full).unwrap();
        assert_eq!(displayed.image_placements.len(), 1);
        assert_eq!(displayed.image_placements[0].z, -1610612636);
        assert_eq!(
            displayed.ops[0].rows[0].cells[0].style.bg_color.tag,
            TerminalStyleColorTag::None,
            "an explicit background hides Katzensteg's below-background image"
        );
        assert_eq!(
            displayed.ops[0].rows[0].cells[1].style.bg_color.tag,
            TerminalStyleColorTag::Rgb,
            "the application's explicit background must still occlude the image"
        );
        assert_eq!(displayed.ops[0].rows[0].cells[2].style.resolved_bg, update.ops[0].rows[0].cells[2].style.resolved_bg);
        for cell in &displayed.ops[0].rows[1].cells {
            assert_eq!(
                cell.style.resolved_bg,
                crate::provider::TerminalRgb { r: 1, g: 2, b: 3 },
                "erased coloured cells must retain their background"
            );
        }
    }

    #[cfg(feature = "ghostty-vt")]
    #[test]
    fn packet_images_round_trip_file_upload_replacement_and_pan_through_terminal_engine() {
        use crate::{
            image_delivery::CaptureImages,
            vt::{ghostty::GhosttyVtEngine, VtEngine},
        };
        let mut source = GhosttyVtEngine::new(10, 10);
        source.set_cell_size(10, 20).unwrap();
        let mut host = GhosttyVtEngine::new(12, 12);
        host.set_cell_size(10, 20).unwrap();
        let mut capture = CaptureImages::default();
        let mut renderer = PacketTerminalRenderer::new(10, 10);
        renderer.set_viewport((8, 8));
        for value in [17, 29] {
            let pixels = vec![value; 10 * 10 * 4];
            source
                .feed(
                    format!("\x1b[H\x1b_Ga=T,C=1,i=7,p=1,f=32,s=10,v=10,c=10,r=10;{}\x1b\\", crate::kitty_output::base64(&pixels))
                        .as_bytes(),
                )
                .unwrap();
            let update = source.render_update(crate::provider::DirtyState::Full).unwrap();
            assert_eq!(update.image_resources.len(), 1);
            let images = capture
                .capture(&update.image_resources, |id, generation, callback| source.with_image_resource_data(id, generation, callback))
                .unwrap();
            renderer.images.set_assets(images);
            let mut output = Vec::new();
            renderer.apply_and_render(&mut output, &update).unwrap();
            host.feed(&output).unwrap();
            let replies = host.drain_replies();
            let mut decoder = crate::attach_input::InputDecoder::new(0x1d);
            for action in decoder.feed(&replies) {
                if let crate::attach_input::Action::GraphicsReply(reply) = action {
                    renderer.images.reply(&mut Vec::new(), &reply).unwrap();
                } else {
                    panic!("unexpected reply {action:?}");
                }
            }
            output.clear();
            renderer.repaint(&mut output).unwrap();
            host.feed(&output).unwrap();
            let displayed = host.render_update(crate::provider::DirtyState::Full).unwrap();
            assert_eq!(displayed.image_placements.len(), 1);
            let resource = &displayed.image_resources[0];
            let mut actual = Vec::new();
            assert!(host
                .with_image_resource_data(resource.image_id, resource.generation, &mut |data| {
                    actual.extend_from_slice(data);
                    true
                })
                .unwrap());
            assert_eq!(actual, pixels);
            renderer.geometry.pan(2, 2);
            output.clear();
            renderer.repaint(&mut output).unwrap();
            host.feed(&output).unwrap();
            let displayed = host.render_update(crate::provider::DirtyState::Full).unwrap();
            assert_eq!(displayed.image_placements[0].source_x, 2);
            assert_eq!(displayed.image_placements[0].source_y, 2);
        }
        source.feed(b"\x1b_Ga=d,d=I,i=7;\x1b\\").unwrap();
        let update = source.render_update(crate::provider::DirtyState::Full).unwrap();
        renderer.images.set_assets(vec![]);
        let mut output = Vec::new();
        renderer.apply_and_render(&mut output, &update).unwrap();
        host.feed(&output).unwrap();
        assert!(host.render_update(crate::provider::DirtyState::Full).unwrap().image_placements.is_empty());
    }

    #[cfg(feature = "ghostty-vt")]
    #[test]
    fn packet_image_replacement_keeps_previous_frame_until_upload_reply() {
        use crate::{image_delivery::CaptureImages, provider::DirtyState, vt::ghostty::GhosttyVtEngine};
        let mut source = GhosttyVtEngine::new(4, 2);
        let mut host = GhosttyVtEngine::new(4, 2);
        source.set_cell_size(10, 20).unwrap();
        host.set_cell_size(10, 20).unwrap();
        let mut capture = CaptureImages::default();
        let mut renderer = PacketTerminalRenderer::new(4, 2);
        for (frame, id) in [7, 7, 8].into_iter().enumerate() {
            // Katzensteg transmits a fresh image, places it, then deletes the old one.
            source.feed(format!("\x1b[H\x1b_Ga=T,C=1,i={id},p=1,f=32,s=1,v=1,c=4,r=2,z=-1610612636;ESIz/w==\x1b\\").as_bytes()).unwrap();
            if id == 8 {
                source.feed(b"\x1b_Ga=d,d=I,i=7;\x1b\\").unwrap();
            }
            let update = source.render_update(DirtyState::Full).unwrap();
            let assets = capture
                .capture(&update.image_resources, |id, generation, copy| source.with_image_resource_data(id, generation, copy))
                .unwrap();
            renderer.images.set_assets(assets);
            let mut output = Vec::new();
            renderer.apply_and_render(&mut output, &update).unwrap();
            host.feed(&output).unwrap();
            if frame > 0 {
                assert_eq!(
                    host.render_update(DirtyState::Full).unwrap().image_placements.len(),
                    1,
                    "replacement upload must not expose a blank frame before its acknowledgement"
                );
            }
            let mut decoder = crate::attach_input::InputDecoder::new(0x1d);
            for action in decoder.feed(&host.drain_replies()) {
                if let crate::attach_input::Action::GraphicsReply(reply) = action {
                    renderer.images.reply(&mut Vec::new(), &reply).unwrap();
                }
            }
            output.clear();
            renderer.refresh_images(&mut output).unwrap();
            assert!(!output.windows(4).any(|bytes| bytes == b"\x1b[2K"), "image acknowledgement must not erase/repaint text rows");
            host.feed(&output).unwrap();
            let displayed = host.render_update(DirtyState::Full).unwrap();
            assert_eq!(displayed.image_placements.len(), 1);
            assert_eq!(displayed.image_resources.len(), 1, "retired frame must be released after replacement");
        }
    }

    #[cfg(feature = "ghostty-vt")]
    #[test]
    fn packet_placeholder_cells_become_resolved_image_placements() {
        use crate::{
            image_delivery::CaptureImages,
            vt::{ghostty::GhosttyVtEngine, VtEngine},
        };
        let mut source = GhosttyVtEngine::new(10, 10);
        source.set_cell_size(10, 20).unwrap();
        source.feed(b"\x1b_Ga=t,i=7,f=32,s=1,v=1;ESIz/w==\x1b\\\x1b_Ga=p,i=7,p=1,U=1,c=1,r=1;\x1b\\").unwrap();
        source.feed("\x1b[38;2;0;0;7m\u{10eeee}\u{0305}\u{0305}\x1b[0m".as_bytes()).unwrap();
        let update = source.render_update(crate::provider::DirtyState::Full).unwrap();
        assert_eq!(update.image_placements.len(), 1);
        assert_ne!(update.image_placements[0].flags & crate::provider::TERMINAL_IMAGE_PLACEMENT_VIRTUAL, 0);
        let images = CaptureImages::default()
            .capture(&update.image_resources, |id, generation, copy| source.with_image_resource_data(id, generation, copy))
            .unwrap();
        let mut renderer = PacketTerminalRenderer::new(10, 10);
        renderer.images.set_assets(images);
        let mut host = GhosttyVtEngine::new(10, 10);
        host.set_cell_size(10, 20).unwrap();
        let mut output = Vec::new();
        renderer.apply_and_render(&mut output, &update).unwrap();
        host.feed(&output).unwrap();
        let mut decoder = crate::attach_input::InputDecoder::new(0x1d);
        for reply in decoder.feed(&host.drain_replies()) {
            if let crate::attach_input::Action::GraphicsReply(reply) = reply {
                renderer.images.reply(&mut Vec::new(), &reply).unwrap();
            }
        }
        output.clear();
        renderer.repaint(&mut output).unwrap();
        host.feed(&output).unwrap();
        let actual = host.render_update(crate::provider::DirtyState::Full).unwrap();
        assert_eq!(actual.image_placements.len(), 1);
        assert!(host.screen_grid().unwrap().cells.iter().all(|cell| !cell.graphemes.contains(&0x10eeee)));
    }

    #[cfg(feature = "ghostty-vt")]
    #[test]
    fn packet_render_preserves_unchanged_background_during_repaint() {
        use crate::vt::ghostty::GhosttyVtEngine;
        let mut source = GhosttyVtEngine::new(8, 2);
        source.feed(b"\x1b[48;2;17;23;29m\x1b[2J\x1b[2;1H*").unwrap();
        let mut renderer = PacketTerminalRenderer::new(8, 2);
        let mut host = GhosttyVtEngine::new(8, 2);
        let initial = source.render_update(crate::provider::DirtyState::Full).unwrap();
        let mut output = Vec::new();
        renderer.apply_and_render(&mut output, &initial).unwrap();
        host.feed(&output).unwrap();
        let expected = host.screen_grid().unwrap().cells[..8].to_vec();

        source.feed(b"\x1b[2;1H+").unwrap();
        let update = source.render_update(crate::provider::DirtyState::Full).unwrap();
        output.clear();
        renderer.apply_and_render(&mut output, &update).unwrap();
        // An outer console may not preserve synchronized-output boundaries.
        // Observe every prefix: repainting must not erase this unchanged row
        // to the host's default background, even temporarily.
        let output = String::from_utf8(output).unwrap().replace("\x1b[?2026h", "").replace("\x1b[?2026l", "");
        for (offset, byte) in output.bytes().enumerate() {
            host.feed(&[byte]).unwrap();
            assert_eq!(host.screen_grid().unwrap().cells[..8], expected, "unchanged row flashed at output byte {offset}");
        }
        assert_eq!(host.screen_grid().unwrap().cells[8].graphemes, vec!['+' as u32]);
    }

    #[cfg(feature = "ghostty-vt")]
    #[test]
    fn packet_repaint_keeps_outer_cursor_visible_across_transport_chunks() {
        use crate::vt::ghostty::GhosttyVtEngine;
        let mut source = GhosttyVtEngine::new(20, 3);
        let mut host = GhosttyVtEngine::new(20, 3);
        let mut renderer = PacketTerminalRenderer::new(20, 3);
        source.feed(b"prompt> \x1b[?25h").unwrap();
        let mut bytes = Vec::new();
        renderer.apply_and_render(&mut bytes, &source.render_update(crate::provider::DirtyState::Full).unwrap()).unwrap();
        host.feed(&bytes).unwrap();
        assert!(host.render_update(crate::provider::DirtyState::Full).unwrap().cursor.visible);
        // The source only echoes a character: it never requests cursor hiding.
        source.feed(b"a").unwrap();
        let update = source.render_update(crate::provider::DirtyState::Partial).unwrap();
        assert!(update.cursor.visible);
        bytes.clear();
        renderer.apply_and_render(&mut bytes, &update).unwrap();
        let split = b"\x1b[?2026h\x1b[?25l".len();
        host.feed(&bytes[..split]).unwrap();
        let intermediate = host.render_update(crate::provider::DirtyState::Partial).unwrap();
        host.feed(&bytes[split..]).unwrap();
        let completed = host.render_update(crate::provider::DirtyState::Partial).unwrap();
        assert!(completed.cursor.visible);
        eprintln!(
            "source visible={}, outer mid-batch={}, outer completed={}",
            update.cursor.visible, intermediate.cursor.visible, completed.cursor.visible
        );
        assert!(intermediate.cursor.visible, "attach repaint exposed a hidden cursor although source cursor stayed visible");
    }

    #[test]
    fn packet_render_batches_repaint_with_cursor_hidden() {
        let mut renderer = PacketTerminalRenderer::new(2, 2);
        let update = TerminalRenderUpdate {
            cols: 2,
            rows: 2,
            cursor: TerminalCursor { col: 1, row: 1, visible: true, ..TerminalCursor::default() },
            ..TerminalRenderUpdate::default()
        };
        let mut output = Vec::new();

        renderer.apply_and_render(&mut output, &update).expect("render packet update");

        assert!(output.starts_with(b"\x1b[?2026h\x1b[?25l"), "repaint must start atomically with the cursor hidden: {output:?}");
        assert!(
            output.ends_with(b"\x1b[?25h\x1b[?7h\x1b[?2026l"),
            "cursor restoration must remain inside the synchronized batch: {output:?}"
        );
        let cursor_restore = output.windows(b"\x1b[?25h".len()).position(|bytes| bytes == b"\x1b[?25h").expect("cursor restore");
        let last_row_repaint = output.windows(b"\x1b[2;1H".len()).position(|bytes| bytes == b"\x1b[2;1H").expect("last row repaint");
        assert!(cursor_restore > last_row_repaint, "cursor must stay hidden throughout synthesized row movement");
    }

    #[cfg(feature = "ghostty-vt")]
    #[test]
    fn packet_render_clears_old_content_outside_shrunken_grid() {
        use crate::vt::ghostty::GhosttyVtEngine;
        let mut source = GhosttyVtEngine::new(4, 3);
        source.feed(b"\x1b[1;1HXXXX\x1b[2;1HXXXX\x1b[3;1HXXXX").unwrap();
        let mut renderer = PacketTerminalRenderer::new(4, 3);
        renderer.set_viewport((4, 3));
        let mut host = GhosttyVtEngine::new(4, 3);
        let mut output = Vec::new();
        renderer.apply_and_render(&mut output, &source.render_update(crate::provider::DirtyState::Full).unwrap()).unwrap();
        host.feed(&output).unwrap();

        let mut source = GhosttyVtEngine::new(2, 1);
        source.feed(b"OK").unwrap();
        output.clear();
        renderer.apply_and_render(&mut output, &source.render_update(crate::provider::DirtyState::Full).unwrap()).unwrap();
        host.feed(&output).unwrap();
        let grid = host.screen_grid().unwrap();
        assert_eq!(grid.cells[0].graphemes, vec!['O' as u32]);
        assert_eq!(grid.cells[1].graphemes, vec!['K' as u32]);
        assert!(grid.cells[2..].iter().all(|cell| cell.graphemes.iter().all(|c| *c == 0 || *c == 32)));
    }

    #[test]
    fn packet_render_clips_smaller_watcher_and_hides_offscreen_cursor() {
        use crate::provider::{TerminalCellWidth, TerminalRenderCell};
        let mut renderer = PacketTerminalRenderer::new(4, 2);
        renderer.set_viewport((2, 1));
        let mut wide = TerminalRenderCell { graphemes: vec!['界' as u32], ..Default::default() };
        wide.style.width = TerminalCellWidth::Wide;
        let update = TerminalRenderUpdate {
            cols: 4,
            rows: 2,
            cursor: TerminalCursor { col: 3, row: 1, visible: true, ..Default::default() },
            ops: vec![TerminalRenderUpdateOp {
                kind: TerminalRenderUpdateOpKind::FullVisibleReplace,
                rows: vec![
                    TerminalRenderRow {
                        row: 0,
                        cells: vec![TerminalRenderCell { graphemes: vec!['A' as u32], ..Default::default() }, wide],
                        ..Default::default()
                    },
                    TerminalRenderRow {
                        row: 1,
                        cells: vec![TerminalRenderCell { graphemes: vec!['Z' as u32], ..Default::default() }],
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut output = Vec::new();
        renderer.apply_and_render(&mut output, &update).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains('A'));
        assert!(!output.contains('界'));
        assert!(!output.contains('Z'));
        assert!(!output.contains("\x1b[2;1H"));
        assert!(!output.contains("\x1b[?25h"));
    }

    #[test]
    fn attachment_chrome_clamps_pan_and_maps_mouse_after_visibility_and_role_changes() {
        use crate::{
            packet::{ChannelRole, RoleState},
            provider::{TerminalModifiers, TerminalMouseButtons, TerminalMouseEvent, TerminalMouseEventKind},
        };
        let mut chrome = super::AttachChrome {
            session_name: "whatever3".to_owned(),
            nested_in: None,
            renderer: PacketTerminalRenderer::new(120, 40),
            panning: false,
            visible: false,
            hidden: false,
            role: RoleState {
                role: ChannelRole::Controller,
                controller: None,
                denial_reason: None,
                participants: vec![],
                exclusive: None,
                fixed_size: Some(crate::packet::Resize { cols: 120, rows: 40 }),
            },
            view: Default::default(),
            hint: None,
        };
        let update = TerminalRenderUpdate { cols: 120, rows: 40, ..Default::default() };
        chrome.paint_at_size(&mut Vec::new(), Some(&update), (80, 24)).unwrap();
        assert!(chrome.visible, "a clipped solo fixed-size driver needs indicators");
        chrome.renderer.geometry.pan(100, 100);
        assert_eq!((chrome.renderer.geometry.x, chrome.renderer.geometry.y), (40, 17));
        let mouse = |kind, col, row| TerminalMouseEvent {
            kind,
            cell_col: col,
            cell_row: row,
            x_px: 0.5,
            y_px: 0.5,
            button: None,
            buttons: TerminalMouseButtons::empty(),
            modifiers: TerminalModifiers::empty(),
            wheel_delta_x: 0.0,
            wheel_delta_y: 0.0,
        };
        for kind in
            [TerminalMouseEventKind::Press, TerminalMouseEventKind::Release, TerminalMouseEventKind::Move, TerminalMouseEventKind::Wheel]
        {
            assert_eq!(chrome.translate_mouse(mouse(kind, 1, 23), 24).is_none(), kind != TerminalMouseEventKind::Release);
            let mapped = chrome.translate_mouse(mouse(kind, 1, 2), 24).unwrap();
            assert_eq!((mapped.cell_col, mapped.cell_row), (41, 19));
            assert_eq!((mapped.x_px, mapped.y_px), (41.5, 19.5));
        }
        chrome.visible = false;
        chrome.hidden = true;
        chrome.hint = Some(("hidden hint".into(), Instant::now()));
        let mut output = Vec::new();
        chrome.paint_at_size(&mut output, None, (80, 24)).unwrap();
        assert!(!String::from_utf8(output).unwrap().contains("hidden hint"));
        assert_eq!((chrome.renderer.geometry.x, chrome.renderer.geometry.y), (40, 16));
        assert_eq!(chrome.translate_mouse(mouse(TerminalMouseEventKind::Press, 79, 23), 24).unwrap().cell_row, 39);
        chrome.role.role = ChannelRole::Watcher;
        chrome.paint_at_size(&mut Vec::new(), None, (130, 50)).unwrap();
        assert_eq!((chrome.renderer.geometry.x, chrome.renderer.geometry.y), (0, 0));
        assert!(chrome.translate_mouse(mouse(TerminalMouseEventKind::Press, 120, 0), 50).is_none());
        assert_eq!(chrome.translate_mouse(mouse(TerminalMouseEventKind::Release, 0, 40), 50).unwrap().cell_row, 39);
        chrome.renderer.mouse_cell_size = (10, 20);
        let mut precise = mouse(TerminalMouseEventKind::Press, 1, 2);
        precise.x_px = 1.25;
        precise.y_px = 2.75;
        let mapped = chrome.translate_mouse(precise, 50).unwrap();
        assert_eq!((mapped.x_px, mapped.y_px), (12.5, 55.0));
    }

    #[test]
    fn packet_render_pan_uses_cached_rows_and_translates_dirty_rows_and_cursor() {
        use crate::provider::{TerminalRenderCell, TerminalViewportKind};
        // The same crop applies to live, alternate-screen and retained history rows.
        for kind in [TerminalViewportKind::LiveNormal, TerminalViewportKind::LiveAlternate, TerminalViewportKind::NormalScrollback] {
            let mut renderer = PacketTerminalRenderer::new(4, 3);
            renderer.set_viewport((2, 1));
            let update = TerminalRenderUpdate {
                cols: 4,
                rows: 3,
                viewport_kind: kind,
                cursor: TerminalCursor { col: 3, row: 2, visible: true, ..Default::default() },
                ops: vec![TerminalRenderUpdateOp {
                    kind: TerminalRenderUpdateOpKind::FullVisibleReplace,
                    rows: vec![TerminalRenderRow {
                        row: 2,
                        cells: "ABCD".chars().map(|c| TerminalRenderCell { graphemes: vec![c as u32], ..Default::default() }).collect(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            };
            renderer.apply_and_render(&mut Vec::new(), &update).unwrap();
            renderer.geometry.pan(2, 2);
            let mut output = Vec::new();
            renderer.repaint(&mut output).unwrap();
            let output = String::from_utf8(output).unwrap();
            assert!(output.contains("mC") && output.contains("mD"), "{output:?}");
            assert!(output.contains("\x1b[1;2H"));
            assert!(output.contains("\x1b[?25h"));
            assert_eq!((renderer.geometry.x, renderer.geometry.y), (2, 2));

            let mut changed = update.clone();
            changed.ops[0].kind = TerminalRenderUpdateOpKind::RowReplace;
            changed.ops[0].rows[0].cells[2].graphemes = vec!['Z' as u32];
            let mut output = Vec::new();
            renderer.apply_and_render(&mut output, &changed).unwrap();
            let output = String::from_utf8(output).unwrap();
            assert!(output.contains("\x1b[1;1H\x1b[0m"));
            assert!(output.contains("mZ"));
            assert!(!output.contains("\x1b[3;1H"));
            assert_eq!((renderer.geometry.x, renderer.geometry.y), (2, 2));
        }
    }

    #[test]
    fn packet_render_blanks_wide_characters_cut_at_either_edge() {
        use crate::provider::{TerminalCellWidth, TerminalRenderCell};
        let mut wide = TerminalRenderCell { graphemes: vec!['界' as u32], ..Default::default() };
        wide.style.width = TerminalCellWidth::Wide;
        let mut tail = TerminalRenderCell::default();
        tail.style.width = TerminalCellWidth::SpacerTail;
        let mut renderer = PacketTerminalRenderer::new(5, 1);
        renderer.set_viewport((3, 1));
        let update = TerminalRenderUpdate {
            cols: 5,
            rows: 1,
            ops: vec![TerminalRenderUpdateOp {
                kind: TerminalRenderUpdateOpKind::FullVisibleReplace,
                rows: vec![TerminalRenderRow {
                    row: 0,
                    cells: vec![
                        wide.clone(),
                        tail.clone(),
                        TerminalRenderCell { graphemes: vec!['X' as u32], ..Default::default() },
                        wide,
                        tail,
                    ],
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        renderer.apply_and_render(&mut Vec::new(), &update).unwrap();
        renderer.geometry.pan(1, 0);
        let mut output = Vec::new();
        renderer.repaint(&mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("\x1b[1;1H\x1b[0m") && output.contains("mX"), "{output:?}");
        assert!(!output.contains('界'));
        #[cfg(feature = "ghostty-vt")]
        {
            let mut host = crate::vt::ghostty::GhosttyVtEngine::new(3, 1);
            host.feed(b"old").unwrap();
            host.feed(output.as_bytes()).unwrap();
            let grid = host.screen_grid().unwrap();
            assert_eq!(grid.cells.iter().map(|c| c.graphemes.clone()).collect::<Vec<_>>(), vec![vec![32], vec!['X' as u32], vec![32]]);
        }
    }

    #[cfg(feature = "ghostty-vt")]
    #[test]
    fn packet_render_prompt_stays_inside_shared_grid_border() {
        use crate::vt::{ghostty::GhosttyVtEngine, VtEngine};
        for (cols, prompt) in
            [(4, "☁️☁️r@"), (73, "~/dev/cleat on 🌱 main [⇡] via 🦀 v1.98.0 on ☁️  (eu-west-2) on ☁️  robert@changedirection.org")]
        {
            let mut source = GhosttyVtEngine::new(cols, 4);
            source.feed(prompt.as_bytes()).unwrap();
            let update = source.render_update(crate::provider::DirtyState::Full).unwrap();
            let host_cols = cols + 17;
            let mut renderer = PacketTerminalRenderer::new(cols, 4);
            renderer.set_viewport((host_cols, 6));
            renderer.bounds = true;
            let mut output = Vec::new();
            renderer.apply_and_render(&mut output, &update).unwrap();
            let mut host = GhosttyVtEngine::new(host_cols, 6);
            host.feed(b"\x1b[?2027h").unwrap();
            host.feed(&output).unwrap();
            let grid = host.screen_grid().unwrap();
            let cloud_count = grid.cells.iter().filter(|cell| cell.graphemes.contains(&0x2601)).count();
            assert_eq!(cloud_count, 2, "both cloud glyphs must survive rendering");
            let expected = source.screen_grid().unwrap();
            for row in 0..4usize {
                for col in 0..usize::from(cols) {
                    let expected_cell = &expected.cells[row * usize::from(cols) + col];
                    if expected_cell.graphemes.iter().any(|c| (33..127).contains(c)) {
                        assert_eq!(
                            grid.cells[row * usize::from(host_cols) + col].graphemes,
                            expected_cell.graphemes,
                            "ASCII shifted at ({col}, {row})"
                        );
                    }
                }
                for col in usize::from(cols + 1)..usize::from(host_cols) {
                    let cell = &grid.cells[row * usize::from(host_cols) + col];
                    assert!(
                        cell.graphemes.iter().all(|c| *c == 0 || *c == 32),
                        "text escaped the border at ({col}, {row}): {:?}",
                        cell.graphemes
                    );
                }
            }
        }
    }

    #[cfg(feature = "ghostty-vt")]
    #[test]
    fn packet_render_wider_host_grapheme_at_bottom_right_does_not_scroll() {
        use crate::vt::{ghostty::GhosttyVtEngine, VtEngine};
        let mut source = GhosttyVtEngine::new(4, 2);
        source.feed(b"\x1b[?2027l").unwrap();
        source.feed("safe\r\nabc☁️".as_bytes()).unwrap();
        let update = source.render_update(crate::provider::DirtyState::Full).unwrap();
        let mut renderer = PacketTerminalRenderer::new(4, 2);
        let mut output = Vec::new();
        renderer.apply_and_render(&mut output, &update).unwrap();
        let mut host = GhosttyVtEngine::new(4, 2);
        host.feed(b"\x1b[?2027h").unwrap();
        host.feed(&output).unwrap();
        assert_eq!(host.screen_grid().unwrap().row_text(0), "safe");
    }

    #[test]
    fn packet_render_bounds_use_only_spare_cells_and_can_be_hidden() {
        let mut renderer = PacketTerminalRenderer::new(2, 1);
        renderer.bounds = true;
        renderer.set_viewport((3, 2));
        let update = TerminalRenderUpdate { cols: 2, rows: 1, ..Default::default() };
        let mut output = Vec::new();
        renderer.apply_and_render(&mut output, &update).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("\x1b[1;3H│"));
        assert!(output.contains("\x1b[2;1H──┘"));
        renderer.bounds = false;
        let mut output = Vec::new();
        renderer.repaint(&mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("\x1b[2;1H\x1b[0m\x1b[K"));
        assert!(!output.contains(['│', '─', '┘']));
        renderer.bounds = true;
        renderer.set_viewport((2, 1));
        let mut output = Vec::new();
        renderer.repaint(&mut output).unwrap();
        assert!(!String::from_utf8(output).unwrap().contains(['│', '─', '┘']));
    }

    #[test]
    fn packet_render_repaints_only_replaced_rows() {
        let mut renderer = PacketTerminalRenderer::new(8, 4);
        let initial = TerminalRenderUpdate { cols: 8, rows: 4, ..TerminalRenderUpdate::default() };
        let mut initial_output = Vec::new();
        renderer.apply_and_render(&mut initial_output, &initial).expect("render initial packet update");

        let update = TerminalRenderUpdate {
            cols: 8,
            rows: 4,
            ops: vec![TerminalRenderUpdateOp {
                kind: TerminalRenderUpdateOpKind::RowReplace,
                rows: vec![TerminalRenderRow { row: 2, ..TerminalRenderRow::default() }],
                ..TerminalRenderUpdateOp::default()
            }],
            ..TerminalRenderUpdate::default()
        };
        let mut output = Vec::new();
        renderer.apply_and_render(&mut output, &update).expect("render dirty packet row");

        assert_eq!(output.windows(b";1H\x1b[0m".len()).filter(|bytes| *bytes == b";1H\x1b[0m").count(), 1, "{output:?}");
        assert!(output.windows(b"\x1b[3;1H".len()).any(|bytes| bytes == b"\x1b[3;1H"), "{output:?}");
        assert!(output.len() < initial_output.len() / 2, "single-row output should be proportional to one row");
    }

    #[test]
    fn packet_render_repaints_only_scroll_copy_destinations() {
        let mut renderer = PacketTerminalRenderer::new(2, 4);
        let base = TerminalRenderUpdate { cols: 2, rows: 4, ..TerminalRenderUpdate::default() };
        renderer.apply_and_render(&mut Vec::new(), &base).expect("render initial frame");
        let update = TerminalRenderUpdate {
            ops: vec![TerminalRenderUpdateOp {
                kind: TerminalRenderUpdateOpKind::ScrollCopy,
                src_row: 0,
                dst_row: 1,
                row_count: 2,
                ..TerminalRenderUpdateOp::default()
            }],
            ..base
        };
        let mut output = Vec::new();
        renderer.apply_and_render(&mut output, &update).expect("render scroll copy");

        assert_eq!(output.windows(b";1H\x1b[0m".len()).filter(|bytes| *bytes == b";1H\x1b[0m").count(), 2, "{output:?}");
        assert!(output.windows(b"\x1b[2;1H".len()).any(|bytes| bytes == b"\x1b[2;1H"), "{output:?}");
        assert!(output.windows(b"\x1b[3;1H".len()).any(|bytes| bytes == b"\x1b[3;1H"), "{output:?}");
    }

    #[test]
    fn packet_render_repaints_every_row_for_full_repaint_triggers() {
        fn painted_rows(output: &[u8]) -> usize {
            output.windows(b";1H\x1b[0m".len()).filter(|bytes| *bytes == b";1H\x1b[0m").count()
        }

        let base = TerminalRenderUpdate { cols: 2, rows: 2, ..TerminalRenderUpdate::default() };

        let mut resized_renderer = PacketTerminalRenderer::new(1, 1);
        let mut resized_output = Vec::new();
        resized_renderer.apply_and_render(&mut resized_output, &base).expect("render resize");
        assert_eq!(painted_rows(&resized_output), 2);

        let mut mode_renderer = PacketTerminalRenderer::new(2, 2);
        mode_renderer.apply_and_render(&mut Vec::new(), &base).expect("render initial frame");
        let mut mode_update = base.clone();
        mode_update.terminal_modes.application_cursor_keys = true;
        let mut mode_output = Vec::new();
        mode_renderer.apply_and_render(&mut mode_output, &mode_update).expect("render mode change");
        assert_eq!(painted_rows(&mode_output), 2);

        let mut full_renderer = PacketTerminalRenderer::new(2, 2);
        full_renderer.apply_and_render(&mut Vec::new(), &base).expect("render initial frame");
        let mut full_update = base;
        full_update.ops =
            vec![TerminalRenderUpdateOp { kind: TerminalRenderUpdateOpKind::FullVisibleReplace, ..TerminalRenderUpdateOp::default() }];
        let mut full_output = Vec::new();
        full_renderer.apply_and_render(&mut full_output, &full_update).expect("render full replacement");
        assert_eq!(painted_rows(&full_output), 2);
    }

    #[cfg(unix)]
    #[test]
    fn attach_foreground_uses_http_upgrade_request() {
        use std::{fs, os::unix::net::UnixListener, sync::mpsc, thread, time::Duration};

        let temp = tempfile::tempdir().expect("tempdir");
        let layout = RuntimeLayout::new(temp.path().to_path_buf());
        layout.ensure_daemon_dirs().expect("create daemon dirs");
        fs::create_dir_all(layout.session_dir("alpha")).expect("create session dir");
        let socket_path = session_socket_path(temp.path(), "alpha");
        let listener = UnixListener::bind(&socket_path).expect("bind socket");
        let (tx, rx) = mpsc::channel();
        let reader = thread::spawn(move || {
            use std::io::Write;

            let (mut stream, _) = listener.accept().expect("accept connection");
            let request = read_http_request_for_test(&mut stream);
            tx.send(request).expect("send request");
            stream
                .write_all(b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: cleat-attach/1\r\n\r\n")
                .expect("write response");
        });

        let attach = attach_foreground(&layout, "alpha", AttachmentIdentity::default(), false, false).expect("attach");
        drop(attach);
        let request = rx.recv_timeout(Duration::from_secs(1)).expect("receive request");

        reader.join().expect("join reader");
        assert!(request.starts_with("POST /sessions/alpha/attach HTTP/1.1\r\n"), "{request}");
        assert!(request.contains("Connection: Upgrade\r\n"), "{request}");
        assert!(request.contains("Upgrade: cleat-attach/1\r\n"), "{request}");
        // Capabilities are detected from the ambient environment, so compare
        // against the serialization of whatever detection currently reports.
        let expected_capabilities =
            serde_json::to_string(&super::attach_capabilities_to_http(attach_init_capabilities())).expect("serialize capabilities");
        assert!(request.contains(&format!(r#""capabilities":{expected_capabilities}"#)), "{request}");
        assert!(request.contains(r#""identity":{"kind":"principal","name":""}"#), "{request}");
    }

    #[test]
    fn cleanup_guard_writes_on_drop() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let guard = AttachCleanupGuard::test_buffer(Arc::clone(&output));

        drop(guard);

        assert_eq!(
            *output.lock().expect("lock output"),
            b"\x1b_Ga=d,d=A,q=2;\x1b\\\x1b[?2026l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1016l\x1b[?2004l\x1b[?1004l\x1b[<u\x1b[r\x1b[0m\x1b[?25h\x1b[2J\x1b[H\x1b[?1049l"
        );
    }

    #[test]
    fn cleanup_writes_fixed_reset_sequence_when_emitted() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut guard = AttachCleanupGuard::test_buffer(Arc::clone(&output));

        guard.emit().expect("emit cleanup");
        drop(guard);

        assert_eq!(
            *output.lock().expect("lock output"),
            b"\x1b_Ga=d,d=A,q=2;\x1b\\\x1b[?2026l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1016l\x1b[?2004l\x1b[?1004l\x1b[<u\x1b[r\x1b[0m\x1b[?25h\x1b[2J\x1b[H\x1b[?1049l"
        );
    }

    #[test]
    fn watcher_chrome_reserves_the_last_row_and_names_the_controller() {
        let mut output = Vec::new();
        super::render_seat_chrome_at_rows(
            &mut output,
            &crate::protocol::SeatState {
                role: "watcher".to_string(),
                controller: Some(crate::protocol::AttachmentIdentity {
                    kind: crate::protocol::AttachmentKind::Supervisor,
                    name: "crew-runner".to_string(),
                }),
            },
            24,
        )
        .expect("render watcher chrome");

        let output = String::from_utf8(output).expect("watcher chrome is utf8");
        assert!(output.contains("\u{1b}[1;23r"));
        assert!(output.contains("\u{1b}[24;1H"));
        assert!(output.contains("watching — controller: crew-runner"));
    }

    #[test]
    fn watcher_chrome_strips_control_sequences_from_controller_identity() {
        let mut output = Vec::new();
        super::render_seat_chrome_at_rows(
            &mut output,
            &crate::protocol::SeatState {
                role: "watcher".to_string(),
                controller: Some(crate::protocol::AttachmentIdentity {
                    kind: crate::protocol::AttachmentKind::Tool,
                    name: "bad\u{1b}]2;injected\u{7}\nname".to_string(),
                }),
            },
            24,
        )
        .expect("render sanitized watcher chrome");

        let output = String::from_utf8(output).expect("watcher chrome is utf8");
        assert!(!output.contains("\u{1b}]2;injected"));
        assert!(!output.contains('\u{7}'));
        assert!(!output.contains('\n'));
        assert!(output.contains("watching — controller: bad]2;injectedname"));
    }

    #[test]
    fn controller_output_does_not_render_watcher_chrome() {
        let mut output = Vec::new();
        super::write_attach_output(&mut output, b"last terminal row", None).expect("relay controller output");

        assert_eq!(output, b"last terminal row");
    }

    #[test]
    fn cleanup_does_not_write_when_target_is_disabled() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut guard = AttachCleanupGuard::test_buffer_disabled(Arc::clone(&output));

        guard.emit().expect("emit cleanup");
        drop(guard);

        assert!(output.lock().expect("lock output").is_empty());
    }

    #[test]
    fn graceful_socket_shutdown_classifies_broken_pipe_disconnects() {
        let err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "broken pipe");
        assert!(super::is_graceful_socket_shutdown(&err));
    }

    #[test]
    fn interrupted_socket_operations_are_retried() {
        let mut attempts = 0;

        let value = super::retry_interrupted(|| {
            attempts += 1;
            if attempts < 3 {
                Err(io::Error::new(io::ErrorKind::Interrupted, "injected interrupt"))
            } else {
                Ok(7)
            }
        })
        .expect("operation should succeed after transient interrupts");

        assert_eq!(value, 7);
        assert_eq!(attempts, 3);
    }

    #[cfg(unix)]
    #[test]
    fn packet_client_backlog_overflow_marks_client_dead_not_daemon_fatal() {
        let (stream, _peer) = std::os::unix::net::UnixStream::pair().expect("unix stream pair");
        let mut client =
            super::PacketClient::new(1, stream, Vec::new(), None, &crate::packet::DirectorySnapshot { sessions: Vec::new() }, None)
                .expect("create packet client");
        client.pending_output = super::PendingOutput::from(vec![0; super::MAX_PENDING_CLIENT_OUTPUT_BYTES - 1]);

        let frame = crate::packet::PacketFrame { channel: 1, msg_type: 0, payload: vec![0; 64] };
        client.enqueue_frame(&frame).expect("overflow must not surface as a daemon-level error");

        assert!(client.dead, "overflowing client should be marked dead for reaping");
        assert!(client.pending_output.is_empty(), "backlog should be released");

        client.enqueue_frame(&frame).expect("enqueue to a dead client is a no-op");
        assert!(client.pending_output.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn dead_packet_client_does_not_hold_the_controller_slot() {
        let (stream, _peer) = std::os::unix::net::UnixStream::pair().expect("unix stream pair");
        let mut client =
            super::PacketClient::new(7, stream, Vec::new(), None, &crate::packet::DirectorySnapshot { sessions: Vec::new() }, None)
                .expect("create packet client");
        let holder = super::PacketChannelRef { client_id: 7, channel: 1 };

        assert!(super::packet_controller_holder_is_live(holder, std::slice::from_ref(&client)));

        client.dead = true;
        assert!(!super::packet_controller_holder_is_live(holder, std::slice::from_ref(&client)));
        assert!(!super::packet_controller_holder_is_live(holder, &[]), "a reaped holder is not live either");
    }

    #[test]
    fn contain_session_unwind_contains_errors_as_faults() {
        let contained = super::contain_session_unwind("alpha", || -> Result<(), String> { Err("actor died".into()) });
        assert!(contained.is_none(), "a servicing error should fault the session, not the daemon");

        let ok = super::contain_session_unwind("alpha", || Ok(7));
        assert_eq!(ok, Some(7));
    }

    #[test]
    fn pending_output_partial_consumes_preserve_byte_order_without_upfront_memmove() {
        let mut pending = super::PendingOutput::new();
        pending.write_all(b"abcdefgh").expect("queue bytes");

        pending.consume(3);
        assert_eq!(pending.as_slice(), b"defgh");
        assert_eq!(pending.len(), 5);
        // Consumed less than remains: the cursor advances, nothing moves yet.
        assert_eq!(pending.start, 3);

        pending.write_all(b"ij").expect("queue more bytes");
        assert_eq!(pending.as_slice(), b"defghij");

        // Dead prefix (7) now exceeds the remainder (3): compaction fires.
        pending.consume(4);
        assert_eq!(pending.as_slice(), b"hij");
        assert_eq!(pending.start, 0);

        pending.consume(3);
        assert!(pending.is_empty());
        assert_eq!(pending.start, 0);
    }

    /// The zero-copy output path must produce byte-identical backlogs to the
    /// generic `Frame` path it replaces (issue #135).
    #[cfg(unix)]
    #[test]
    fn enqueue_output_frames_bytes_identically_to_enqueue_frame() {
        let (stream_a, _peer_a) = std::os::unix::net::UnixStream::pair().expect("unix stream pair");
        let (stream_b, _peer_b) = std::os::unix::net::UnixStream::pair().expect("unix stream pair");
        let capabilities = crate::vt::ClientCapabilities::conservative_fallback();
        let mut via_output = super::ActiveClient::new(stream_a, capabilities, AttachmentIdentity::default()).expect("create active client");
        let mut via_frame = super::ActiveClient::new(stream_b, capabilities, AttachmentIdentity::default()).expect("create active client");

        let payload = b"\x1b[1mchunk\x00\xff";
        via_output.enqueue_output(payload).expect("enqueue output payload");
        via_frame.enqueue_frame(&super::Frame::Output(payload.to_vec())).expect("enqueue output frame");

        assert_eq!(via_output.pending_output.as_slice(), via_frame.pending_output.as_slice());
    }

    #[cfg(unix)]
    #[test]
    fn active_client_rejects_unbounded_output_backlog() {
        let (stream, _peer) = std::os::unix::net::UnixStream::pair().expect("unix stream pair");
        let mut client =
            super::ActiveClient::new(stream, crate::vt::ClientCapabilities::conservative_fallback(), AttachmentIdentity::default())
                .expect("create active client");
        client.pending_output = super::PendingOutput::from(vec![0; super::MAX_PENDING_CLIENT_OUTPUT_BYTES - 1]);

        let err = client.enqueue_frame(&super::Frame::Output(vec![1])).expect_err("backlog should overflow");
        assert!(err.contains("client output backlog exceeded"));

        let err = client.enqueue_output(&[1]).expect_err("zero-copy path enforces the same backlog cap");
        assert!(err.contains("client output backlog exceeded"));
    }

    #[test]
    fn default_vt_engine_starts_with_default_size() {
        let session = SessionMetadata {
            id: "test".to_string(),
            vt_engine: vt::default_vt_engine_kind(),
            cwd: None,
            cmd: None,
            tags: Vec::new(),
            environment: Vec::new(),
            record: false,
            initial_size: TerminalSize::default(),
            colors: vt::TerminalColors::default(),
        };
        let engine = default_vt_engine(&session).expect("create default vt engine");
        assert_eq!(engine.size(), (crate::runtime::DEFAULT_TERMINAL_COLS, crate::runtime::DEFAULT_TERMINAL_ROWS));
        #[cfg(feature = "ghostty-vt")]
        assert!(engine.supports_replay());
        #[cfg(not(feature = "ghostty-vt"))]
        assert!(!engine.supports_replay());
        #[cfg(feature = "ghostty-vt")]
        assert!(engine.replay_payload(&vt::ClientCapabilities::conservative_fallback()).expect("replay payload").is_some());
        #[cfg(not(feature = "ghostty-vt"))]
        assert_eq!(engine.replay_payload(&vt::ClientCapabilities::conservative_fallback()).expect("replay payload"), None);
    }

    #[test]
    fn vt_engine_helpers_feed_and_resize_default_engine() {
        let session = SessionMetadata {
            id: "test".to_string(),
            vt_engine: vt::default_vt_engine_kind(),
            cwd: None,
            cmd: None,
            tags: Vec::new(),
            environment: Vec::new(),
            record: false,
            initial_size: TerminalSize { cols: 120, rows: 40 },
            colors: vt::TerminalColors::default(),
        };
        let mut engine = default_vt_engine(&session).expect("create default vt engine");
        assert_eq!(engine.size(), (120, 40));
        record_pty_output(engine.as_mut(), b"hello").expect("feed output");
        let replay =
            apply_attach_state(engine.as_mut(), 132, 40, &vt::ClientCapabilities::conservative_fallback()).expect("apply attach state");

        assert_eq!(engine.size(), (132, 40));
        #[cfg(feature = "ghostty-vt")]
        assert!(replay.is_some());
        #[cfg(not(feature = "ghostty-vt"))]
        assert_eq!(replay, None);
    }

    #[test]
    fn lifecycle_attach_init_capabilities_detect_from_environment() {
        // Detection reads the ambient environment, so assert the wiring rather
        // than a specific level; the pure detection logic is unit-tested in
        // `vt::tests`.
        assert_eq!(attach_init_capabilities(), vt::ClientCapabilities::detect());
    }

    fn screen_stable_fingerprint_with_cells(cell_count: usize, changed_prefix: usize) -> ScreenStableFingerprint {
        let cells = (0..cell_count)
            .map(|index| crate::provider::TerminalCell {
                graphemes: vec![if index < changed_prefix { 'x' as u32 } else { 'a' as u32 }],
                ..crate::provider::TerminalCell::default()
            })
            .collect();
        ScreenStableFingerprint {
            cols: cell_count as u16,
            rows: 1,
            viewport_kind: crate::provider::TerminalViewportKind::LiveNormal,
            scrollback_offset_rows: 0,
            cells,
        }
    }

    #[test]
    fn screen_stable_tolerates_small_rendered_cell_churn() {
        let baseline = screen_stable_fingerprint_with_cells(80, 0);
        let spinner_tick = screen_stable_fingerprint_with_cells(80, SCREEN_STABLE_CHANGED_CELL_TOLERANCE);

        assert!(!baseline.significant_change_from(&spinner_tick));
    }

    #[test]
    fn screen_stable_resets_on_large_rendered_cell_change() {
        let baseline = screen_stable_fingerprint_with_cells(80, 0);
        let changed = screen_stable_fingerprint_with_cells(80, SCREEN_STABLE_CHANGED_CELL_TOLERANCE + 1);

        assert!(baseline.significant_change_from(&changed));
    }

    #[test]
    fn screen_stable_resets_on_geometry_change() {
        let baseline = screen_stable_fingerprint_with_cells(80, 0);
        let mut resized = baseline.clone();
        resized.cols = 40;

        assert!(baseline.significant_change_from(&resized));
    }

    #[test]
    fn screen_stable_observe_only_resets_timestamp_for_significant_changes() {
        let baseline = screen_stable_fingerprint_with_cells(80, 0);
        let mut state = ScreenStableState::new(baseline, Instant::now());
        let original_stable_since = state.stable_since;

        state.observe(screen_stable_fingerprint_with_cells(80, 1), original_stable_since + Duration::from_millis(100));
        assert_eq!(state.stable_since, original_stable_since);

        let reset_at = original_stable_since + Duration::from_millis(200);
        state.observe(screen_stable_fingerprint_with_cells(80, SCREEN_STABLE_CHANGED_CELL_TOLERANCE + 1), reset_at);
        assert_eq!(state.stable_since, reset_at);
    }

    #[test]
    fn screen_stable_snapshot_gating_tracks_render_generation() {
        // No fingerprint taken yet: always snapshot.
        assert!(super::screen_stable_needs_snapshot(0, None));
        // Nothing rendered since the last fingerprint: the screen cannot have
        // changed, skip the snapshot and let the stability window age.
        assert!(!super::screen_stable_needs_snapshot(7, Some(7)));
        // A pump advanced the generation: re-fingerprint.
        assert!(super::screen_stable_needs_snapshot(8, Some(7)));
    }

    #[test]
    fn lifecycle_apply_attach_state_uses_attach_capabilities_for_replay() {
        let mut engine = TestReplayProbeVtEngine::new(80, 24);
        let capabilities = vt::ClientCapabilities::new(vt::ColorLevel::Ansi256, true);

        let replay = apply_attach_state(&mut engine, 100, 30, &capabilities).expect("apply attach state");

        assert_eq!(engine.size(), (100, 30));
        assert_eq!(replay, Some(b"Ansi256:true".to_vec()));
    }

    #[test]
    fn client_install_plan_sends_snapshot_covered_chunks_only_to_existing_clients() {
        let (tap_tx, tap) = crate::host::actor::RawOutputTap::test_channel(2);
        tap_tx
            .send(crate::host::actor::RawOutputChunk { sequence: 1, bytes: b"line2\n".to_vec().into() })
            .expect("queue snapshot-covered output");
        tap_tx
            .send(crate::host::actor::RawOutputChunk { sequence: 2, bytes: b"line3\n".to_vec().into() })
            .expect("queue post-snapshot output");

        let plan = super::plan_raw_output_client_install(
            &tap,
            crate::host::actor::RawOutputReplay { payload: Some(b"line2\n".to_vec()), through_sequence: 1 },
            super::ReplayMode::FreshTerminal,
        );

        let super::RawOutputClientInstallPlan::Complete { existing_chunks, new_client_frames } = plan else {
            panic!("connected tap should produce a complete install plan");
        };
        assert_eq!(existing_chunks, vec![Arc::from(&b"line2\n"[..]), Arc::from(&b"line3\n"[..])]);
        assert_eq!(new_client_frames, vec![
            crate::protocol::Frame::Output(b"line2\n".to_vec()),
            crate::protocol::Frame::Output(b"line3\n".to_vec())
        ]);
    }

    #[test]
    fn resize_burst_is_coalesced_to_the_latest_geometry_without_reordering_input() {
        let mut frames = VecDeque::from([
            crate::protocol::Frame::Resize { cols: 94, rows: 24 },
            crate::protocol::Frame::Resize { cols: 95, rows: 30 },
            crate::protocol::Frame::Resize { cols: 99, rows: 48 },
            crate::protocol::Frame::Input(b"x".to_vec()),
            crate::protocol::Frame::Resize { cols: 100, rows: 49 },
            crate::protocol::Frame::Resize { cols: 101, rows: 50 },
        ]);

        super::coalesce_resize_bursts(&mut frames);

        assert_eq!(
            frames,
            VecDeque::from([
                crate::protocol::Frame::Resize { cols: 99, rows: 48 },
                crate::protocol::Frame::Input(b"x".to_vec()),
                crate::protocol::Frame::Resize { cols: 101, rows: 50 },
            ])
        );
    }

    struct FakeRawOutputRecoverySource {
        seen_capabilities: RefCell<Vec<vt::ClientCapabilities>>,
        payloads: Vec<Option<Vec<u8>>>,
    }

    impl super::RawOutputRecoverySource for FakeRawOutputRecoverySource {
        fn recover_raw_output(&self, capabilities: Vec<vt::ClientCapabilities>) -> Result<crate::host::actor::RawOutputRecovery, String> {
            self.seen_capabilities.replace(capabilities);
            let (_tap_tx, tap) = crate::host::actor::RawOutputTap::test_channel(1);
            Ok(crate::host::actor::RawOutputRecovery { tap, payloads: self.payloads.clone() })
        }
    }

    #[test]
    fn disconnected_tap_recovers_with_terminal_reset_and_fresh_replay() {
        let (tap_tx, tap) = crate::host::actor::RawOutputTap::test_channel(1);
        tap_tx
            .send(crate::host::actor::RawOutputChunk { sequence: 1, bytes: b"queued\n".to_vec().into() })
            .expect("queue output before overflow");
        drop(tap_tx);
        let install = super::plan_raw_output_client_install(
            &tap,
            crate::host::actor::RawOutputReplay { payload: Some(b"stale snapshot".to_vec()), through_sequence: 0 },
            super::ReplayMode::FreshTerminal,
        );
        let super::RawOutputClientInstallPlan::Disconnected { existing_chunks } = install else {
            panic!("dropped tap sender should require recovery");
        };
        assert_eq!(existing_chunks, vec![Arc::from(&b"queued\n"[..])]);

        let capabilities = vt::ClientCapabilities::new(vt::ColorLevel::Ansi256, true);
        let source = FakeRawOutputRecoverySource {
            seen_capabilities: RefCell::new(Vec::new()),
            payloads: vec![Some(b"fresh snapshot with OVERFLOW_END".to_vec())],
        };
        let recovery = super::plan_raw_output_recovery(&source, &[super::RawOutputRecoveryRequest {
            capabilities,
            replay_mode: super::ReplayMode::ResetTerminal,
        }])
        .expect("plan recovery");

        assert_eq!(*source.seen_capabilities.borrow(), vec![capabilities]);
        assert_eq!(recovery.recipient_frames, vec![vec![
            crate::protocol::Frame::Output(super::REATTACH_CLEAR_SEQUENCE.to_vec()),
            crate::protocol::Frame::Output(b"fresh snapshot with OVERFLOW_END".to_vec())
        ]]);
    }

    #[cfg(not(feature = "ghostty-vt"))]
    #[test]
    fn vt_engine_helpers_compile_without_ghostty_feature() {
        let mut engine = vt::make_default_vt_engine(80, 24);

        record_pty_output(engine.as_mut(), b"hello").expect("feed output");
        let replay =
            apply_attach_state(engine.as_mut(), 100, 30, &vt::ClientCapabilities::conservative_fallback()).expect("apply attach state");

        assert_eq!(engine.size(), (100, 30));
        assert_eq!(replay, None);
    }
}

#[cfg(all(test, unix))]
#[path = "session_packet_output_tests.rs"]
mod packet_output_tests;

#[cfg(all(test, feature = "ghostty-vt"))]
mod packet_keyboard_tests {
    use super::*;
    use crate::{
        attach_keyboard::KeyboardMode,
        vt::{ghostty::GhosttyVtEngine, VtEngine},
    };

    #[test]
    fn packet_keyboard_restores_outer_stack_across_screen_changes_and_cleanup() {
        let mut outer = GhosttyVtEngine::new(80, 24);
        outer.feed(b"\x1b[>5u").unwrap();
        let keyboard = Arc::new(Mutex::new(KeyboardMode::default()));
        let mut renderer = PacketTerminalRenderer::new(80, 24);
        renderer.keyboard = Some(keyboard.clone());
        let mut bytes = Vec::new();
        keyboard.lock().unwrap().enable(&mut bytes).unwrap();
        outer.feed(&bytes).unwrap();
        assert_eq!(outer.drain_replies(), b"\x1b[?31u");
        for alternate in [true, false, true, false, true] {
            bytes.clear();
            renderer
                .apply_and_render(&mut bytes, &TerminalRenderUpdate {
                    cols: 80,
                    rows: 24,
                    terminal_modes: vt::TerminalModeState { active_alternate_screen: alternate, ..Default::default() },
                    ..Default::default()
                })
                .unwrap();
            outer.feed(&bytes).unwrap();
            assert_eq!(outer.drain_replies(), b"\x1b[?31u");
        }
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut cleanup = AttachCleanupGuard::test_buffer(output.clone());
        cleanup.keyboard = Some(keyboard);
        cleanup.emit().unwrap();
        cleanup.emit().unwrap();
        outer.feed(&output.lock().unwrap()).unwrap();
        outer.feed(crate::attach_keyboard::QUERY).unwrap();
        assert_eq!(outer.drain_replies(), b"\x1b[?5u");
    }

    #[test]
    fn decoded_pixel_positions_encode_for_cell_and_pixel_applications() {
        let mut mode = crate::attach_mouse::MouseMode::default();
        mode.set_pixel_origin(0);
        mode.reply(b"\x1b[6;20;10t");
        mode.reply(b"\x1b[?1016;2$y");
        mode.reply(b"\x1b[?1016;1$y");
        let event = mode.decode(b"\x1b[<0;16;26M").unwrap();
        for (format, expected) in [(b"\x1b[?1006h".as_slice(), b"\x1b[<0;2;2M".as_slice()), (b"\x1b[?1016h", b"\x1b[<0;16;26M")] {
            let mut app = GhosttyVtEngine::new(80, 24);
            app.set_cell_size(10, 20).unwrap();
            app.feed(b"\x1b[?1000h").unwrap();
            app.feed(format).unwrap();
            let bytes = app
                .encode_mouse(
                    vt::MouseAction::Press,
                    Some(vt::MouseButton::Left),
                    true,
                    Default::default(),
                    event.x_px * 10.0,
                    event.y_px * 20.0,
                )
                .unwrap();
            assert_eq!(bytes, expected);
            let wheel = crate::host::actor::mouse_report_bytes_from_wheel(
                SessionWheelEvent {
                    modifiers: Default::default(),
                    cell_col: event.cell_col,
                    cell_row: event.cell_row,
                    x_px: event.x_px * 10.0,
                    y_px: event.y_px * 20.0,
                    wheel_delta_x: 0.0,
                    wheel_delta_y: 1.0,
                },
                app.terminal_mode_state().unwrap(),
            )
            .unwrap();
            let expected_wheel = String::from_utf8(expected.to_vec()).unwrap().replacen("<0;", "<64;", 1);
            assert_eq!(wheel, expected_wheel.as_bytes());
        }
    }

    #[test]
    fn pixel_mouse_format_survives_application_mode_changes_and_cleanup_disables_it() {
        let mut outer = GhosttyVtEngine::new(80, 24);
        outer.feed(b"\x1b[?1006h\x1b[?1016h").unwrap();
        let mut previous = vt::TerminalModeState::default();
        for tracking in [vt::MouseTrackingMode::Normal, vt::MouseTrackingMode::Any, vt::MouseTrackingMode::None] {
            let current = vt::TerminalModeState {
                mouse_tracking_mode: tracking,
                active_alternate_screen: !previous.active_alternate_screen,
                ..Default::default()
            };
            let mut bytes = Vec::new();
            render_packet_terminal_modes(&mut bytes, previous, current).unwrap();
            outer.feed(&bytes).unwrap();
            assert_eq!(outer.terminal_mode_state().unwrap().mouse_report_format, vt::MouseReportFormat::SgrPixels);
            previous = current;
        }
        let mut bytes = Vec::new();
        write_detach_cleanup(&mut bytes).unwrap();
        outer.feed(&bytes).unwrap();
        assert!(!outer.terminal_mode_state().unwrap().mouse_sgr_pixels);
    }

    #[test]
    fn outer_kitty_events_are_reencoded_for_each_applications_current_mode() {
        use crate::attach_input::{Action, InputDecoder};
        let mut legacy = GhosttyVtEngine::new(80, 24);
        let mut kitty = GhosttyVtEngine::new(80, 24);
        kitty.feed(b"\x1b[>3u").unwrap();
        let mut decoder = InputDecoder::new(0x1d);
        decoder.set_keyboard_flags(31);
        let mut legacy_bytes = Vec::new();
        let mut kitty_bytes = Vec::new();
        for action in decoder.feed(b"\x1b[97;5u\x1b[97;5:2u\x1b[97;1:3u") {
            let Action::Key(event) = action else { panic!("expected key") };
            legacy_bytes.extend(legacy.encode_key(&event).unwrap());
            kitty_bytes.extend(kitty.encode_key(&event).unwrap());
        }
        assert_eq!(legacy_bytes, b"\x01\x01");
        assert_eq!(kitty_bytes, b"\x1b[97;5u\x1b[97;5:2u\x1b[97;1:3u");
    }
}
