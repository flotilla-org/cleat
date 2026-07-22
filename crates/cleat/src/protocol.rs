use std::{
    io::{Error, ErrorKind, Read, Write},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

pub use crate::screen_activity::ScreenActivity;
use crate::vt::VtEngineKind;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub vt_engine: VtEngineKind,
    pub vt_engine_status: String,
    pub functional_vt_available: bool,
    pub cwd: Option<PathBuf>,
    pub cmd: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub status: SessionStatus,
    /// Engines without screen-activity observation support report `stable`.
    #[serde(default)]
    pub screen_activity: ScreenActivity,
    /// Unix timestamp in milliseconds when the current stable period began.
    #[serde(default)]
    pub stable_since: Option<u64>,
    /// Unix timestamp in milliseconds of the most recent render-changing output.
    #[serde(default)]
    pub last_output_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionStatus {
    Attached,
    Detached,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectResult {
    pub session: SessionInspect,
    pub terminal: TerminalInspect,
    pub process: ProcessInspect,
    pub attachments: Vec<AttachmentInspect>,
    pub recording: RecordingInspect,
    /// Engines without screen-activity observation support report `stable`.
    #[serde(default)]
    pub screen_activity: ScreenActivity,
    /// Unix timestamp in milliseconds when the current stable period began.
    #[serde(default)]
    pub stable_since: Option<u64>,
    /// Unix timestamp in milliseconds of the most recent render-changing output.
    #[serde(default)]
    pub last_output_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInspect {
    pub id: String,
    pub state: String,
    pub vt_engine: String,
    #[serde(default)]
    pub vt_engine_status: String,
    #[serde(default)]
    pub functional_vt_available: bool,
    pub cwd: Option<PathBuf>,
    pub cmd: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalInspect {
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessInspect {
    pub leader_pid: u32,
    pub foreground_pgid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leader_cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground_cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentInspect {
    pub role: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingInspect {
    pub active: bool,
    pub bytes_written: u64,
    pub markers: std::collections::HashMap<String, u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalTarget {
    Foreground = 0,
    Leader = 1,
    Tree = 2,
}

pub(crate) const WIRE_HEADER_LEN: usize = 5;

const TAG_INPUT: u8 = 2;
const TAG_OUTPUT: u8 = 3;
const TAG_RESIZE: u8 = 4;
const TAG_ERROR: u8 = 9;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitCondition {
    OutputIdle { quiet_ms: u64 },
    TextMatch { text: String },
    ScreenStable { stable_ms: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitStatus {
    Ready = 0,
    Timeout = 1,
    SessionGone = 2,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Input(Vec<u8>),
    Output(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Error(String),
}

impl Frame {
    pub fn read(reader: &mut impl Read) -> std::io::Result<Self> {
        let mut header = [0u8; WIRE_HEADER_LEN];
        reader.read_exact(&mut header)?;
        Self::read_after_header(header, reader)
    }

    pub(crate) fn read_after_header(header: [u8; WIRE_HEADER_LEN], reader: &mut impl Read) -> std::io::Result<Self> {
        let tag = header[0];
        let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut payload = vec![0u8; len];
        reader.read_exact(&mut payload)?;
        Self::decode(tag, payload)
    }

    pub(crate) fn read_from_buffer(buffer: &mut Vec<u8>) -> std::io::Result<Option<Self>> {
        if buffer.len() < WIRE_HEADER_LEN {
            return Ok(None);
        }
        let tag = buffer[0];
        let len = u32::from_le_bytes([buffer[1], buffer[2], buffer[3], buffer[4]]) as usize;
        let frame_len = WIRE_HEADER_LEN.checked_add(len).ok_or_else(|| Error::new(ErrorKind::InvalidData, "frame length overflow"))?;
        if buffer.len() < frame_len {
            return Ok(None);
        }
        let payload = buffer[WIRE_HEADER_LEN..frame_len].to_vec();
        buffer.drain(..frame_len);
        Self::decode(tag, payload).map(Some)
    }

    pub fn write(&self, writer: &mut impl Write) -> std::io::Result<()> {
        match self {
            Frame::Resize { cols, rows } => {
                let mut payload = [0u8; 4];
                payload[..2].copy_from_slice(&cols.to_le_bytes());
                payload[2..].copy_from_slice(&rows.to_le_bytes());
                write_tagged(writer, TAG_RESIZE, &payload)
            }
            Frame::Input(bytes) => write_tagged(writer, TAG_INPUT, bytes),
            Frame::Output(bytes) => write_tagged(writer, TAG_OUTPUT, bytes),
            Frame::Error(message) => write_tagged(writer, TAG_ERROR, message.as_bytes()),
        }
    }

    /// Wire size of this frame (header plus payload), computed without
    /// encoding anything.
    pub(crate) fn encoded_len(&self) -> usize {
        let payload_len = match self {
            Frame::Resize { .. } => 4,
            Frame::Input(bytes) | Frame::Output(bytes) => bytes.len(),
            Frame::Error(message) => message.len(),
        };
        WIRE_HEADER_LEN + payload_len
    }

    /// Wire size of an `Output` frame carrying `payload_len` bytes.
    pub(crate) fn output_encoded_len(payload_len: usize) -> usize {
        WIRE_HEADER_LEN + payload_len
    }

    /// Frames `payload` as an `Output` frame without constructing a `Frame`,
    /// so the fan-out hot path never copies the chunk into an owned payload
    /// first. Byte-identical to `Frame::Output(payload.to_vec()).write(..)`.
    pub(crate) fn write_output(writer: &mut impl Write, payload: &[u8]) -> std::io::Result<()> {
        write_tagged(writer, TAG_OUTPUT, payload)
    }

    fn decode(tag: u8, payload: Vec<u8>) -> std::io::Result<Self> {
        match tag {
            TAG_RESIZE => decode_size_frame(payload).map(|(cols, rows)| Frame::Resize { cols, rows }),
            TAG_INPUT => Ok(Frame::Input(payload)),
            TAG_OUTPUT => Ok(Frame::Output(payload)),
            TAG_ERROR => String::from_utf8(payload)
                .map(Frame::Error)
                .map_err(|err| Error::new(ErrorKind::InvalidData, format!("invalid error frame utf-8: {err}"))),
            _ => Err(Error::new(ErrorKind::InvalidData, format!("unknown frame tag {tag}"))),
        }
    }
}

fn write_tagged(writer: &mut impl Write, tag: u8, payload: &[u8]) -> std::io::Result<()> {
    let mut header = [0u8; WIRE_HEADER_LEN];
    header[0] = tag;
    header[1..].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    writer.write_all(&header)?;
    writer.write_all(payload)
}

fn decode_size_frame(payload: Vec<u8>) -> std::io::Result<(u16, u16)> {
    if payload.len() != 4 {
        return Err(Error::new(ErrorKind::InvalidData, "invalid size frame"));
    }
    let cols = u16::from_le_bytes([payload[0], payload[1]]);
    let rows = u16::from_le_bytes([payload[2], payload[3]]);
    Ok((cols, rows))
}

#[cfg(test)]
mod tests {
    use super::Frame;

    #[test]
    fn output_round_trip_preserves_binary_payloads() {
        let frame = Frame::Output(vec![0, 1, 2, 3, 4, 5]);
        let mut bytes = Vec::new();
        frame.write(&mut bytes).expect("write frame");
        let decoded = Frame::read(&mut bytes.as_slice()).expect("read frame");
        assert_eq!(decoded, frame);
    }

    #[test]
    fn input_round_trip_preserves_binary_payloads() {
        let frame = Frame::Input(vec![0, 1, 2, 3, 4, 5]);
        let mut bytes = Vec::new();
        frame.write(&mut bytes).expect("write frame");
        let decoded = Frame::read(&mut bytes.as_slice()).expect("read frame");
        assert_eq!(decoded, frame);
    }

    #[test]
    fn resize_round_trip_preserves_dimensions() {
        let frame = Frame::Resize { cols: 120, rows: 40 };
        let mut bytes = Vec::new();
        frame.write(&mut bytes).expect("write frame");
        let decoded = Frame::read(&mut bytes.as_slice()).expect("read frame");
        assert_eq!(decoded, frame);
    }

    #[test]
    fn error_round_trip_preserves_message() {
        let frame = Frame::Error("nope".to_string());
        let mut bytes = Vec::new();
        frame.write(&mut bytes).expect("write frame");
        let decoded = Frame::read(&mut bytes.as_slice()).expect("read frame");
        assert_eq!(decoded, frame);
    }

    /// Pins the wire format byte-for-byte: tag, u32 LE payload length,
    /// payload. Guards the allocation-free `write` restructuring (issue
    /// #135) against any framing drift.
    #[test]
    fn wire_format_is_tag_then_le_length_then_payload() {
        let cases: Vec<(Frame, Vec<u8>)> = vec![
            (Frame::Input(vec![0xDE, 0xAD]), vec![2, 2, 0, 0, 0, 0xDE, 0xAD]),
            (Frame::Output(vec![1, 2, 3]), vec![3, 3, 0, 0, 0, 1, 2, 3]),
            (Frame::Resize { cols: 0x0102, rows: 0x0304 }, vec![4, 4, 0, 0, 0, 0x02, 0x01, 0x04, 0x03]),
            (Frame::Error("no".to_string()), vec![9, 2, 0, 0, 0, b'n', b'o']),
        ];
        for (frame, expected) in cases {
            let mut bytes = Vec::new();
            frame.write(&mut bytes).expect("write frame");
            assert_eq!(bytes, expected, "wire bytes for {frame:?}");
            assert_eq!(frame.encoded_len(), expected.len(), "encoded_len for {frame:?}");
        }
    }

    #[test]
    fn write_output_matches_output_frame_encoding() {
        let payload = [0u8, 1, 2, 250, 255];
        let mut direct = Vec::new();
        Frame::write_output(&mut direct, &payload).expect("write output payload");
        let mut via_frame = Vec::new();
        Frame::Output(payload.to_vec()).write(&mut via_frame).expect("write output frame");
        assert_eq!(direct, via_frame);
        assert_eq!(Frame::output_encoded_len(payload.len()), direct.len());
    }

    #[test]
    fn session_inspect_deserializes_without_vt_status_fields() {
        let json = r#"{
            "id": "test",
            "state": "running",
            "vt_engine": "ghostty",
            "cwd": null,
            "cmd": "bash"
        }"#;
        let result: super::SessionInspect = serde_json::from_str(json).expect("deserialize");
        assert_eq!(result.id, "test");
        assert_eq!(result.vt_engine_status, "");
        assert!(!result.functional_vt_available);
    }
}
