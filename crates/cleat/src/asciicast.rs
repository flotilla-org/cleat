use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Metadata about the cleat tool that created the recording.
#[derive(Debug, Clone, PartialEq)]
pub struct CleatMeta {
    pub version: String,
    pub build: Option<String>,
    pub engine: String,
}

/// Asciicast v3 header (first line of the NDJSON file).
#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    pub cols: u16,
    pub rows: u16,
    pub timestamp: Option<u64>,
    pub term_type: Option<String>,
    pub title: Option<String>,
    pub cleat: Option<CleatMeta>,
}

impl Default for Header {
    fn default() -> Self {
        Self { cols: 80, rows: 24, timestamp: None, term_type: None, title: None, cleat: None }
    }
}

/// An event code in an asciicast v3 recording.
#[derive(Debug, Clone, PartialEq)]
pub enum EventCode {
    Output,
    Input,
    Resize,
    Marker,
    Exit,
    Custom(char),
}

impl EventCode {
    fn as_char(&self) -> char {
        match self {
            EventCode::Output => 'o',
            EventCode::Input => 'i',
            EventCode::Resize => 'r',
            EventCode::Marker => 'm',
            EventCode::Exit => 'x',
            EventCode::Custom(c) => *c,
        }
    }

    fn from_char(c: char) -> Self {
        match c {
            'o' => EventCode::Output,
            'i' => EventCode::Input,
            'r' => EventCode::Resize,
            'm' => EventCode::Marker,
            'x' => EventCode::Exit,
            other => EventCode::Custom(other),
        }
    }
}

/// A single event in an asciicast v3 recording.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// Absolute time from the start of the recording.
    pub time: Duration,
    pub code: EventCode,
    pub data: String,
}

// --- Internal serde types for the header ---

#[derive(Serialize, Deserialize)]
struct TermJson {
    cols: u16,
    rows: u16,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    term_type: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct CleatMetaJson {
    version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    build: Option<String>,
    engine: String,
}

#[derive(Serialize, Deserialize)]
struct HeaderJson {
    version: u8,
    term: TermJson,
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cleat: Option<CleatMetaJson>,
}

/// Serialize a `Header` as a JSON string (the first line of an asciicast v3 file).
pub fn encode_header(header: &Header) -> String {
    let hj = HeaderJson {
        version: 3,
        term: TermJson { cols: header.cols, rows: header.rows, term_type: header.term_type.clone() },
        timestamp: header.timestamp,
        title: header.title.clone(),
        cleat: header.cleat.as_ref().map(|m| CleatMetaJson {
            version: m.version.clone(),
            build: m.build.clone(),
            engine: m.engine.clone(),
        }),
    };
    serde_json::to_string(&hj).expect("header serialization is infallible")
}

/// Parse a JSON header line into a `Header`.
pub fn decode_header(line: &str) -> Result<Header, String> {
    let hj: HeaderJson = serde_json::from_str(line).map_err(|e| format!("invalid header JSON: {e}"))?;
    if hj.version != 3 {
        return Err(format!("unsupported asciicast version: {}", hj.version));
    }
    Ok(Header {
        cols: hj.term.cols,
        rows: hj.term.rows,
        timestamp: hj.timestamp,
        term_type: hj.term.term_type,
        title: hj.title,
        cleat: hj.cleat.map(|m| CleatMeta { version: m.version, build: m.build, engine: m.engine }),
    })
}

/// Serialize an `Event` as an asciicast v3 NDJSON event line.
///
/// The delta written to the line is `event.time - *prev_time`.
/// `*prev_time` is updated to `event.time` after encoding.
pub fn encode_event(event: &Event, prev_time: &mut Duration) -> String {
    let delta = event.time.saturating_sub(*prev_time);
    *prev_time = event.time;

    let secs = delta.as_secs();
    let millis = delta.subsec_millis();
    let delta_str = format!("{secs}.{millis:03}");

    let code_str = event.code.as_char().to_string();
    // Use serde_json to serialize the data string so control chars are escaped correctly.
    let data_json = serde_json::to_string(&event.data).expect("string serialization is infallible");

    format!("[{delta_str}, \"{code_str}\", {data_json}]")
}

/// Parse an asciicast v3 NDJSON event line into an `Event`.
///
/// The delta read from the line is added to `*prev_time` to produce the
/// absolute `event.time`. `*prev_time` is updated to the new absolute time.
pub fn decode_event(line: &str, prev_time: &mut Duration) -> Result<Event, String> {
    let tuple: (f64, String, String) = serde_json::from_str(line).map_err(|e| format!("invalid event JSON: {e}"))?;

    let (delta_secs, code_str, data) = tuple;

    if delta_secs < 0.0 {
        return Err(format!("negative event delta: {delta_secs}"));
    }

    let delta_millis = (delta_secs * 1000.0).round() as u64;
    let delta = Duration::from_millis(delta_millis);
    let abs_time = *prev_time + delta;
    *prev_time = abs_time;

    let code_char = code_str.chars().next().ok_or_else(|| "empty event code".to_string())?;

    Ok(Event { time: abs_time, code: EventCode::from_char(code_char), data })
}
