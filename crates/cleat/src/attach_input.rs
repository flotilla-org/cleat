//! Incremental terminal decoding followed by attachment-local command matching.
//! Neither transport reads nor bracketed paste fragments are input operations.
use crate::provider::{TerminalKey, TerminalKeyAction, TerminalKeyEvent, TerminalModifiers, TerminalNamedKey};

const PASTE_LIMIT: usize = 1024 * 1024;
const PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Debug, PartialEq)]
pub(crate) enum Command {
    Pan(i16, i16),
    RevealCursor,
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
    Key(TerminalKeyEvent),
    KeyboardFlags(u32),
    MouseCellSize(u16, u16),
    EnablePixelMouse,
    GraphicsReply(Vec<u8>),
    Paste(String),
    Command(Command),
    Hint(&'static str),
    Focus(bool),
    Mouse(crate::provider::TerminalMouseEvent),
}

pub(crate) struct InputDecoder {
    prefix: u8,
    armed: bool,
    panning: bool,
    escape: Vec<u8>,
    discard_escape: bool,
    keyboard_flags: u32,
    mouse: crate::attach_mouse::MouseMode,
    forwarded_buttons: Vec<crate::provider::TerminalMouseEvent>,
    forwarded_keys: Vec<TerminalKeyEvent>,
    paste: Option<Vec<u8>>,
    paste_tail: Vec<u8>,
    overflow: bool,
    driving: bool,
    paste_authorized: bool,
    paste_started_in_pan: bool,
}

impl InputDecoder {
    pub fn new(prefix: u8) -> Self {
        Self {
            prefix,
            armed: false,
            panning: false,
            escape: Vec::new(),
            discard_escape: false,
            keyboard_flags: 0,
            mouse: Default::default(),
            forwarded_buttons: Vec::new(),
            forwarded_keys: Vec::new(),
            paste: None,
            paste_tail: Vec::new(),
            overflow: false,
            driving: true,
            paste_authorized: true,
            paste_started_in_pan: false,
        }
    }

    pub fn is_panning(&self) -> bool {
        self.panning
    }

    pub fn set_pixel_origin(&mut self, origin: u32) {
        self.mouse.set_pixel_origin(origin);
    }

    pub fn set_keyboard_flags(&mut self, flags: u32) {
        self.keyboard_flags = flags;
    }

    pub fn set_driving(&mut self, driving: bool) {
        if self.driving && !driving {
            self.forwarded_keys.clear();
            self.forwarded_buttons.clear();
        }
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
                    if self.paste_started_in_pan {
                        actions.push(Action::Hint("Paste discarded; attachment was in pan mode"));
                    } else if !self.paste_authorized {
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
            if self.discard_escape {
                if (0x40..=0x7e).contains(&byte) {
                    self.discard_escape = false;
                    actions.push(Action::Hint("Oversized terminal key report discarded"));
                }
                continue;
            }
            if !self.escape.is_empty() {
                self.escape.push(byte);
                let apc = self.escape.get(1) == Some(&b'_');
                if apc {
                    if self.escape.ends_with(b"\x1b\\") {
                        let sequence = std::mem::take(&mut self.escape);
                        actions.push(Action::GraphicsReply(sequence));
                    } else if self.escape.len() > 512 {
                        // Keep consuming through ST without retaining an unbounded reply.
                        let last = *self.escape.last().unwrap();
                        self.escape = vec![0x1b, b'_', last];
                    }
                    continue;
                }
                let complete = if self.escape.get(1) == Some(&b'[') {
                    self.escape.len() >= 3 && (0x40..=0x7e).contains(&byte)
                } else if self.escape.get(1) == Some(&b'O') {
                    self.escape.len() >= 3
                } else {
                    true
                };
                if !complete && self.escape.len() >= 4096 {
                    self.escape.clear();
                    self.discard_escape = true;
                    continue;
                }
                if complete {
                    let sequence = std::mem::take(&mut self.escape);
                    if let Some(replies) = self.mouse.reply(&sequence) {
                        for reply in replies {
                            match reply {
                                crate::attach_mouse::Reply::CellSize(w, h) => actions.push(Action::MouseCellSize(w, h)),
                                crate::attach_mouse::Reply::EnablePixels => actions.push(Action::EnablePixelMouse),
                                crate::attach_mouse::Reply::Consumed => {}
                            }
                        }
                        continue;
                    }
                    if sequence.starts_with(b"\x1b[<") {
                        if let Some(mouse) = self.mouse.decode(&sequence) {
                            self.mouse_event(mouse, &mut actions);
                        } else {
                            actions.push(Action::Hint("Invalid mouse report discarded"));
                        }
                        continue;
                    }
                    match crate::attach_keyboard::decode(&sequence, self.keyboard_flags) {
                        crate::attach_keyboard::Report::Flags(flags) => {
                            self.keyboard_flags = flags;
                            actions.push(Action::KeyboardFlags(flags));
                            continue;
                        }
                        crate::attach_keyboard::Report::Key { event, tap } => {
                            self.key(event, tap, &mut actions);
                            continue;
                        }
                        crate::attach_keyboard::Report::Unsupported => {
                            actions.push(Action::Hint("Unsupported terminal key report discarded"));
                            continue;
                        }
                        crate::attach_keyboard::Report::NotKey => {}
                    }
                    if sequence == b"\x1b[200~" {
                        self.paste = Some(Vec::new());
                        self.paste_authorized = self.driving;
                        self.paste_started_in_pan = self.panning;
                        self.overflow = false;
                        self.armed = false;
                    } else if sequence == b"\x1b[I" {
                        actions.push(Action::Focus(true));
                    } else if sequence == b"\x1b[O" {
                        actions.push(Action::Focus(false));
                    } else if (self.armed || self.panning)
                        && matches!(
                            sequence.as_slice(),
                            b"\x1b[A" | b"\x1b[B" | b"\x1b[C" | b"\x1b[D" | b"\x1bOA" | b"\x1bOB" | b"\x1bOC" | b"\x1bOD"
                        )
                    {
                        self.armed = false;
                        if !self.panning {
                            self.release_forwarded(&mut actions);
                        }
                        self.panning = true;
                        let (x, y) = match sequence[2] {
                            b'A' => (0, -1),
                            b'B' => (0, 1),
                            b'C' => (1, 0),
                            _ => (-1, 0),
                        };
                        actions.push(Action::Command(Command::Pan(x, y)));
                    } else if self.armed || self.panning {
                        self.armed = false;
                        actions.push(Action::Hint("Unknown cleat command"));
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
                let command = local_command(byte);
                if byte == self.prefix {
                    push_raw(&mut actions, &[byte]);
                } else if let Some(command) = command {
                    actions.push(Action::Command(command));
                } else {
                    actions.push(Action::Hint("Unknown cleat command"));
                }
            } else if byte == self.prefix {
                self.armed = true;
                actions.push(Action::Hint(COMMAND_HINT));
            } else if self.panning {
                // Pan mode consumes application keystrokes until Escape,
                // while prefixed attachment commands remain available.
            } else {
                push_raw(&mut actions, &[byte]);
            }
        }
        actions
    }

    fn mouse_event(&mut self, mut event: crate::provider::TerminalMouseEvent, actions: &mut Vec<Action>) {
        use crate::provider::{TerminalMouseButton as Button, TerminalMouseButtons as Buttons, TerminalMouseEventKind as Kind};
        if (self.armed || self.panning) && event.kind != Kind::Release {
            return;
        }
        let index = self.forwarded_buttons.iter().position(|e| e.button == event.button);
        for held in &mut self.forwarded_buttons {
            held.x_px = event.x_px;
            held.y_px = event.y_px;
            held.cell_col = event.cell_col;
            held.cell_row = event.cell_row;
        }
        match event.kind {
            Kind::Press if self.driving && index.is_none() => self.forwarded_buttons.push(event.clone()),
            Kind::Release => {
                if let Some(i) = index {
                    self.forwarded_buttons.remove(i);
                } else {
                    return;
                }
            }
            Kind::Move if event.button.is_some() && index.is_none() => return,
            _ => {}
        }
        for held in &self.forwarded_buttons {
            event.buttons |= match held.button {
                Some(Button::Left) => Buttons::LEFT,
                Some(Button::Middle) => Buttons::MIDDLE,
                Some(Button::Right) => Buttons::RIGHT,
                Some(Button::Back) => Buttons::BACK,
                Some(Button::Forward) => Buttons::FORWARD,
                None => Buttons::empty(),
            };
        }
        actions.push(Action::Mouse(event));
    }

    fn release_forwarded(&mut self, actions: &mut Vec<Action>) {
        for mut event in self.forwarded_buttons.drain(..) {
            event.kind = crate::provider::TerminalMouseEventKind::Release;
            event.buttons = crate::provider::TerminalMouseButtons::empty();
            event.modifiers = TerminalModifiers::empty();
            actions.push(Action::Mouse(event));
        }
        for mut event in self.forwarded_keys.drain(..) {
            event.action = TerminalKeyAction::Release;
            event.modifiers = TerminalModifiers::empty();
            event.consumed_modifiers = TerminalModifiers::empty();
            actions.push(Action::Key(event));
        }
    }

    fn key(&mut self, mut event: TerminalKeyEvent, tap: bool, actions: &mut Vec<Action>) {
        let held = self
            .forwarded_keys
            .iter()
            .position(|key| key.key == event.key || (key.physical_key.is_some() && key.physical_key == event.physical_key));
        if event.action == TerminalKeyAction::Release {
            if let Some(index) = held {
                let original = self.forwarded_keys.remove(index);
                event.key = original.key;
                event.physical_key = original.physical_key;
                event.generated_text = original.generated_text;
                actions.push(Action::Key(event));
            }
            return;
        }
        let byte = command_byte(&event);
        let pan = if event.modifiers.intersects(TerminalModifiers::CTRL | TerminalModifiers::ALT | TerminalModifiers::SUPER) {
            None
        } else {
            match event.key {
                TerminalKey::Named(TerminalNamedKey::ArrowUp) => Some((0, -1)),
                TerminalKey::Named(TerminalNamedKey::ArrowDown) => Some((0, 1)),
                TerminalKey::Named(TerminalNamedKey::ArrowLeft) => Some((-1, 0)),
                TerminalKey::Named(TerminalNamedKey::ArrowRight) => Some((1, 0)),
                _ => None,
            }
        };
        if let Some((x, y)) = pan.filter(|_| self.armed || self.panning) {
            self.armed = false;
            if !self.panning {
                self.release_forwarded(actions);
            }
            self.panning = true;
            actions.push(Action::Command(Command::Pan(x, y)));
            return;
        }
        if event.action == TerminalKeyAction::Repeat && held.is_none() {
            return;
        }
        if (self.armed || self.panning) && byte == Some(0x1b) {
            self.armed = false;
            self.panning = false;
            actions.push(Action::Hint(""));
            return;
        }
        if self.armed {
            self.armed = false;
            if byte != Some(self.prefix) {
                let command = byte.and_then(local_command);
                actions.push(command.map(Action::Command).unwrap_or(Action::Hint("Unknown cleat command")));
                return;
            }
        } else if byte == Some(self.prefix) {
            self.armed = true;
            actions.push(Action::Hint(COMMAND_HINT));
            return;
        } else if self.panning {
            return;
        }
        if let Some(index) = held {
            event.key = self.forwarded_keys[index].key.clone();
            event.physical_key = self.forwarded_keys[index].physical_key.clone();
        }
        if !tap && held.is_none() && self.driving {
            if self.forwarded_keys.len() >= 256 {
                actions.push(Action::Hint("Too many held keys; key discarded"));
                return;
            }
            self.forwarded_keys.push(event.clone());
        }
        actions.push(Action::Key(event.clone()));
        if tap {
            event.action = TerminalKeyAction::Release;
            actions.push(Action::Key(event));
        }
    }

    /// Resolve an isolated Escape after the input timeout. Partial CSI/paste
    /// sequences remain buffered; an interrupted paste is discarded on drop.
    pub fn idle(&mut self) -> Vec<Action> {
        if self.escape == [0x1b] {
            self.escape.clear();
            if self.armed || self.panning {
                self.armed = false;
                self.panning = false;
                vec![Action::Hint("")]
            } else {
                vec![Action::Raw(vec![0x1b])]
            }
        } else {
            Vec::new()
        }
    }
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
    fn graphics_replies_are_consumed_separately_even_when_fragmented() {
        let mut decoder = InputDecoder::new(0x1d);
        assert!(decoder.feed(b"\x1b_Gi=7;").is_empty());
        assert_eq!(decoder.feed(b"OK\x1b\\x"), vec![Action::GraphicsReply(b"\x1b_Gi=7;OK\x1b\\".to_vec()), Action::Raw(b"x".to_vec())]);
    }

    #[test]
    fn prefixed_commands_remain_available_while_panning() {
        for (key, command) in [(b'c', Command::Chrome), (b'd', Command::Detach), (b'g', Command::Drive), (b'r', Command::RevealCursor)] {
            let mut decoder = InputDecoder::new(0x1d);
            decoder.feed(b"\x1d\x1b[C");
            assert!(decoder.is_panning());
            assert!(matches!(decoder.feed(b"\x1d").as_slice(), [Action::Hint(_)]));
            assert_eq!(decoder.feed(&[key]), vec![Action::Command(command)]);
            assert!(decoder.is_panning());
            assert_eq!(decoder.feed(b"\x1b[B"), vec![Action::Command(Command::Pan(0, 1))]);
            assert!(decoder.feed(b"typing").is_empty());
        }
    }

    #[test]
    fn paste_during_pan_reports_pan_mode_and_never_replays_bytes() {
        let bytes = b"\x1b[200~discard\x1dc\x1b[201~";
        for split in 0..=bytes.len() {
            let mut decoder = InputDecoder::new(0x1d);
            decoder.feed(b"\x1d\x1b[C");
            let mut actions = decoder.feed(&bytes[..split]);
            actions.extend(decoder.feed(&bytes[split..]));
            assert_eq!(actions, vec![Action::Hint("Paste discarded; attachment was in pan mode")], "split {split}");
            assert!(decoder.is_panning());
            decoder.feed(b"\x1b");
            decoder.idle();
            assert_eq!(decoder.feed(b"\x1b[200~accepted\x1b[201~"), vec![Action::Paste("accepted".into())]);
        }
    }

    #[test]
    fn pan_mode_handles_fragmented_normal_and_application_arrows() {
        for arrow in [b"\x1b[C".as_slice(), b"\x1bOC".as_slice()] {
            for split in 0..=arrow.len() {
                let mut decoder = InputDecoder::new(0x1d);
                decoder.set_driving(false);
                decoder.feed(b"\x1d");
                let mut actions = decoder.feed(&arrow[..split]);
                actions.extend(decoder.feed(&arrow[split..]));
                assert_eq!(actions, vec![Action::Command(Command::Pan(1, 0))]);
                assert_eq!(decoder.feed(b"\x1b[B"), vec![Action::Command(Command::Pan(0, 1))]);
                assert!(decoder.feed(b"typed").is_empty());
                decoder.feed(b"\x1b");
                assert_eq!(decoder.idle(), vec![Action::Hint("")]);
                assert_eq!(decoder.feed(b"z"), vec![Action::Raw(vec![b'z'])]);
                decoder.feed(b"\x1d");
                assert_eq!(decoder.feed(b"r"), vec![Action::Command(Command::RevealCursor)]);
            }
        }
    }

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

const COMMAND_HINT: &str = "cleat: d detach | g drive | w watch | x exclusive | c chrome | a auto size | [ history | ] live | k/j scroll | arrows pan | r reveal cursor | Esc cancel";

fn local_command(byte: u8) -> Option<Command> {
    match byte {
        b'r' => Some(Command::RevealCursor),
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
    }
}

fn command_byte(event: &TerminalKeyEvent) -> Option<u8> {
    if event.modifiers.intersects(TerminalModifiers::ALT | TerminalModifiers::SUPER) {
        return None;
    }
    match event.key {
        TerminalKey::Named(TerminalNamedKey::Escape) if !event.modifiers.contains(TerminalModifiers::CTRL) => Some(0x1b),
        TerminalKey::UnicodeScalar(c) if c < 128 => {
            let c = c as u8;
            if event.modifiers.contains(TerminalModifiers::CTRL) {
                if (b'@'..=b'_').contains(&c) || c.is_ascii_alphabetic() {
                    Some(c.to_ascii_uppercase() & 0x1f)
                } else {
                    None
                }
            } else {
                Some(
                    event
                        .generated_text
                        .as_deref()
                        .filter(|s| s.len() == 1 && s.is_ascii())
                        .map(|s| s.as_bytes()[0])
                        .unwrap_or_else(|| if event.modifiers.contains(TerminalModifiers::SHIFT) { c.to_ascii_uppercase() } else { c }),
                )
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod keyboard_tests {
    use super::*;
    fn decoder() -> InputDecoder {
        let mut d = InputDecoder::new(0x1d);
        d.set_keyboard_flags(31);
        d
    }
    #[test]
    fn kitty_events_and_replies_survive_every_read_split() {
        let bytes = b"\x1b[?31u\x1b[122:90:119;2;90u\x1b[122;2:2u\x1b[122;1:3u";
        let expected = decoder().feed(bytes);
        assert_eq!(expected.len(), 4);
        assert_eq!(expected[0], Action::KeyboardFlags(31));
        let Action::Key(release) = &expected[3] else { panic!("missing key release") };
        assert_eq!(release.action, TerminalKeyAction::Release);
        assert_eq!(release.physical_key.as_deref(), Some("KeyW"));
        for split in 0..=bytes.len() {
            let mut d = decoder();
            let mut actual = d.feed(&bytes[..split]);
            actual.extend(d.feed(&bytes[split..]));
            assert_eq!(actual, expected, "split {split}");
        }
    }
    #[test]
    fn local_commands_consume_their_repeats_and_releases() {
        let mut d = decoder();
        assert!(matches!(d.feed(b"\x1b[93;5u").as_slice(), [Action::Hint(_)]));
        assert!(d.feed(b"\x1b[93;5:2u\x1b[93;5:3u").is_empty());
        assert_eq!(d.feed(b"\x1b[100;;100u"), vec![Action::Command(Command::Detach)]);
        assert!(d.feed(b"\x1b[100;1:3u").is_empty());
        let mut d = decoder();
        d.feed(b"\x1b[93;5u\x1b[93;5:3u");
        assert!(matches!(d.feed(b"\x1b[93;5u").as_slice(), [Action::Key(_)]));
        assert!(matches!(d.feed(b"\x1b[93;1:3u").as_slice(), [Action::Key(_)]));
    }
    #[test]
    fn pan_mode_releases_application_holds_and_accepts_local_arrow_repeats() {
        let mut d = decoder();
        d.feed(b"\x1b[119;;119u");
        d.feed(b"\x1b[93;5u");
        let actions = d.feed(b"\x1b[C");
        assert!(matches!(&actions[0],Action::Key(k) if k.action==TerminalKeyAction::Release));
        assert_eq!(actions[1], Action::Command(Command::Pan(1, 0)));
        assert_eq!(d.feed(b"\x1b[1;1:2C"), vec![Action::Command(Command::Pan(1, 0))]);
        assert!(d.feed(b"\x1b[119;1:2u\x1b[119;1:3u").is_empty());
        assert_eq!(d.feed(b"\x1b[27u"), vec![Action::Hint("")]);
        assert!(!d.is_panning());
    }
    #[test]
    fn paste_and_oversized_reports_do_not_turn_into_commands_or_replies() {
        let mut d = decoder();
        assert_eq!(d.feed(b"\x1b[200~\x1b[?31u\x1b[93;5u\x1b[201~"), vec![Action::Paste("\x1b[?31u\x1b[93;5u".into())]);
        let mut bytes = b"\x1b[".to_vec();
        bytes.extend(vec![b'1'; 8192]);
        bytes.extend(b"uok");
        assert_eq!(d.feed(&bytes), vec![Action::Hint("Oversized terminal key report discarded"), Action::Raw(b"ok".to_vec())]);
        assert!(d.escape.is_empty());
    }
}

#[cfg(test)]
mod mouse_tests {
    use super::*;
    use crate::provider::{TerminalMouseButtons as Buttons, TerminalMouseEventKind as Kind};
    #[test]
    fn fragmented_negotiation_chords_and_pan_release() {
        let bytes = b"\x1b[6;20;10t\x1b[?1016;2$y\x1b[?1016;1$y\x1b[<0;16;26M\x1b[<2;16;26M\x1b[<0;16;26m";
        for split in 0..=bytes.len() {
            let mut decoder = InputDecoder::new(0x1d);
            let mut actions = decoder.feed(&bytes[..split]);
            actions.extend(decoder.feed(&bytes[split..]));
            assert!(matches!(actions[0], Action::MouseCellSize(10, 20)));
            assert!(matches!(actions[1], Action::EnablePixelMouse));
            let Action::Mouse(event) = &actions[3] else { panic!("mouse chord") };
            assert_eq!(event.buttons, Buttons::LEFT | Buttons::RIGHT);
            assert_eq!((event.x_px, event.y_px), (1.5, 1.25));
            let Action::Mouse(event) = &actions[4] else { panic!("mouse release") };
            assert_eq!(event.buttons, Buttons::RIGHT);
            let pan = decoder.feed(b"\x1d\x1b[C");
            assert!(pan.iter().any(|a| matches!(a, Action::Mouse(e) if e.kind == Kind::Release)));
            assert!(decoder.feed(b"\x1b[<34;20;26M\x1b[<2;20;26m").is_empty());
        }
    }
    #[test]
    fn paste_does_not_negotiate_mouse_and_malformed_mouse_does_not_leak() {
        let mut decoder = InputDecoder::new(0x1d);
        assert!(matches!(decoder.feed(b"\x1b[200~\x1b[6;20;10t\x1b[201~").as_slice(), [Action::Paste(_)]));
        assert!(matches!(decoder.feed(b"\x1b[<0;0;2M").as_slice(), [Action::Hint(_)]));
    }
}
