use std::io::{Error, ErrorKind, Read, Write};

use http::{
    header::{CONNECTION, CONTENT_LENGTH, CONTENT_TYPE},
    HeaderName, HeaderValue, Method, Request, Response, StatusCode, Uri, Version,
};
use serde::{Deserialize, Serialize};

use crate::provider::{DirtyState, TerminalCellWidth, TerminalCursorStyle, TerminalSnapshot};

const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

pub(crate) type HttpRequest = Request<Vec<u8>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Route {
    Root,
    Health,
    SessionExpect { id: String },
    SessionInspect { id: String },
    SessionInput { id: String },
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
pub(crate) struct KeysRequest {
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct KeysWithMarkRequest {
    pub bytes: Vec<u8>,
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
    pub cells: Vec<CellResponse>,
    pub cursor: CursorResponse,
    pub dirty: String,
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
        || prefix.starts_with(b"DELETE")
        || prefix.starts_with(b"PATCH")
        || prefix.starts_with(b"HEAD ")
        || prefix.starts_with(b"OPTIO")
}

pub(crate) fn read_request_with_prefix(reader: &mut impl Read, prefix: &[u8]) -> std::io::Result<HttpRequest> {
    let mut bytes = prefix.to_vec();
    while !has_header_end(&bytes) {
        if bytes.len() >= MAX_HEADER_BYTES {
            return Err(Error::new(ErrorKind::InvalidData, "HTTP request headers exceeded maximum size"));
        }
        let mut buf = [0; 1024];
        let n = reader.read(&mut buf)?;
        if n == 0 {
            return Err(Error::new(ErrorKind::UnexpectedEof, "connection closed before HTTP headers completed"));
        }
        bytes.extend_from_slice(&buf[..n]);
    }

    let header_end = header_end_index(&bytes).expect("header end checked");
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

pub(crate) fn route(request: &HttpRequest) -> Route {
    let path = request.uri().path();
    match (request.method(), path) {
        (&Method::GET, "/") => Route::Root,
        (&Method::GET, "/healthz") => Route::Health,
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
                (&Method::POST, Some("expect"), None) => Route::SessionExpect { id: id.to_string() },
                (&Method::POST, Some("input"), None) => Route::SessionInput { id: id.to_string() },
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
    write_response(writer, response(StatusCode::NO_CONTENT, "application/octet-stream", Vec::new())?)
}

pub(crate) fn write_error(writer: &mut impl Write, status: StatusCode, message: &str) -> std::io::Result<()> {
    write_json(writer, status, &ErrorResponse { error: message })
}

pub(crate) fn snapshot_response(snapshot: TerminalSnapshot) -> SnapshotResponse {
    SnapshotResponse {
        cols: snapshot.cols,
        rows: snapshot.rows,
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
            wide_tail: snapshot.cursor.wide_tail,
        },
        dirty: dirty_name(snapshot.dirty).to_string(),
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

fn has_header_end(bytes: &[u8]) -> bool {
    header_end_index(bytes).is_some()
}

fn header_end_index(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
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
    fn routes_provider_critical_session_endpoints() {
        let cases = [
            ("GET", "/healthz", Route::Health),
            ("GET", "/sessions/alpha", Route::SessionInspect { id: "alpha".to_string() }),
            ("POST", "/sessions/alpha/expect", Route::SessionExpect { id: "alpha".to_string() }),
            ("POST", "/sessions/alpha/input", Route::SessionInput { id: "alpha".to_string() }),
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
}
