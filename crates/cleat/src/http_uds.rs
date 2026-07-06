use std::io::{Error, ErrorKind, Read, Write};

use http::{
    header::{CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, UPGRADE},
    HeaderName, HeaderValue, Method, Request, Response, StatusCode, Uri, Version,
};
use serde::{Deserialize, Serialize};

use crate::{
    provider::{
        DirtyState, TerminalCellWidth, TerminalCursorStyle, TerminalGeometry, TerminalScrollbarState, TerminalSnapshot,
        TerminalViewportKind,
    },
    vt,
};

const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

pub(crate) type HttpRequest = Request<Vec<u8>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Route {
    Root,
    Health,
    PacketConnect,
    Sessions,
    SessionCreate,
    SessionDelete { id: String },
    SessionAttach { id: String },
    SessionWatch { id: String },
    SessionDetach { id: String },
    SessionExpect { id: String },
    SessionInspect { id: String },
    SessionInput { id: String },
    SessionPasteWithMark { id: String },
    SessionKeys { id: String },
    SessionKeysWithMark { id: String },
    SessionMark { id: String },
    SessionRecord { id: String },
    SessionResolveMarker { id: String },
    SessionResolveNextMarker { id: String },
    SessionResize { id: String },
    SessionScreen { id: String },
    SessionSignal { id: String },
    SessionSnapshot { id: String },
    SessionTags { id: String },
    SessionWait { id: String },
    NotFound,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum InputRequest {
    Text { text: String },
    Paste { text: String },
    Key { key: KeyRequest },
    RawBytes { bytes: Vec<u8> },
    Resize { cols: u16, rows: u16 },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum KeyRequest {
    UnicodeScalar { codepoint: u32 },
    Named { key: NamedKey },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NamedKey {
    Enter,
    Escape,
    Backspace,
    Tab,
    Delete,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct AttachRequest {
    pub cols: u16,
    pub rows: u16,
    pub capabilities: AttachCapabilitiesRequest,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct AttachCapabilitiesRequest {
    pub color_level: AttachColorLevelRequest,
    pub kitty_keyboard: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AttachColorLevelRequest {
    Sixteen,
    Ansi256,
    TrueColor,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct KeysRequest {
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct KeysWithMarkRequest {
    pub bytes: Vec<u8>,
    pub marker_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct PasteWithMarkRequest {
    pub text: String,
    pub marker_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ResizeRequest {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct RecordRequest {
    pub enable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct MarkRequest {
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct MarkResponse {
    pub offset: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct TagRequest {
    pub add: Vec<String>,
    pub remove: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct TagResponse {
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ResolveMarkerRequest {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ResolveNextMarkerRequest {
    pub after: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ResolveNextMarkerResponse {
    pub offset: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct WaitRequest {
    pub conditions: Vec<WaitConditionRequest>,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum WaitConditionRequest {
    OutputIdle { quiet_ms: u64 },
    TextMatch { text: String },
    ScreenStable { stable_ms: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ExpectRequest {
    pub text: String,
    pub since_offset: u64,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct WaitResultResponse {
    pub status: WaitStatusResponse,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WaitStatusResponse {
    Ready,
    Timeout,
    SessionGone,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ScreenResponse {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SignalRequest {
    pub signal: i32,
    pub target: SignalTargetRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SessionListResponse {
    pub sessions: Vec<crate::protocol::InspectResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub(crate) struct DirectorySubscribeRequest {
    #[serde(default)]
    pub selectors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct CreateSessionResponse {
    pub session: crate::runtime::SessionMetadata,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SignalTargetRequest {
    Foreground,
    Leader,
    Tree,
}

#[derive(Debug, Clone, Serialize, PartialEq, Deserialize)]
pub(crate) struct SnapshotResponse {
    pub cols: u16,
    pub rows: u16,
    pub geometry: GeometryResponse,
    pub viewport_kind: String,
    pub scrollback_offset_rows: u64,
    pub scrollbar: ScrollbarResponse,
    pub terminal_modes: TerminalModeResponse,
    pub cells: Vec<CellResponse>,
    pub cursor: CursorResponse,
    pub dirty: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Deserialize)]
pub(crate) struct TerminalModeResponse {
    pub active_alternate_screen: bool,
    pub application_cursor_keys: bool,
    pub alternate_scroll: bool,
    pub mouse_tracking: bool,
    pub mouse_tracking_mode: String,
    pub mouse_report_format: String,
    pub mouse_sgr: bool,
    pub mouse_sgr_pixels: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Deserialize)]
pub(crate) struct ScrollbarResponse {
    pub viewport_kind: String,
    pub total_rows: u64,
    pub viewport_rows: u16,
    pub viewport_top_row: u64,
    pub at_bottom: bool,
}

impl From<TerminalScrollbarState> for ScrollbarResponse {
    fn from(value: TerminalScrollbarState) -> Self {
        Self {
            viewport_kind: viewport_kind_name(value.viewport_kind).to_string(),
            total_rows: value.total_rows,
            viewport_rows: value.viewport_rows,
            viewport_top_row: value.viewport_top_row,
            at_bottom: value.at_bottom,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Deserialize)]
pub(crate) struct GeometryResponse {
    pub cell_width_px: f32,
    pub cell_height_px: f32,
    pub content_x_px: f32,
    pub content_y_px: f32,
    pub content_width_px: f32,
    pub content_height_px: f32,
}

impl From<TerminalGeometry> for GeometryResponse {
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

#[derive(Debug, Clone, Serialize, PartialEq, Deserialize)]
pub(crate) struct CellResponse {
    pub graphemes: Vec<u32>,
    pub fg: RgbResponse,
    pub bg: RgbResponse,
    pub flags: u32,
    pub width: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct RgbResponse {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct CursorResponse {
    pub col: u16,
    pub row: u16,
    pub visible: bool,
    pub style: String,
    #[serde(default)]
    pub blink: bool,
    pub wide_tail: bool,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub(crate) struct ErrorResponse<'a> {
    pub error: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HttpResponse {
    pub status: StatusCode,
    pub body: Vec<u8>,
}

pub(crate) fn looks_like_http_prefix(prefix: &[u8]) -> bool {
    prefix.starts_with(b"GET ")
        || prefix.starts_with(b"POST ")
        || prefix.starts_with(b"PUT ")
        || prefix.starts_with(b"DELET")
        || prefix.starts_with(b"PATCH")
        || prefix.starts_with(b"HEAD ")
        || prefix.starts_with(b"OPTIO")
}

pub(crate) fn read_request_with_prefix(reader: &mut impl Read, prefix: &[u8]) -> std::io::Result<HttpRequest> {
    let mut bytes = prefix.to_vec();
    let header_end = loop {
        if let Some(header_end) = header_end_index(&bytes) {
            break header_end;
        }
        if bytes.len() >= MAX_HEADER_BYTES {
            return Err(Error::new(ErrorKind::InvalidData, "HTTP request headers exceeded maximum size"));
        }
        let mut buf = [0; 1024];
        let n = reader.read(&mut buf)?;
        if n == 0 {
            return Err(Error::new(ErrorKind::UnexpectedEof, "connection closed before HTTP headers completed"));
        }
        bytes.extend_from_slice(&buf[..n]);
    };

    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Request::new(&mut headers);
    let status = parsed.parse(&bytes[..header_end + 4]).map_err(|err| Error::new(ErrorKind::InvalidData, err))?;
    if status.is_partial() {
        return Err(Error::new(ErrorKind::UnexpectedEof, "partial HTTP request headers"));
    }
    let method = parsed.method.ok_or_else(|| Error::new(ErrorKind::InvalidData, "missing HTTP method"))?;
    let path = parsed.path.ok_or_else(|| Error::new(ErrorKind::InvalidData, "missing HTTP path"))?;
    let version = parsed.version.ok_or_else(|| Error::new(ErrorKind::InvalidData, "missing HTTP version"))?;
    let content_length = content_length(parsed.headers)?;
    if content_length > MAX_BODY_BYTES {
        return Err(Error::new(ErrorKind::InvalidData, "HTTP request body exceeded maximum size"));
    }

    let body_start = header_end + 4;
    let mut body = bytes[body_start..].to_vec();
    while body.len() < content_length {
        let remaining = content_length - body.len();
        let mut buf = vec![0; remaining.min(8192)];
        let n = reader.read(&mut buf)?;
        if n == 0 {
            return Err(Error::new(ErrorKind::UnexpectedEof, "connection closed before HTTP body completed"));
        }
        body.extend_from_slice(&buf[..n]);
    }
    body.truncate(content_length);

    let mut builder = Request::builder()
        .method(Method::from_bytes(method.as_bytes()).map_err(|err| Error::new(ErrorKind::InvalidData, err))?)
        .uri(Uri::try_from(path).map_err(|err| Error::new(ErrorKind::InvalidData, err))?)
        .version(match version {
            0 => Version::HTTP_10,
            1 => Version::HTTP_11,
            _ => return Err(Error::new(ErrorKind::InvalidData, "unsupported HTTP version")),
        });
    for header in parsed.headers.iter() {
        builder = builder.header(
            HeaderName::from_bytes(header.name.as_bytes()).map_err(|err| Error::new(ErrorKind::InvalidData, err))?,
            HeaderValue::from_bytes(header.value).map_err(|err| Error::new(ErrorKind::InvalidData, err))?,
        );
    }
    builder.body(body).map_err(Error::other)
}

pub(crate) fn write_request(writer: &mut impl Write, method: Method, path: &str, body: &[u8]) -> std::io::Result<()> {
    write!(
        writer,
        "{method} {path} HTTP/1.1\r\nHost: cleat\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    writer.write_all(body)
}

pub(crate) fn write_attach_upgrade_request(writer: &mut impl Write, path: &str, body: &[u8]) -> std::io::Result<()> {
    write!(
        writer,
        "POST {path} HTTP/1.1\r\nHost: cleat\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: Upgrade\r\nUpgrade: cleat-attach/1\r\n\r\n",
        body.len()
    )?;
    writer.write_all(body)
}

pub(crate) fn read_response(reader: &mut impl Read) -> std::io::Result<HttpResponse> {
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    let header_end =
        header_end_index(&bytes).ok_or_else(|| Error::new(ErrorKind::InvalidData, "HTTP response missing header terminator"))?;
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Response::new(&mut headers);
    let status = parsed.parse(&bytes[..header_end + 4]).map_err(|err| Error::new(ErrorKind::InvalidData, err))?;
    if status.is_partial() {
        return Err(Error::new(ErrorKind::UnexpectedEof, "partial HTTP response headers"));
    }
    let code = parsed.code.ok_or_else(|| Error::new(ErrorKind::InvalidData, "missing HTTP response status"))?;
    let content_length = content_length(parsed.headers)?;
    let body_start = header_end + 4;
    if bytes.len().saturating_sub(body_start) < content_length {
        return Err(Error::new(ErrorKind::UnexpectedEof, "HTTP response body shorter than content-length"));
    }
    let mut body = bytes[body_start..].to_vec();
    body.truncate(content_length);
    Ok(HttpResponse { status: StatusCode::from_u16(code).map_err(|err| Error::new(ErrorKind::InvalidData, err))?, body })
}

pub(crate) fn read_response_head(reader: &mut impl Read) -> std::io::Result<HttpResponse> {
    let mut bytes = Vec::new();
    let header_end = loop {
        if let Some(header_end) = header_end_index(&bytes) {
            break header_end;
        }
        if bytes.len() >= MAX_HEADER_BYTES {
            return Err(Error::new(ErrorKind::InvalidData, "HTTP response headers exceeded maximum size"));
        }
        // Used for the attach upgrade handshake. Do not stage larger reads
        // here: bytes after the header already belong to the upgraded frame
        // stream and must stay unread for the attach relay.
        let mut buf = [0; 1];
        let n = reader.read(&mut buf)?;
        if n == 0 {
            return Err(Error::new(ErrorKind::UnexpectedEof, "connection closed before HTTP response headers completed"));
        }
        bytes.extend_from_slice(&buf[..n]);
    };

    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Response::new(&mut headers);
    let status = parsed.parse(&bytes[..header_end + 4]).map_err(|err| Error::new(ErrorKind::InvalidData, err))?;
    if status.is_partial() {
        return Err(Error::new(ErrorKind::UnexpectedEof, "partial HTTP response headers"));
    }
    let code = parsed.code.ok_or_else(|| Error::new(ErrorKind::InvalidData, "missing HTTP response status"))?;
    Ok(HttpResponse { status: StatusCode::from_u16(code).map_err(|err| Error::new(ErrorKind::InvalidData, err))?, body: Vec::new() })
}

pub(crate) fn route(request: &HttpRequest) -> Route {
    let path = request.uri().path();
    match (request.method(), path) {
        (&Method::GET, "/") => Route::Root,
        (&Method::GET, "/healthz") => Route::Health,
        (&Method::POST, "/connect") => Route::PacketConnect,
        (&Method::GET, "/sessions") => Route::Sessions,
        (&Method::POST, "/sessions") => Route::SessionCreate,
        _ => {
            let Some(rest) = path.strip_prefix("/sessions/") else {
                return Route::NotFound;
            };
            let mut segments = rest.split('/');
            let Some(id) = segments.next().filter(|value| !value.is_empty()) else {
                return Route::NotFound;
            };
            match (request.method(), segments.next(), segments.next()) {
                (&Method::GET, None, None) => Route::SessionInspect { id: id.to_string() },
                (&Method::DELETE, None, None) => Route::SessionDelete { id: id.to_string() },
                (&Method::POST, Some("attach"), None) => Route::SessionAttach { id: id.to_string() },
                (&Method::POST, Some("watch"), None) => Route::SessionWatch { id: id.to_string() },
                (&Method::POST, Some("detach"), None) => Route::SessionDetach { id: id.to_string() },
                (&Method::POST, Some("expect"), None) => Route::SessionExpect { id: id.to_string() },
                (&Method::POST, Some("input"), None) => Route::SessionInput { id: id.to_string() },
                (&Method::POST, Some("paste-with-mark"), None) => Route::SessionPasteWithMark { id: id.to_string() },
                (&Method::POST, Some("keys"), None) => Route::SessionKeys { id: id.to_string() },
                (&Method::POST, Some("keys-with-mark"), None) => Route::SessionKeysWithMark { id: id.to_string() },
                (&Method::POST, Some("mark"), None) => Route::SessionMark { id: id.to_string() },
                (&Method::POST, Some("record"), None) => Route::SessionRecord { id: id.to_string() },
                (&Method::POST, Some("resolve-marker"), None) => Route::SessionResolveMarker { id: id.to_string() },
                (&Method::POST, Some("resolve-next-marker"), None) => Route::SessionResolveNextMarker { id: id.to_string() },
                (&Method::POST, Some("resize"), None) => Route::SessionResize { id: id.to_string() },
                (&Method::GET, Some("screen"), None) => Route::SessionScreen { id: id.to_string() },
                (&Method::POST, Some("signal"), None) => Route::SessionSignal { id: id.to_string() },
                (&Method::GET, Some("snapshot"), None) => Route::SessionSnapshot { id: id.to_string() },
                (&Method::POST, Some("tags"), None) => Route::SessionTags { id: id.to_string() },
                (&Method::POST, Some("wait"), None) => Route::SessionWait { id: id.to_string() },
                _ => Route::NotFound,
            }
        }
    }
}

pub(crate) fn write_json<T: Serialize>(writer: &mut impl Write, status: StatusCode, value: &T) -> std::io::Result<()> {
    let body = serde_json::to_vec(value).map_err(Error::other)?;
    write_response(writer, response(status, "application/json", body)?)
}

pub(crate) fn write_no_content(writer: &mut impl Write) -> std::io::Result<()> {
    let response = Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header(CONTENT_LENGTH, 0)
        .header(CONNECTION, "close")
        .body(Vec::new())
        .map_err(Error::other)?;
    write_response(writer, response)
}

pub(crate) fn write_switching_protocols(writer: &mut impl Write) -> std::io::Result<()> {
    write_switching_protocols_for(writer, "cleat-attach/1")
}

pub(crate) fn write_packet_switching_protocols(writer: &mut impl Write) -> std::io::Result<()> {
    write_switching_protocols_for(writer, "cleat-packet/1")
}

fn write_switching_protocols_for(writer: &mut impl Write, upgrade: &str) -> std::io::Result<()> {
    write!(writer, "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: {upgrade}\r\n\r\n")
}

pub(crate) fn request_has_upgrade_token(request: &HttpRequest, token: &str) -> bool {
    request.headers().get(UPGRADE).and_then(|value| value.to_str().ok()).is_some_and(|value| value.eq_ignore_ascii_case(token))
}

pub(crate) fn write_error(writer: &mut impl Write, status: StatusCode, message: &str) -> std::io::Result<()> {
    write_json(writer, status, &ErrorResponse { error: message })
}

pub(crate) fn snapshot_response(snapshot: TerminalSnapshot) -> SnapshotResponse {
    SnapshotResponse {
        cols: snapshot.cols,
        rows: snapshot.rows,
        geometry: snapshot.geometry.into(),
        viewport_kind: viewport_kind_name(snapshot.viewport_kind).to_string(),
        scrollback_offset_rows: snapshot.scrollback_offset_rows,
        scrollbar: snapshot.scrollbar.into(),
        terminal_modes: terminal_modes_response(snapshot.terminal_modes),
        cells: snapshot
            .cells
            .into_iter()
            .map(|cell| CellResponse {
                graphemes: cell.graphemes,
                fg: RgbResponse { r: cell.fg.r, g: cell.fg.g, b: cell.fg.b },
                bg: RgbResponse { r: cell.bg.r, g: cell.bg.g, b: cell.bg.b },
                flags: cell.flags.bits(),
                width: cell_width_name(cell.width).to_string(),
            })
            .collect(),
        cursor: CursorResponse {
            col: snapshot.cursor.col,
            row: snapshot.cursor.row,
            visible: snapshot.cursor.visible,
            style: cursor_style_name(snapshot.cursor.style).to_string(),
            blink: snapshot.cursor.blink,
            wide_tail: snapshot.cursor.wide_tail,
        },
        dirty: dirty_name(snapshot.dirty).to_string(),
    }
}

fn terminal_modes_response(modes: vt::TerminalModeState) -> TerminalModeResponse {
    TerminalModeResponse {
        active_alternate_screen: modes.active_alternate_screen,
        application_cursor_keys: modes.application_cursor_keys,
        alternate_scroll: modes.alternate_scroll,
        mouse_tracking: modes.mouse_tracking,
        mouse_tracking_mode: mouse_tracking_mode_name(modes.mouse_tracking_mode).to_string(),
        mouse_report_format: mouse_report_format_name(modes.mouse_report_format).to_string(),
        mouse_sgr: modes.mouse_sgr,
        mouse_sgr_pixels: modes.mouse_sgr_pixels,
    }
}

fn response(status: StatusCode, content_type: &'static str, body: Vec<u8>) -> std::io::Result<Response<Vec<u8>>> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, content_type)
        .header(CONTENT_LENGTH, body.len())
        .header(CONNECTION, "close")
        .body(body)
        .map_err(Error::other)
}

fn write_response(writer: &mut impl Write, response: Response<Vec<u8>>) -> std::io::Result<()> {
    let status = response.status();
    let reason = status.canonical_reason().unwrap_or("");
    write!(writer, "HTTP/1.1 {} {}\r\n", status.as_u16(), reason)?;
    for (name, value) in response.headers() {
        writer.write_all(name.as_str().as_bytes())?;
        writer.write_all(b": ")?;
        writer.write_all(value.as_bytes())?;
        writer.write_all(b"\r\n")?;
    }
    writer.write_all(b"\r\n")?;
    writer.write_all(response.body())
}

fn content_length(headers: &[httparse::Header<'_>]) -> std::io::Result<usize> {
    for header in headers {
        if header.name.eq_ignore_ascii_case("content-length") {
            let value = std::str::from_utf8(header.value).map_err(|err| Error::new(ErrorKind::InvalidData, err))?;
            return value.trim().parse().map_err(|err| Error::new(ErrorKind::InvalidData, format!("invalid content-length: {err}")));
        }
    }
    Ok(0)
}

fn header_end_index(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

#[cfg(test)]
pub(crate) fn read_http_request_for_test(stream: &mut impl Read) -> String {
    let mut bytes = Vec::new();
    loop {
        let mut buf = [0; 1024];
        let n = stream.read(&mut buf).expect("read request");
        assert_ne!(n, 0, "connection closed before request completed");
        bytes.extend_from_slice(&buf[..n]);
        if http_request_complete_for_test(&bytes) {
            return String::from_utf8(bytes).expect("request utf8");
        }
    }
}

#[cfg(test)]
fn http_request_complete_for_test(bytes: &[u8]) -> bool {
    let Some(header_end) = header_end_index(bytes) else {
        return false;
    };
    let header = String::from_utf8_lossy(&bytes[..header_end + 4]);
    let content_length = header
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().expect("content length"))
        })
        .unwrap_or(0);
    bytes.len() >= header_end + 4 + content_length
}

fn dirty_name(dirty: DirtyState) -> &'static str {
    match dirty {
        DirtyState::Clean => "clean",
        DirtyState::Partial => "partial",
        DirtyState::Full => "full",
    }
}

fn cell_width_name(width: TerminalCellWidth) -> &'static str {
    match width {
        TerminalCellWidth::Narrow => "narrow",
        TerminalCellWidth::Wide => "wide",
        TerminalCellWidth::SpacerTail => "spacer_tail",
        TerminalCellWidth::SpacerHead => "spacer_head",
    }
}

fn cursor_style_name(style: TerminalCursorStyle) -> &'static str {
    match style {
        TerminalCursorStyle::Bar => "bar",
        TerminalCursorStyle::Block => "block",
        TerminalCursorStyle::Underline => "underline",
        TerminalCursorStyle::BlockHollow => "block_hollow",
    }
}

fn viewport_kind_name(kind: TerminalViewportKind) -> &'static str {
    match kind {
        TerminalViewportKind::LiveNormal => "live_normal",
        TerminalViewportKind::LiveAlternate => "live_alternate",
        TerminalViewportKind::NormalScrollback => "normal_scrollback",
    }
}

fn mouse_tracking_mode_name(mode: vt::MouseTrackingMode) -> &'static str {
    match mode {
        vt::MouseTrackingMode::None => "none",
        vt::MouseTrackingMode::X10 => "x10",
        vt::MouseTrackingMode::Normal => "normal",
        vt::MouseTrackingMode::Button => "button",
        vt::MouseTrackingMode::Any => "any",
    }
}

fn mouse_report_format_name(format: vt::MouseReportFormat) -> &'static str {
    match format {
        vt::MouseReportFormat::Legacy => "legacy",
        vt::MouseReportFormat::Sgr => "sgr",
        vt::MouseReportFormat::SgrPixels => "sgr_pixels",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_http_request_with_body_split_after_prefix() {
        let mut reader = b"T /sessions/alpha/keys HTTP/1.1\r\nHost: cleat\r\nContent-Length: 17\r\n\r\n{\"bytes\":[1,2,3]}".as_slice();

        let request = read_request_with_prefix(&mut reader, b"POS").expect("request");

        assert_eq!(request.method(), Method::POST);
        assert_eq!(request.uri(), "/sessions/alpha/keys");
        assert_eq!(request.body(), br#"{"bytes":[1,2,3]}"#);
    }

    #[test]
    fn detects_http_methods_from_five_byte_prefix() {
        for prefix in [b"GET /".as_slice(), b"POST ", b"PUT /", b"DELET", b"PATCH", b"HEAD ", b"OPTIO"] {
            assert!(looks_like_http_prefix(prefix));
        }
        assert!(!looks_like_http_prefix(b"\0\0\0\0\0"));
    }

    #[test]
    fn routes_provider_critical_session_endpoints() {
        let cases = [
            ("GET", "/healthz", Route::Health),
            ("POST", "/connect", Route::PacketConnect),
            ("GET", "/sessions", Route::Sessions),
            ("POST", "/sessions", Route::SessionCreate),
            ("GET", "/sessions/alpha", Route::SessionInspect { id: "alpha".to_string() }),
            ("DELETE", "/sessions/alpha", Route::SessionDelete { id: "alpha".to_string() }),
            ("POST", "/sessions/alpha/attach", Route::SessionAttach { id: "alpha".to_string() }),
            ("POST", "/sessions/alpha/watch", Route::SessionWatch { id: "alpha".to_string() }),
            ("POST", "/sessions/alpha/detach", Route::SessionDetach { id: "alpha".to_string() }),
            ("POST", "/sessions/alpha/expect", Route::SessionExpect { id: "alpha".to_string() }),
            ("POST", "/sessions/alpha/input", Route::SessionInput { id: "alpha".to_string() }),
            ("POST", "/sessions/alpha/paste-with-mark", Route::SessionPasteWithMark { id: "alpha".to_string() }),
            ("POST", "/sessions/alpha/keys", Route::SessionKeys { id: "alpha".to_string() }),
            ("POST", "/sessions/alpha/keys-with-mark", Route::SessionKeysWithMark { id: "alpha".to_string() }),
            ("POST", "/sessions/alpha/mark", Route::SessionMark { id: "alpha".to_string() }),
            ("POST", "/sessions/alpha/record", Route::SessionRecord { id: "alpha".to_string() }),
            ("POST", "/sessions/alpha/resolve-marker", Route::SessionResolveMarker { id: "alpha".to_string() }),
            ("POST", "/sessions/alpha/resolve-next-marker", Route::SessionResolveNextMarker { id: "alpha".to_string() }),
            ("POST", "/sessions/alpha/resize", Route::SessionResize { id: "alpha".to_string() }),
            ("GET", "/sessions/alpha/screen", Route::SessionScreen { id: "alpha".to_string() }),
            ("POST", "/sessions/alpha/signal", Route::SessionSignal { id: "alpha".to_string() }),
            ("GET", "/sessions/alpha/snapshot", Route::SessionSnapshot { id: "alpha".to_string() }),
            ("POST", "/sessions/alpha/tags", Route::SessionTags { id: "alpha".to_string() }),
            ("POST", "/sessions/alpha/wait", Route::SessionWait { id: "alpha".to_string() }),
        ];

        for (method, path, expected) in cases {
            let request = Request::builder().method(method).uri(path).body(Vec::new()).expect("request");
            assert_eq!(route(&request), expected);
        }
    }

    #[test]
    fn writes_json_with_content_length() {
        let mut response = Vec::new();

        write_json(&mut response, StatusCode::OK, &ErrorResponse { error: "nope" }).expect("write");
        let response = String::from_utf8(response).expect("utf8");

        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("content-type: application/json\r\n"));
        assert!(response.ends_with("{\"error\":\"nope\"}"));
    }

    #[test]
    fn no_content_response_omits_content_type() {
        let mut response = Vec::new();

        write_no_content(&mut response).expect("write");
        let response = String::from_utf8(response).expect("utf8");

        assert!(response.starts_with("HTTP/1.1 204 No Content\r\n"));
        assert!(response.contains("content-length: 0\r\n"));
        assert!(!response.contains("content-type:"));
    }
}
