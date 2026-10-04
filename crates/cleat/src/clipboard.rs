//! Live clipboard writes are transient effects, never render or replay state.
use std::{collections::VecDeque, io::Write};

use serde::{Deserialize, Serialize};

pub const MAX_CLIPBOARD_PAYLOAD: usize = 64 * 1024;
pub const MAX_CLIPBOARD_EVENTS: usize = 16;
pub const MAX_CLIPBOARD_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardEvent {
    /// A fresh identity for each hosting actor, never restored from recordings.
    pub session_epoch: [u8; 16],
    pub connection_epoch: u64,
    pub sequence: u64,
    /// Ghostty's normalized destination: standard=0, selection=1, primary=2.
    pub destination: u32,
    /// None clears the destination. Empty representations are unsupported,
    /// since OSC 52 cannot relay them distinctly from a clear.
    pub text: Option<String>,
}
impl ClipboardEvent {
    pub fn valid(&self) -> bool {
        self.destination <= 2 && self.text.as_ref().is_none_or(|s| !s.is_empty() && s.len() <= MAX_CLIPBOARD_PAYLOAD && !s.contains('\0'))
    }
    fn bytes(&self) -> usize {
        self.text.as_ref().map_or(0, String::len)
    }
    /// Encode one live event. Ghostty treats empty OSC 52 data as clear.
    pub fn write_osc52(&self, out: &mut impl Write) -> std::io::Result<()> {
        if !self.valid() {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid clipboard event"));
        }
        let destination = b"csp"[self.destination as usize];
        out.write_all(&[0x1b, b']', b'5', b'2', b';', destination, b';'])?;
        match &self.text {
            None => {}
            Some(text) => {
                const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
                for chunk in text.as_bytes().chunks(3) {
                    let a = chunk[0] as usize;
                    let b = chunk.get(1).copied().unwrap_or(0) as usize;
                    let c = chunk.get(2).copied().unwrap_or(0) as usize;
                    out.write_all(&[
                        ALPHABET[a >> 2],
                        ALPHABET[((a & 3) << 4) | (b >> 4)],
                        if chunk.len() > 1 { ALPHABET[((b & 15) << 2) | (c >> 6)] } else { b'=' },
                        if chunk.len() > 2 { ALPHABET[c & 63] } else { b'=' },
                    ])?;
                }
            }
        }
        out.write_all(b"\x1b\\")
    }
}

#[derive(Default)]
pub(crate) struct ClipboardQueue {
    events: VecDeque<ClipboardEvent>,
    bytes: usize,
    pub(crate) dropped: u64,
}
impl ClipboardQueue {
    /// Drop newest on invalid input or either bound; never block the parser.
    pub(crate) fn push(&mut self, event: ClipboardEvent) -> bool {
        if !event.valid() || self.events.len() >= MAX_CLIPBOARD_EVENTS || self.bytes + event.bytes() > MAX_CLIPBOARD_BYTES {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }
        self.bytes += event.bytes();
        self.events.push_back(event);
        true
    }
    pub(crate) fn pop(&mut self) -> Option<ClipboardEvent> {
        let event = self.events.pop_front()?;
        self.bytes -= event.bytes();
        Some(event)
    }
    pub(crate) fn clear(&mut self) {
        self.dropped = self.dropped.saturating_add(self.events.len() as u64);
        self.events.clear();
        self.bytes = 0;
    }
}

pub(crate) struct ClipboardRouter {
    pub(crate) target: Option<u128>,
    epoch: [u8; 16],
    connection_epoch: u64,
    sequence: u64,
    suspended: bool,
    pub(crate) queue: ClipboardQueue,
}
impl Default for ClipboardRouter {
    fn default() -> Self {
        Self {
            target: None,
            epoch: uuid::Uuid::new_v4().into_bytes(),
            connection_epoch: 0,
            sequence: 0,
            suspended: false,
            queue: Default::default(),
        }
    }
}
impl ClipboardRouter {
    fn advance_connection_epoch(&mut self) {
        if let Some(epoch) = self.connection_epoch.checked_add(1) {
            self.connection_epoch = epoch;
        } else {
            // Rotate the identity namespace instead of reusing an old epoch.
            self.epoch = uuid::Uuid::new_v4().into_bytes();
            self.connection_epoch = 0;
            self.sequence = 0;
        }
    }
    pub(crate) fn set_target(&mut self, target: Option<u128>) {
        if target != self.target {
            self.queue.clear();
            self.advance_connection_epoch();
            self.target = target;
        }
    }
    #[cfg(unix)]
    pub(crate) fn suspend(&mut self) {
        self.queue.clear();
        self.advance_connection_epoch();
        self.suspended = true;
    }
    #[cfg(unix)]
    pub(crate) fn resume(&mut self) {
        self.suspended = false;
    }
    pub(crate) fn accept(&mut self, mut event: ClipboardEvent) -> bool {
        let Some(sequence) = self.sequence.checked_add(1) else {
            self.queue.dropped = self.queue.dropped.saturating_add(1);
            return false;
        };
        self.sequence = sequence;
        if self.target.is_none() || self.suspended {
            self.queue.dropped = self.queue.dropped.saturating_add(1);
            return false;
        }
        event.session_epoch = self.epoch;
        event.connection_epoch = self.connection_epoch;
        event.sequence = self.sequence;
        self.queue.push(event)
    }
    pub(crate) fn drain(&mut self, target: u128) -> Vec<ClipboardEvent> {
        if self.target != Some(target) {
            return Vec::new();
        }
        std::iter::from_fn(|| self.queue.pop()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn event(text: Option<String>) -> ClipboardEvent {
        ClipboardEvent { session_epoch: [0; 16], connection_epoch: 0, sequence: 0, destination: 0, text }
    }
    #[test]
    fn exhausted_connection_epoch_rotates_identity_and_discards_pending() {
        let mut router = ClipboardRouter::default();
        router.set_target(Some(1));
        router.connection_epoch = u64::MAX;
        assert!(router.accept(event(None)));
        let old_epoch = router.epoch;
        router.set_target(Some(2));
        assert!(router.drain(2).is_empty());
        assert!(router.accept(event(None)));
        let next = router.drain(2).pop().unwrap();
        assert_ne!(next.session_epoch, old_epoch);
        assert_eq!(next.connection_epoch, 0);
        assert_eq!(next.sequence, 1);
        #[cfg(unix)]
        {
            router.connection_epoch = u64::MAX;
            router.suspend();
            assert_ne!(router.epoch, next.session_epoch);
            assert_eq!(router.connection_epoch, 0);
        }
    }
    #[test]
    fn exhausted_sequence_drops_without_reusing_identity() {
        let mut router = ClipboardRouter::default();
        router.set_target(Some(1));
        router.sequence = u64::MAX - 1;
        assert!(router.accept(event(None)));
        assert_eq!(router.drain(1)[0].sequence, u64::MAX);
        assert!(!router.accept(event(None)));
        assert!(!router.accept(event(None)));
        assert!(router.drain(1).is_empty());
        assert_eq!(router.queue.dropped, 2);
    }
    #[test]
    fn controller_epoch_fences_pending_effects_and_transfer() {
        // Every target change discards old effects; watchers and late/reconnected
        // controllers never receive a pending event from an earlier activation.
        let mut router = ClipboardRouter::default();
        assert!(!router.accept(event(Some("unattached".into()))));
        router.set_target(Some(1));
        assert!(router.accept(event(Some("first".into()))));
        assert!(router.drain(2).is_empty());
        router.set_target(Some(2));
        assert!(router.drain(1).is_empty());
        assert!(router.drain(2).is_empty());
        assert!(router.accept(event(None)));
        let first = router.drain(2).pop().unwrap();
        assert!(router.drain(2).is_empty());
        router.set_target(None);
        router.set_target(Some(2));
        assert!(router.accept(event(Some("new connection".into()))));
        let second = router.drain(2).pop().unwrap();
        assert!(second.connection_epoch > first.connection_epoch);
        assert!(second.sequence > first.sequence);
        assert_eq!(second.session_epoch, first.session_epoch);
        #[cfg(unix)]
        {
            assert!(router.accept(event(None)));
            router.suspend();
            assert!(!router.accept(event(None)));
            assert!(router.drain(2).is_empty());
            router.resume();
            assert!(router.accept(event(None)));
            assert!(router.drain(2)[0].connection_epoch > second.connection_epoch);
        }
        assert_ne!(ClipboardRouter::default().epoch, router.epoch);
    }
    #[test]
    fn queue_bounds_drop_newest_and_terminal_work_can_continue() {
        // Explicitly cross payload, event-count and byte bounds; clear events
        // cost zero payload bytes but must still consume an event slot.
        for payload in [0, 1, MAX_CLIPBOARD_PAYLOAD - 1, MAX_CLIPBOARD_PAYLOAD, MAX_CLIPBOARD_PAYLOAD + 1] {
            let mut queue = ClipboardQueue::default();
            let count_limit = MAX_CLIPBOARD_EVENTS.min(MAX_CLIPBOARD_BYTES.checked_div(payload).unwrap_or(MAX_CLIPBOARD_EVENTS));
            let mut accepted = 0;
            for _ in 0..MAX_CLIPBOARD_EVENTS + 2 {
                accepted += usize::from(queue.push(event(if payload == 0 { None } else { Some("x".repeat(payload)) })));
                assert!(queue.bytes <= MAX_CLIPBOARD_BYTES);
                assert!(queue.events.len() <= MAX_CLIPBOARD_EVENTS);
            }
            assert_eq!(accepted, if payload > MAX_CLIPBOARD_PAYLOAD { 0 } else { count_limit });
            assert_eq!(queue.dropped, (MAX_CLIPBOARD_EVENTS + 2 - accepted) as u64);
            for _ in 0..accepted {
                assert_eq!(queue.pop().unwrap().text.as_ref().map_or(0, String::len), payload);
            }
            assert!(queue.pop().is_none());
            assert!(queue.push(event(None)));
            assert_eq!(queue.pop().unwrap().text, None);
        }
        let mut queue = ClipboardQueue::default();
        assert!(!queue.push(event(Some("binary\0text".into()))));
    }
}
