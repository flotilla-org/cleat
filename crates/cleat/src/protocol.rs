use std::{
    io::{Error, ErrorKind, Read, Write},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

use crate::vt::{ClientCapabilities, ColorLevel, VtEngineKind};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub vt_engine: VtEngineKind,
    pub vt_engine_status: String,
    pub functional_vt_available: bool,
    pub cwd: Option<PathBuf>,
    pub cmd: Option<String>,
    pub status: SessionStatus,
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

const TAG_ATTACH_INIT: u8 = 1;
const TAG_INPUT: u8 = 2;
const TAG_OUTPUT: u8 = 3;
const TAG_RESIZE: u8 = 4;
const TAG_ACK: u8 = 5;
const TAG_BUSY: u8 = 6;
const TAG_DETACH: u8 = 7;
const TAG_CAPTURE: u8 = 8;
const TAG_ERROR: u8 = 9;
const TAG_SEND_KEYS: u8 = 10;
const TAG_INSPECT: u8 = 11;
const TAG_INSPECT_RESULT: u8 = 12;
const TAG_SIGNAL: u8 = 13;
const TAG_RECORD_CONTROL: u8 = 14;
const TAG_MARK: u8 = 15;
const TAG_MARK_RESULT: u8 = 16;
const TAG_RESOLVE_MARKER: u8 = 17;
const TAG_WAIT: u8 = 18;
const TAG_WAIT_RESULT: u8 = 19;
const TAG_EXPECT: u8 = 20;
const TAG_EXPECT_RESULT: u8 = 21;
const TAG_SEND_KEYS_WITH_MARK: u8 = 22;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitCondition {
    OutputIdle { quiet_ms: u64 },
    TextMatch { text: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitStatus {
    Ready = 0,
    Timeout = 1,
    SessionGone = 2,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    AttachInit { cols: u16, rows: u16, capabilities: ClientCapabilities },
    Input(Vec<u8>),
    Output(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Ack,
    Busy,
    Detach,
    Capture,
    SendKeys(Vec<u8>),
    Error(String),
    Inspect,
    InspectResult(Vec<u8>),
    Signal { signal: i32, target: SignalTarget },
    RecordControl { enable: bool },
    Mark { name: Option<String> },
    MarkResult { offset: u64 },
    ResolveMarker { name: String },
    Wait { conditions: Vec<WaitCondition>, timeout_ms: u64 },
    WaitResult { status: WaitStatus, elapsed_ms: u64 },
    Expect { text: String, since_offset: u64, timeout_ms: u64 },
    ExpectResult { status: WaitStatus, elapsed_ms: u64 },
    SendKeysWithMark { bytes: Vec<u8>, marker_name: String },
}

impl Frame {
    pub fn read(reader: &mut impl Read) -> std::io::Result<Self> {
        let mut header = [0u8; 5];
        reader.read_exact(&mut header)?;
        let tag = header[0];
        let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut payload = vec![0u8; len];
        reader.read_exact(&mut payload)?;
        Self::decode(tag, payload)
    }

    pub fn write(&self, writer: &mut impl Write) -> std::io::Result<()> {
        let (tag, payload) = self.encode();
        let mut header = [0u8; 5];
        header[0] = tag;
        header[1..].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        writer.write_all(&header)?;
        writer.write_all(&payload)
    }

    fn encode(&self) -> (u8, Vec<u8>) {
        match self {
            Frame::AttachInit { cols, rows, capabilities } => {
                let mut payload = Vec::with_capacity(5);
                payload.extend_from_slice(&cols.to_le_bytes());
                payload.extend_from_slice(&rows.to_le_bytes());
                payload.push(encode_capabilities(*capabilities));
                (TAG_ATTACH_INIT, payload)
            }
            Frame::Resize { cols, rows } => {
                let mut payload = Vec::with_capacity(4);
                payload.extend_from_slice(&cols.to_le_bytes());
                payload.extend_from_slice(&rows.to_le_bytes());
                (TAG_RESIZE, payload)
            }
            Frame::Input(bytes) => (TAG_INPUT, bytes.clone()),
            Frame::Output(bytes) => (TAG_OUTPUT, bytes.clone()),
            Frame::Ack => (TAG_ACK, vec![]),
            Frame::Busy => (TAG_BUSY, vec![]),
            Frame::Detach => (TAG_DETACH, vec![]),
            Frame::Capture => (TAG_CAPTURE, vec![]),
            Frame::SendKeys(bytes) => (TAG_SEND_KEYS, bytes.clone()),
            Frame::Error(message) => (TAG_ERROR, message.clone().into_bytes()),
            Frame::Inspect => (TAG_INSPECT, vec![]),
            Frame::InspectResult(bytes) => (TAG_INSPECT_RESULT, bytes.clone()),
            Frame::Signal { signal, target } => {
                let mut payload = Vec::with_capacity(5);
                payload.extend_from_slice(&signal.to_le_bytes());
                payload.push(*target as u8);
                (TAG_SIGNAL, payload)
            }
            Frame::RecordControl { enable } => (TAG_RECORD_CONTROL, vec![if *enable { 1 } else { 0 }]),
            Frame::Mark { ref name } => {
                let payload = match name {
                    Some(n) => n.as_bytes().to_vec(),
                    None => vec![],
                };
                (TAG_MARK, payload)
            }
            Frame::MarkResult { offset } => (TAG_MARK_RESULT, offset.to_le_bytes().to_vec()),
            Frame::ResolveMarker { ref name } => (TAG_RESOLVE_MARKER, name.as_bytes().to_vec()),
            Frame::Wait { ref conditions, timeout_ms } => {
                debug_assert!(conditions.len() <= 255, "wait frame supports at most 255 conditions");
                let mut payload = Vec::new();
                payload.extend_from_slice(&timeout_ms.to_le_bytes());
                payload.push(conditions.len() as u8);
                for condition in conditions {
                    match condition {
                        WaitCondition::OutputIdle { quiet_ms } => {
                            payload.push(0);
                            payload.extend_from_slice(&quiet_ms.to_le_bytes());
                        }
                        WaitCondition::TextMatch { text } => {
                            debug_assert!(text.len() <= u32::MAX as usize, "text match pattern too long");
                            payload.push(1);
                            payload.extend_from_slice(&(text.len() as u32).to_le_bytes());
                            payload.extend_from_slice(text.as_bytes());
                        }
                    }
                }
                (TAG_WAIT, payload)
            }
            Frame::WaitResult { status, elapsed_ms } => {
                let mut payload = Vec::with_capacity(9);
                payload.push(*status as u8);
                payload.extend_from_slice(&elapsed_ms.to_le_bytes());
                (TAG_WAIT_RESULT, payload)
            }
            Frame::Expect { ref text, since_offset, timeout_ms } => {
                let mut payload = Vec::new();
                payload.extend_from_slice(&timeout_ms.to_le_bytes());
                payload.extend_from_slice(&since_offset.to_le_bytes());
                payload.extend_from_slice(&(text.len() as u32).to_le_bytes());
                payload.extend_from_slice(text.as_bytes());
                (TAG_EXPECT, payload)
            }
            Frame::ExpectResult { status, elapsed_ms } => {
                let mut payload = Vec::with_capacity(9);
                payload.push(*status as u8);
                payload.extend_from_slice(&elapsed_ms.to_le_bytes());
                (TAG_EXPECT_RESULT, payload)
            }
            Frame::SendKeysWithMark { ref bytes, ref marker_name } => {
                let mut payload = Vec::new();
                payload.extend_from_slice(&(marker_name.len() as u32).to_le_bytes());
                payload.extend_from_slice(marker_name.as_bytes());
                payload.extend_from_slice(bytes);
                (TAG_SEND_KEYS_WITH_MARK, payload)
            }
        }
    }

    fn decode(tag: u8, payload: Vec<u8>) -> std::io::Result<Self> {
        match tag {
            TAG_ATTACH_INIT => decode_attach_init(payload),
            TAG_RESIZE => decode_size_frame(payload).map(|(cols, rows)| Frame::Resize { cols, rows }),
            TAG_INPUT => Ok(Frame::Input(payload)),
            TAG_OUTPUT => Ok(Frame::Output(payload)),
            TAG_ACK => Ok(Frame::Ack),
            TAG_BUSY => Ok(Frame::Busy),
            TAG_DETACH => Ok(Frame::Detach),
            TAG_CAPTURE => Ok(Frame::Capture),
            TAG_SEND_KEYS => Ok(Frame::SendKeys(payload)),
            TAG_ERROR => String::from_utf8(payload)
                .map(Frame::Error)
                .map_err(|err| Error::new(ErrorKind::InvalidData, format!("invalid error frame utf-8: {err}"))),
            TAG_INSPECT => Ok(Frame::Inspect),
            TAG_INSPECT_RESULT => Ok(Frame::InspectResult(payload)),
            TAG_SIGNAL => {
                if payload.len() != 5 {
                    return Err(Error::new(ErrorKind::InvalidData, "invalid signal frame"));
                }
                let signal = i32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
                let target = match payload[4] {
                    0 => SignalTarget::Foreground,
                    1 => SignalTarget::Leader,
                    2 => SignalTarget::Tree,
                    _ => return Err(Error::new(ErrorKind::InvalidData, "invalid signal target")),
                };
                Ok(Frame::Signal { signal, target })
            }
            TAG_RECORD_CONTROL => {
                if payload.len() != 1 {
                    return Err(Error::new(ErrorKind::InvalidData, "invalid record control frame"));
                }
                Ok(Frame::RecordControl { enable: payload[0] != 0 })
            }
            TAG_MARK => {
                let name = if payload.is_empty() {
                    None
                } else {
                    Some(String::from_utf8(payload).map_err(|e| Error::new(ErrorKind::InvalidData, format!("invalid mark name: {e}")))?)
                };
                Ok(Frame::Mark { name })
            }
            TAG_RESOLVE_MARKER => {
                let name =
                    String::from_utf8(payload).map_err(|e| Error::new(ErrorKind::InvalidData, format!("invalid marker name: {e}")))?;
                Ok(Frame::ResolveMarker { name })
            }
            TAG_MARK_RESULT => {
                if payload.len() != 8 {
                    return Err(Error::new(ErrorKind::InvalidData, "invalid mark result frame"));
                }
                let offset =
                    u64::from_le_bytes([payload[0], payload[1], payload[2], payload[3], payload[4], payload[5], payload[6], payload[7]]);
                Ok(Frame::MarkResult { offset })
            }
            TAG_WAIT => {
                if payload.len() < 9 {
                    return Err(Error::new(ErrorKind::InvalidData, "invalid wait frame: too short"));
                }
                let timeout_ms =
                    u64::from_le_bytes([payload[0], payload[1], payload[2], payload[3], payload[4], payload[5], payload[6], payload[7]]);
                let num_conditions = payload[8] as usize;
                let mut offset = 9;
                let mut conditions = Vec::with_capacity(num_conditions);
                for _ in 0..num_conditions {
                    if offset >= payload.len() {
                        return Err(Error::new(ErrorKind::InvalidData, "invalid wait frame: truncated condition"));
                    }
                    let condition_tag = payload[offset];
                    offset += 1;
                    match condition_tag {
                        0 => {
                            if offset + 8 > payload.len() {
                                return Err(Error::new(ErrorKind::InvalidData, "invalid wait frame: truncated output_idle"));
                            }
                            let quiet_ms = u64::from_le_bytes([
                                payload[offset],
                                payload[offset + 1],
                                payload[offset + 2],
                                payload[offset + 3],
                                payload[offset + 4],
                                payload[offset + 5],
                                payload[offset + 6],
                                payload[offset + 7],
                            ]);
                            offset += 8;
                            conditions.push(WaitCondition::OutputIdle { quiet_ms });
                        }
                        1 => {
                            if offset + 4 > payload.len() {
                                return Err(Error::new(ErrorKind::InvalidData, "invalid wait frame: truncated text_match length"));
                            }
                            let text_len =
                                u32::from_le_bytes([payload[offset], payload[offset + 1], payload[offset + 2], payload[offset + 3]])
                                    as usize;
                            offset += 4;
                            if offset + text_len > payload.len() {
                                return Err(Error::new(ErrorKind::InvalidData, "invalid wait frame: truncated text_match text"));
                            }
                            let text = String::from_utf8(payload[offset..offset + text_len].to_vec())
                                .map_err(|e| Error::new(ErrorKind::InvalidData, format!("invalid wait text_match utf-8: {e}")))?;
                            offset += text_len;
                            conditions.push(WaitCondition::TextMatch { text });
                        }
                        _ => {
                            return Err(Error::new(ErrorKind::InvalidData, format!("invalid wait condition tag: {condition_tag}")));
                        }
                    }
                }
                Ok(Frame::Wait { conditions, timeout_ms })
            }
            TAG_WAIT_RESULT => {
                if payload.len() != 9 {
                    return Err(Error::new(ErrorKind::InvalidData, "invalid wait result frame"));
                }
                let status = match payload[0] {
                    0 => WaitStatus::Ready,
                    1 => WaitStatus::Timeout,
                    2 => WaitStatus::SessionGone,
                    _ => return Err(Error::new(ErrorKind::InvalidData, format!("invalid wait status: {}", payload[0]))),
                };
                let elapsed_ms =
                    u64::from_le_bytes([payload[1], payload[2], payload[3], payload[4], payload[5], payload[6], payload[7], payload[8]]);
                Ok(Frame::WaitResult { status, elapsed_ms })
            }
            TAG_EXPECT => {
                if payload.len() < 20 {
                    return Err(Error::new(ErrorKind::InvalidData, "invalid expect frame: too short"));
                }
                let timeout_ms =
                    u64::from_le_bytes([payload[0], payload[1], payload[2], payload[3], payload[4], payload[5], payload[6], payload[7]]);
                let since_offset = u64::from_le_bytes([
                    payload[8],
                    payload[9],
                    payload[10],
                    payload[11],
                    payload[12],
                    payload[13],
                    payload[14],
                    payload[15],
                ]);
                let text_len = u32::from_le_bytes([payload[16], payload[17], payload[18], payload[19]]) as usize;
                if payload.len() < 20 + text_len {
                    return Err(Error::new(ErrorKind::InvalidData, "invalid expect frame: truncated text"));
                }
                let text = String::from_utf8(payload[20..20 + text_len].to_vec())
                    .map_err(|e| Error::new(ErrorKind::InvalidData, format!("invalid expect text utf-8: {e}")))?;
                Ok(Frame::Expect { text, since_offset, timeout_ms })
            }
            TAG_EXPECT_RESULT => {
                if payload.len() != 9 {
                    return Err(Error::new(ErrorKind::InvalidData, "invalid expect result frame"));
                }
                let status = match payload[0] {
                    0 => WaitStatus::Ready,
                    1 => WaitStatus::Timeout,
                    2 => WaitStatus::SessionGone,
                    _ => return Err(Error::new(ErrorKind::InvalidData, format!("invalid expect status: {}", payload[0]))),
                };
                let elapsed_ms =
                    u64::from_le_bytes([payload[1], payload[2], payload[3], payload[4], payload[5], payload[6], payload[7], payload[8]]);
                Ok(Frame::ExpectResult { status, elapsed_ms })
            }
            TAG_SEND_KEYS_WITH_MARK => {
                if payload.len() < 4 {
                    return Err(Error::new(ErrorKind::InvalidData, "invalid send-keys-with-mark frame: too short"));
                }
                let name_len = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
                if payload.len() < 4 + name_len {
                    return Err(Error::new(ErrorKind::InvalidData, "invalid send-keys-with-mark frame: truncated name"));
                }
                let marker_name = String::from_utf8(payload[4..4 + name_len].to_vec())
                    .map_err(|e| Error::new(ErrorKind::InvalidData, format!("invalid marker name utf-8: {e}")))?;
                let bytes = payload[4 + name_len..].to_vec();
                Ok(Frame::SendKeysWithMark { bytes, marker_name })
            }
            _ => Err(Error::new(ErrorKind::InvalidData, format!("unknown frame tag {tag}"))),
        }
    }
}

fn decode_size_frame(payload: Vec<u8>) -> std::io::Result<(u16, u16)> {
    if payload.len() != 4 {
        return Err(Error::new(ErrorKind::InvalidData, "invalid size frame"));
    }
    let cols = u16::from_le_bytes([payload[0], payload[1]]);
    let rows = u16::from_le_bytes([payload[2], payload[3]]);
    Ok((cols, rows))
}

fn decode_attach_init(payload: Vec<u8>) -> std::io::Result<Frame> {
    if payload.len() != 5 {
        return Err(Error::new(ErrorKind::InvalidData, "invalid attach init frame"));
    }

    let cols = u16::from_le_bytes([payload[0], payload[1]]);
    let rows = u16::from_le_bytes([payload[2], payload[3]]);
    let capabilities = decode_capabilities(payload[4])?;

    Ok(Frame::AttachInit { cols, rows, capabilities })
}

fn encode_capabilities(capabilities: ClientCapabilities) -> u8 {
    let color_bits = match capabilities.color_level {
        ColorLevel::Sixteen => 0,
        ColorLevel::Ansi256 => 1,
        ColorLevel::TrueColor => 2,
    };
    let kitty_keyboard_bit = if capabilities.kitty_keyboard { 1 << 2 } else { 0 };
    color_bits | kitty_keyboard_bit
}

fn decode_capabilities(byte: u8) -> std::io::Result<ClientCapabilities> {
    let color_level = match byte & 0b11 {
        0 => ColorLevel::Sixteen,
        1 => ColorLevel::Ansi256,
        2 => ColorLevel::TrueColor,
        _ => {
            return Err(Error::new(ErrorKind::InvalidData, format!("invalid attach capability color level {byte:#010b}")));
        }
    };
    let kitty_keyboard = (byte & (1 << 2)) != 0;
    Ok(ClientCapabilities::new(color_level, kitty_keyboard))
}

#[cfg(test)]
mod tests {
    use super::{Frame, SignalTarget, WaitCondition, WaitStatus};
    use crate::vt::{ClientCapabilities, ColorLevel};

    #[test]
    fn attach_init_round_trip_preserves_capability_profile() {
        let frame = Frame::AttachInit { cols: 120, rows: 40, capabilities: ClientCapabilities::new(ColorLevel::Ansi256, true) };
        let mut bytes = Vec::new();
        frame.write(&mut bytes).expect("write frame");
        let decoded = Frame::read(&mut bytes.as_slice()).expect("read frame");
        assert_eq!(decoded, frame);
    }

    #[test]
    fn frame_round_trip_preserves_binary_payloads() {
        let frame = Frame::Output(vec![0, 1, 2, 3, 4, 5]);
        let mut bytes = Vec::new();
        frame.write(&mut bytes).expect("write frame");
        let decoded = Frame::read(&mut bytes.as_slice()).expect("read frame");
        assert_eq!(decoded, frame);
    }

    #[test]
    fn send_keys_round_trip_preserves_binary_payloads() {
        let frame = Frame::SendKeys(vec![0, 1, 2, 3, 4, 5]);
        let mut bytes = Vec::new();
        frame.write(&mut bytes).expect("write frame");
        let decoded = Frame::read(&mut bytes.as_slice()).expect("read frame");
        assert_eq!(decoded, frame);
    }

    #[test]
    fn inspect_result_round_trip_preserves_json_payload() {
        let json = br#"{"session":{"id":"test"}}"#.to_vec();
        let frame = Frame::InspectResult(json.clone());
        let mut bytes = Vec::new();
        frame.write(&mut bytes).expect("write frame");
        let decoded = Frame::read(&mut bytes.as_slice()).expect("read frame");
        assert_eq!(decoded, Frame::InspectResult(json));
    }

    #[test]
    fn inspect_round_trip_is_empty() {
        let frame = Frame::Inspect;
        let mut bytes = Vec::new();
        frame.write(&mut bytes).expect("write frame");
        let decoded = Frame::read(&mut bytes.as_slice()).expect("read frame");
        assert_eq!(decoded, Frame::Inspect);
    }

    #[test]
    fn signal_round_trip_preserves_target_and_signal() {
        let frame = Frame::Signal { signal: libc::SIGINT, target: SignalTarget::Foreground };
        let mut bytes = Vec::new();
        frame.write(&mut bytes).expect("write frame");
        let decoded = Frame::read(&mut bytes.as_slice()).expect("read frame");
        assert_eq!(decoded, frame);
    }

    #[test]
    fn record_control_round_trip() {
        let frame = Frame::RecordControl { enable: true };
        let mut bytes = Vec::new();
        frame.write(&mut bytes).expect("write frame");
        let decoded = Frame::read(&mut bytes.as_slice()).expect("read frame");
        assert_eq!(decoded, frame);
    }

    #[test]
    fn mark_round_trip() {
        let frame = Frame::Mark { name: None };
        let mut bytes = Vec::new();
        frame.write(&mut bytes).expect("write");
        let decoded = Frame::read(&mut bytes.as_slice()).expect("read");
        assert_eq!(decoded, Frame::Mark { name: None });
    }

    #[test]
    fn mark_result_round_trip() {
        let frame = Frame::MarkResult { offset: 12345 };
        let mut bytes = Vec::new();
        frame.write(&mut bytes).expect("write");
        let decoded = Frame::read(&mut bytes.as_slice()).expect("read");
        assert_eq!(decoded, Frame::MarkResult { offset: 12345 });
    }

    #[test]
    fn named_mark_round_trip() {
        let frame = Frame::Mark { name: Some("test-start".to_string()) };
        let mut bytes = Vec::new();
        frame.write(&mut bytes).expect("write");
        let decoded = Frame::read(&mut bytes.as_slice()).expect("read");
        assert_eq!(decoded, Frame::Mark { name: Some("test-start".to_string()) });
    }

    #[test]
    fn resolve_marker_round_trip() {
        let frame = Frame::ResolveMarker { name: "checkpoint".to_string() };
        let mut bytes = Vec::new();
        frame.write(&mut bytes).expect("write");
        let decoded = Frame::read(&mut bytes.as_slice()).expect("read");
        assert_eq!(decoded, Frame::ResolveMarker { name: "checkpoint".to_string() });
    }

    #[test]
    fn wait_output_idle_round_trip() {
        let frame = Frame::Wait { conditions: vec![WaitCondition::OutputIdle { quiet_ms: 500 }], timeout_ms: 5000 };
        let mut bytes = Vec::new();
        frame.write(&mut bytes).expect("write");
        let decoded = Frame::read(&mut bytes.as_slice()).expect("read");
        assert_eq!(decoded, frame);
    }

    #[test]
    fn wait_text_match_round_trip() {
        let frame = Frame::Wait { conditions: vec![WaitCondition::TextMatch { text: "hello world".to_string() }], timeout_ms: 3000 };
        let mut bytes = Vec::new();
        frame.write(&mut bytes).expect("write");
        let decoded = Frame::read(&mut bytes.as_slice()).expect("read");
        assert_eq!(decoded, frame);
    }

    #[test]
    fn wait_combined_conditions_round_trip() {
        let frame = Frame::Wait {
            conditions: vec![WaitCondition::OutputIdle { quiet_ms: 200 }, WaitCondition::TextMatch { text: "$ ".to_string() }],
            timeout_ms: 10000,
        };
        let mut bytes = Vec::new();
        frame.write(&mut bytes).expect("write");
        let decoded = Frame::read(&mut bytes.as_slice()).expect("read");
        assert_eq!(decoded, frame);
    }

    #[test]
    fn wait_result_ready_round_trip() {
        let frame = Frame::WaitResult { status: WaitStatus::Ready, elapsed_ms: 42 };
        let mut bytes = Vec::new();
        frame.write(&mut bytes).expect("write");
        let decoded = Frame::read(&mut bytes.as_slice()).expect("read");
        assert_eq!(decoded, frame);
    }

    #[test]
    fn wait_result_timeout_round_trip() {
        let frame = Frame::WaitResult { status: WaitStatus::Timeout, elapsed_ms: 5000 };
        let mut bytes = Vec::new();
        frame.write(&mut bytes).expect("write");
        let decoded = Frame::read(&mut bytes.as_slice()).expect("read");
        assert_eq!(decoded, frame);
    }

    #[test]
    fn wait_result_session_gone_round_trip() {
        let frame = Frame::WaitResult { status: WaitStatus::SessionGone, elapsed_ms: 1234 };
        let mut bytes = Vec::new();
        frame.write(&mut bytes).expect("write");
        let decoded = Frame::read(&mut bytes.as_slice()).expect("read");
        assert_eq!(decoded, frame);
    }

    #[test]
    fn expect_round_trip() {
        let frame = Frame::Expect { text: "PASS".to_string(), since_offset: 12345, timeout_ms: 5000 };
        let mut bytes = Vec::new();
        frame.write(&mut bytes).expect("write");
        let decoded = Frame::read(&mut bytes.as_slice()).expect("read");
        assert_eq!(decoded, frame);
    }

    #[test]
    fn expect_result_round_trip() {
        let frame = Frame::ExpectResult { status: WaitStatus::Ready, elapsed_ms: 42 };
        let mut bytes = Vec::new();
        frame.write(&mut bytes).expect("write");
        let decoded = Frame::read(&mut bytes.as_slice()).expect("read");
        assert_eq!(decoded, frame);
    }

    #[test]
    fn send_keys_with_mark_round_trip() {
        let frame = Frame::SendKeysWithMark { bytes: b"hello\r".to_vec(), marker_name: "m1".to_string() };
        let mut bytes = Vec::new();
        frame.write(&mut bytes).expect("write");
        let decoded = Frame::read(&mut bytes.as_slice()).expect("read");
        assert_eq!(decoded, frame);
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
