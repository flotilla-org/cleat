//! Incremental terminal decoding followed by attachment-local command matching.
//! Neither transport reads nor bracketed paste fragments are input operations.
const PASTE_LIMIT: usize = 1024 * 1024;
const PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Debug, PartialEq)]
pub(crate) enum Command {
    Detach,
    AutoSize,
    Watch,
    Drive,
    Exclusive,
    Chrome,
    Top,
    Bottom,
    Up,
    Down,
}
#[derive(Debug, PartialEq)]
pub(crate) enum Action {
    Raw(Vec<u8>),
    Paste(String),
    Command(Command),
    Hint(&'static str),
    Focus(bool),
    Mouse(crate::provider::TerminalMouseEvent),
}

pub(crate) struct InputDecoder {
    prefix: u8,
    armed: bool,
    escape: Vec<u8>,
    paste: Option<Vec<u8>>,
    paste_tail: Vec<u8>,
    overflow: bool,
    driving: bool,
    paste_authorized: bool,
}

impl InputDecoder {
    pub fn new(prefix: u8) -> Self {
        Self {
            prefix,
            armed: false,
            escape: Vec::new(),
            paste: None,
            paste_tail: Vec::new(),
            overflow: false,
            driving: true,
            paste_authorized: true,
        }
    }

    pub fn set_driving(&mut self, driving: bool) {
        self.driving = driving;
        if self.paste.is_some() {
            self.paste_authorized &= driving;
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Action> {
        let mut actions = Vec::new();
        for &byte in bytes {
            if let Some(paste) = &mut self.paste {
                self.paste_tail.push(byte);
                if self.paste_tail.len() > PASTE_END.len() {
                    self.paste_tail.remove(0);
                }
                if !self.overflow {
                    paste.push(byte);
                    if paste.len() > PASTE_LIMIT + PASTE_END.len() {
                        paste.clear();
                        self.overflow = true;
                    }
                }
                if self.paste_tail == PASTE_END {
                    let mut paste = self.paste.take().expect("collecting paste");
                    if !self.paste_authorized {
                        actions.push(Action::Hint("Paste discarded; attachment was not driving"));
                    } else if self.overflow {
                        actions.push(Action::Hint("Paste exceeds 1 MiB; discarded"));
                    } else {
                        paste.truncate(paste.len() - PASTE_END.len());
                        match String::from_utf8(paste) {
                            Ok(text) => actions.push(Action::Paste(text)),
                            Err(_) => actions.push(Action::Hint("Paste is not UTF-8; discarded")),
                        }
                    }
                    self.paste_tail.clear();
                }
                continue;
            }
            if !self.escape.is_empty() {
                self.escape.push(byte);
                let complete =
                    if self.escape.get(1) == Some(&b'[') { self.escape.len() >= 3 && (0x40..=0x7e).contains(&byte) } else { true };
                if complete || self.escape.len() >= 128 {
                    let sequence = std::mem::take(&mut self.escape);
                    if sequence == b"\x1b[200~" {
                        self.paste = Some(Vec::new());
                        self.paste_authorized = self.driving;
                        self.overflow = false;
                        self.armed = false;
                    } else if sequence == b"\x1b[I" {
                        actions.push(Action::Focus(true));
                    } else if sequence == b"\x1b[O" {
                        actions.push(Action::Focus(false));
                    } else if self.armed {
                        self.armed = false;
                        actions.push(Action::Hint("Unknown cleat command"));
                    } else if let Some(mouse) = decode_mouse(&sequence) {
                        actions.push(Action::Mouse(mouse));
                    } else {
                        push_raw(&mut actions, &sequence);
                    }
                }
                continue;
            }
            if byte == 0x1b {
                self.escape.push(byte);
                continue;
            }
            if self.armed {
                self.armed = false;
                let command = match byte {
                    b'd' => Some(Command::Detach),
                    b'a' => Some(Command::AutoSize),
                    b'w' => Some(Command::Watch),
                    b'g' => Some(Command::Drive),
                    b'x' => Some(Command::Exclusive),
                    b'c' => Some(Command::Chrome),
                    b'[' => Some(Command::Top),
                    b']' => Some(Command::Bottom),
                    b'k' => Some(Command::Up),
                    b'j' => Some(Command::Down),
                    _ => None,
                };
                if byte == self.prefix {
                    push_raw(&mut actions, &[byte]);
                } else if let Some(command) = command {
                    actions.push(Action::Command(command));
                } else {
                    actions.push(Action::Hint("Unknown cleat command"));
                }
            } else if byte == self.prefix {
                self.armed = true;
                actions.push(Action::Hint(
                    "cleat: d detach | g drive | w watch | x exclusive | c chrome | a auto size | [ history | ] live | k/j scroll | Esc cancel",
                ));
            } else {
                push_raw(&mut actions, &[byte]);
            }
        }
        actions
    }

    /// Resolve an isolated Escape after the input timeout. Partial CSI/paste
    /// sequences remain buffered; an interrupted paste is discarded on drop.
    pub fn idle(&mut self) -> Vec<Action> {
        if self.escape == [0x1b] {
            self.escape.clear();
            if self.armed {
                self.armed = false;
                vec![Action::Hint("")]
            } else {
                vec![Action::Raw(vec![0x1b])]
            }
        } else {
            Vec::new()
        }
    }
}

fn decode_mouse(sequence: &[u8]) -> Option<crate::provider::TerminalMouseEvent> {
    use crate::provider::*;
    let body = sequence.strip_prefix(b"\x1b[<")?;
    let release = *body.last()? == b'm';
    if !release && *body.last()? != b'M' {
        return None;
    }
    let text = std::str::from_utf8(&body[..body.len() - 1]).ok()?;
    let mut fields = text.split(';');
    let code: u16 = fields.next()?.parse().ok()?;
    let col: u16 = fields.next()?.parse::<u16>().ok()?.saturating_sub(1);
    let row: u16 = fields.next()?.parse::<u16>().ok()?.saturating_sub(1);
    if fields.next().is_some() {
        return None;
    }
    let wheel = code & 64 != 0;
    let button = match code & 3 {
        0 => Some(TerminalMouseButton::Left),
        1 => Some(TerminalMouseButton::Middle),
        2 => Some(TerminalMouseButton::Right),
        _ => None,
    };
    let buttons = if release || wheel {
        TerminalMouseButtons::empty()
    } else {
        match button {
            Some(TerminalMouseButton::Left) => TerminalMouseButtons::LEFT,
            Some(TerminalMouseButton::Middle) => TerminalMouseButtons::MIDDLE,
            Some(TerminalMouseButton::Right) => TerminalMouseButtons::RIGHT,
            _ => TerminalMouseButtons::empty(),
        }
    };
    let mut modifiers = TerminalModifiers::empty();
    if code & 4 != 0 {
        modifiers |= TerminalModifiers::SHIFT;
    }
    if code & 8 != 0 {
        modifiers |= TerminalModifiers::ALT;
    }
    if code & 16 != 0 {
        modifiers |= TerminalModifiers::CTRL;
    }
    Some(TerminalMouseEvent {
        kind: if wheel {
            TerminalMouseEventKind::Wheel
        } else if release {
            TerminalMouseEventKind::Release
        } else if code & 32 != 0 {
            TerminalMouseEventKind::Move
        } else {
            TerminalMouseEventKind::Press
        },
        button: if wheel { None } else { button },
        buttons,
        modifiers,
        cell_col: col,
        cell_row: row,
        x_px: f32::from(col) + 0.5,
        y_px: f32::from(row) + 0.5,
        wheel_delta_x: 0.0,
        wheel_delta_y: if wheel {
            if code & 1 == 0 {
                1.0
            } else {
                -1.0
            }
        } else {
            0.0
        },
    })
}

fn push_raw(actions: &mut Vec<Action>, bytes: &[u8]) {
    if let Some(Action::Raw(previous)) = actions.last_mut() {
        previous.extend_from_slice(bytes);
    } else {
        actions.push(Action::Raw(bytes.to_vec()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn commands_and_escape_survive_read_boundaries() {
        let mut decoder = InputDecoder::new(0x1d);
        assert!(matches!(decoder.feed(b"\x1d").as_slice(), [Action::Hint(_)]));
        assert_eq!(decoder.feed(b"d"), vec![Action::Command(Command::Detach)]);
        decoder.feed(b"\x1d");
        assert_eq!(decoder.feed(b"\x1d"), vec![Action::Raw(vec![0x1d])]);
        decoder.feed(b"\x1d\x1b");
        assert_eq!(decoder.idle(), vec![Action::Hint("")]);
        assert_eq!(decoder.feed(b"x"), vec![Action::Raw(b"x".to_vec())]);
        decoder.feed(b"\x1d");
        assert_eq!(decoder.feed(b"?"), vec![Action::Hint("Unknown cleat command")]);
    }
    #[test]
    fn paste_is_one_operation_and_bypasses_commands_at_every_split() {
        let bytes = b"\x1b[200~a\x1ddb\x1b[201~";
        for split in 0..=bytes.len() {
            let mut decoder = InputDecoder::new(0x1d);
            let mut actions = decoder.feed(&bytes[..split]);
            actions.extend(decoder.feed(&bytes[split..]));
            assert_eq!(actions, vec![Action::Paste("a\x1ddb".into())], "split {split}");
        }
    }
    #[test]
    fn watcher_paste_is_not_replayed_after_promotion() {
        let mut decoder = InputDecoder::new(0x1d);
        decoder.set_driving(false);
        decoder.feed(b"\x1b[200~discard");
        decoder.set_driving(true);
        assert_eq!(decoder.feed(b"\x1b[201~"), vec![Action::Hint("Paste discarded; attachment was not driving")]);
    }

    #[test]
    fn oversized_paste_is_discarded_without_sending_a_prefix() {
        let mut decoder = InputDecoder::new(0x1d);
        assert!(decoder.feed(b"\x1b[200~").is_empty());
        assert!(decoder.feed(&vec![b'a'; PASTE_LIMIT + 10]).is_empty());
        assert_eq!(decoder.feed(b"\x1b[201~"), vec![Action::Hint("Paste exceeds 1 MiB; discarded")]);
        assert_eq!(decoder.feed(b"ok"), vec![Action::Raw(b"ok".to_vec())]);
    }
}
