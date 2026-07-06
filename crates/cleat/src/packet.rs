use std::io::{Error, ErrorKind, Read, Write};

use serde::{Deserialize, Serialize};

use crate::provider::{TerminalInputEvent, TerminalRenderUpdate};

pub const PROTOCOL_VERSION: u16 = 2;
pub const CHANNEL_CONTROL: u32 = 0;

pub const MSG_CONTROL_HELLO: u8 = 1;
pub const MSG_CONTROL_DIRECTORY_SNAPSHOT: u8 = 2;
pub const MSG_CONTROL_DIRECTORY_DELTA: u8 = 3;
pub const MSG_CONTROL_OPEN_CHANNEL: u8 = 4;
pub const MSG_CONTROL_CLOSE_CHANNEL: u8 = 5;
pub const MSG_CONTROL_ERROR: u8 = 6;

pub const MSG_SESSION_RENDER: u8 = 16;
pub const MSG_SESSION_ACK: u8 = 17;
pub const MSG_SESSION_INPUT: u8 = 18;
pub const MSG_SESSION_RESIZE: u8 = 19;
pub const MSG_SESSION_VIEWPORT: u8 = 20;
pub const MSG_SESSION_ROLE: u8 = 21;

const HEADER_LEN: usize = 9;
pub const MAX_PACKET_PAYLOAD_LEN: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlHello {
    pub version: u16,
    pub min_supported_version: u16,
}

impl ControlHello {
    pub fn current() -> Self {
        Self { version: PROTOCOL_VERSION, min_supported_version: PROTOCOL_VERSION }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectorySnapshot {
    pub sessions: Vec<DirectoryEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryDelta {
    pub upserted: Vec<DirectoryEntry>,
    pub removed_session_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryEntry {
    pub session_id: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub controller_count: u32,
    #[serde(default)]
    pub watcher_count: u32,
    #[serde(default)]
    pub recreatable: bool,
    pub cols: u16,
    pub rows: u16,
}

/// Attachment role of a session channel. One controller per session across
/// packet and legacy stream clients: the controller's input, resize, and
/// viewport commands are routed; watchers are read-only and never resize.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelRole {
    Watcher,
    Controller,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenChannel {
    pub channel: u32,
    pub session_id: String,
    /// Requested role; the granted role arrives as a `RoleState` before the
    /// initial render packet (a controller request may be granted Watcher).
    pub role: ChannelRole,
    /// Preempt an existing packet controller. Never preempts a legacy stream
    /// controller (there is no way to demote a raw attach to read-only).
    pub take: bool,
}

/// Client→server on an open session channel: request a role change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleRequest {
    pub role: ChannelRole,
    pub take: bool,
}

/// Server→client on a session channel: the granted role, sent on open and on
/// any later change (e.g. demotion because another client took control).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleState {
    pub role: ChannelRole,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseChannel {
    pub channel: u32,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlError {
    pub channel: u32,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RenderPacket {
    pub update: TerminalRenderUpdate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ack {
    pub generation: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Input {
    pub event: TerminalInputEvent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resize {
    pub cols: u16,
    pub rows: u16,
}

/// Client→server viewport movement (scrollbar drag, keyboard paging). The
/// resulting viewport/scrollbar state flows back through the next render
/// packet rather than a reply message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Viewport {
    pub command: crate::provider::ViewportCommand,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PacketFrame {
    pub channel: u32,
    pub msg_type: u8,
    pub payload: Vec<u8>,
}

impl PacketFrame {
    pub fn new<T: Serialize>(channel: u32, msg_type: u8, value: &T) -> std::io::Result<Self> {
        let payload = postcard::to_allocvec(value).map_err(|err| Error::new(ErrorKind::InvalidData, err))?;
        if payload.len() > MAX_PACKET_PAYLOAD_LEN {
            return Err(Error::new(ErrorKind::InvalidInput, "packet payload exceeds maximum length"));
        }
        Ok(Self { channel, msg_type, payload })
    }

    pub fn decode<T: for<'de> Deserialize<'de>>(&self) -> std::io::Result<T> {
        postcard::from_bytes(&self.payload).map_err(|err| Error::new(ErrorKind::InvalidData, err))
    }

    pub fn write(&self, writer: &mut impl Write) -> std::io::Result<()> {
        let len =
            u32::try_from(self.payload.len()).map_err(|_| Error::new(ErrorKind::InvalidInput, "packet payload exceeds u32 length"))?;
        writer.write_all(&self.channel.to_le_bytes())?;
        writer.write_all(&[self.msg_type])?;
        writer.write_all(&len.to_le_bytes())?;
        writer.write_all(&self.payload)
    }

    pub fn read(reader: &mut impl Read) -> std::io::Result<Self> {
        let mut header = [0u8; HEADER_LEN];
        reader.read_exact(&mut header)?;
        let channel = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        let msg_type = header[4];
        let len = u32::from_le_bytes([header[5], header[6], header[7], header[8]]) as usize;
        if len > MAX_PACKET_PAYLOAD_LEN {
            return Err(Error::new(ErrorKind::InvalidData, "packet payload exceeds maximum length"));
        }
        let mut payload = vec![0u8; len];
        reader.read_exact(&mut payload)?;
        Ok(Self { channel, msg_type, payload })
    }

    pub fn read_from_buffer(buffer: &mut Vec<u8>) -> std::io::Result<Option<Self>> {
        if buffer.len() < HEADER_LEN {
            return Ok(None);
        }
        let channel = u32::from_le_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]);
        let msg_type = buffer[4];
        let len = u32::from_le_bytes([buffer[5], buffer[6], buffer[7], buffer[8]]) as usize;
        if len > MAX_PACKET_PAYLOAD_LEN {
            return Err(Error::new(ErrorKind::InvalidData, "packet payload exceeds maximum length"));
        }
        let frame_len = HEADER_LEN.checked_add(len).ok_or_else(|| Error::new(ErrorKind::InvalidData, "packet length overflow"))?;
        if buffer.len() < frame_len {
            return Ok(None);
        }
        let payload = buffer[HEADER_LEN..frame_len].to_vec();
        buffer.drain(..frame_len);
        Ok(Some(Self { channel, msg_type, payload }))
    }
}

pub struct PacketClient<S> {
    stream: S,
    buffer: Vec<u8>,
}

impl<S: Read + Write> PacketClient<S> {
    pub fn new(stream: S) -> Self {
        Self { stream, buffer: Vec::new() }
    }

    pub fn open_channel(&mut self, channel: u32, session_id: &str, role: ChannelRole) -> std::io::Result<()> {
        self.write(CHANNEL_CONTROL, MSG_CONTROL_OPEN_CHANNEL, &OpenChannel {
            channel,
            session_id: session_id.to_string(),
            role,
            take: false,
        })
    }

    pub fn close_channel(&mut self, channel: u32, reason: Option<String>) -> std::io::Result<()> {
        self.write(CHANNEL_CONTROL, MSG_CONTROL_CLOSE_CHANNEL, &CloseChannel { channel, reason })
    }

    pub fn ack(&mut self, channel: u32, generation: u64) -> std::io::Result<()> {
        self.write(channel, MSG_SESSION_ACK, &Ack { generation })
    }

    pub fn input(&mut self, channel: u32, event: TerminalInputEvent) -> std::io::Result<()> {
        self.write(channel, MSG_SESSION_INPUT, &Input { event })
    }

    pub fn resize(&mut self, channel: u32, cols: u16, rows: u16) -> std::io::Result<()> {
        self.write(channel, MSG_SESSION_RESIZE, &Resize { cols, rows })
    }

    pub fn read_frame(&mut self) -> std::io::Result<PacketFrame> {
        loop {
            if let Some(frame) = PacketFrame::read_from_buffer(&mut self.buffer)? {
                return Ok(frame);
            }
            let mut chunk = [0; 8192];
            let n = self.stream.read(&mut chunk)?;
            if n == 0 {
                return Err(Error::new(ErrorKind::UnexpectedEof, "packet stream closed"));
            }
            self.buffer.extend_from_slice(&chunk[..n]);
        }
    }

    pub fn read_render(&mut self, channel: u32) -> std::io::Result<RenderPacket> {
        loop {
            let frame = self.read_frame()?;
            if frame.channel == channel && frame.msg_type == MSG_SESSION_RENDER {
                return frame.decode();
            }
            if frame.channel == CHANNEL_CONTROL && frame.msg_type == MSG_CONTROL_ERROR {
                let error = frame.decode::<ControlError>()?;
                if error.channel == channel {
                    return Err(Error::new(ErrorKind::InvalidData, error.message));
                }
            }
        }
    }

    pub fn read_directory_delta(&mut self) -> std::io::Result<DirectoryDelta> {
        loop {
            let frame = self.read_frame()?;
            if frame.channel == CHANNEL_CONTROL && frame.msg_type == MSG_CONTROL_DIRECTORY_DELTA {
                return frame.decode();
            }
        }
    }

    fn write<T: Serialize>(&mut self, channel: u32, msg_type: u8, value: &T) -> std::io::Result<()> {
        PacketFrame::new(channel, msg_type, value)?.write(&mut self.stream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_round_trips_postcard_control_payloads() {
        let hello = ControlHello::current();
        let frame = PacketFrame::new(CHANNEL_CONTROL, MSG_CONTROL_HELLO, &hello).expect("encode hello");
        let mut bytes = Vec::new();
        frame.write(&mut bytes).expect("write frame");

        let decoded = PacketFrame::read(&mut bytes.as_slice()).expect("read frame");

        assert_eq!(decoded.channel, CHANNEL_CONTROL);
        assert_eq!(decoded.msg_type, MSG_CONTROL_HELLO);
        assert_eq!(decoded.decode::<ControlHello>().expect("decode hello"), hello);
    }

    #[test]
    fn buffer_reader_skips_unknown_message_payload_by_length() {
        let unknown = PacketFrame { channel: CHANNEL_CONTROL, msg_type: 250, payload: vec![1, 2, 3, 4] };
        let directory = DirectorySnapshot {
            sessions: vec![DirectoryEntry {
                session_id: "alpha".to_string(),
                tags: vec!["role=impl".to_string()],
                state: "running".to_string(),
                controller_count: 0,
                watcher_count: 0,
                recreatable: false,
                cols: 80,
                rows: 24,
            }],
        };
        let known = PacketFrame::new(CHANNEL_CONTROL, MSG_CONTROL_DIRECTORY_SNAPSHOT, &directory).expect("encode directory");
        let mut bytes = Vec::new();
        unknown.write(&mut bytes).expect("write unknown");
        known.write(&mut bytes).expect("write known");

        let first = PacketFrame::read_from_buffer(&mut bytes).expect("read unknown").expect("unknown frame");
        let second = PacketFrame::read_from_buffer(&mut bytes).expect("read known").expect("known frame");

        assert_eq!(first.msg_type, 250);
        assert_eq!(first.payload, vec![1, 2, 3, 4]);
        assert_eq!(second.decode::<DirectorySnapshot>().expect("decode directory"), directory);
        assert!(bytes.is_empty());
    }

    #[test]
    fn read_from_buffer_waits_for_complete_payload() {
        let frame = PacketFrame { channel: 3, msg_type: 99, payload: vec![1, 2, 3] };
        let mut bytes = Vec::new();
        frame.write(&mut bytes).expect("write frame");
        bytes.pop();

        assert!(PacketFrame::read_from_buffer(&mut bytes).expect("read partial").is_none());
    }

    #[test]
    fn oversized_packet_payload_is_rejected_before_allocation() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&CHANNEL_CONTROL.to_le_bytes());
        bytes.push(MSG_CONTROL_HELLO);
        bytes.extend_from_slice(&(MAX_PACKET_PAYLOAD_LEN as u32 + 1).to_le_bytes());

        let err = PacketFrame::read(&mut bytes.as_slice()).expect_err("oversized frame rejected");
        assert_eq!(err.kind(), ErrorKind::InvalidData);

        let err = PacketFrame::read_from_buffer(&mut bytes).expect_err("oversized buffered frame rejected");
        assert_eq!(err.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn packet_client_read_render_surfaces_matching_control_error() {
        let mut bytes = Vec::new();
        PacketFrame::new(CHANNEL_CONTROL, MSG_CONTROL_ERROR, &ControlError { channel: 7, message: "bad channel".to_string() })
            .expect("encode error")
            .write(&mut bytes)
            .expect("write error");
        let mut client = PacketClient::new(std::io::Cursor::new(bytes));

        let err = client.read_render(7).expect_err("control error should surface");
        assert_eq!(err.kind(), ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "bad channel");
    }
}
