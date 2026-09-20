//! The outer terminal's keyboard protocol is independent of the application's.
use std::io::{self, Write};

use crate::provider::{TerminalKey, TerminalKeyAction, TerminalKeyEvent, TerminalModifiers as Mods, TerminalNamedKey as Named};

pub(crate) const QUERY: &[u8] = b"\x1b[?u";
pub(crate) const FLAGS: u32 = 31;

#[derive(Debug, Default)]
pub(crate) struct KeyboardMode {
    supported: bool,
    pushed: bool,
    closed: bool,
}
impl KeyboardMode {
    /// Called only after a valid capability reply. Returns whether this call
    /// starts negotiation, so the decoder can expect events while the reply
    /// confirming the requested flags is in flight.
    pub fn enable(&mut self, writer: &mut impl Write) -> io::Result<bool> {
        if self.supported || self.closed {
            return Ok(false);
        }
        self.supported = true;
        self.enter_screen(writer)?;
        Ok(true)
    }
    pub fn leave_screen(&mut self, writer: &mut impl Write) -> io::Result<()> {
        if self.pushed {
            writer.write_all(b"\x1b[<u")?;
            self.pushed = false;
        }
        Ok(())
    }
    pub fn enter_screen(&mut self, writer: &mut impl Write) -> io::Result<()> {
        if self.supported && !self.pushed && !self.closed {
            writer.write_all(b"\x1b[>31u")?;
            self.pushed = true;
            writer.write_all(QUERY)?;
        }
        Ok(())
    }
    pub fn close(&mut self, writer: &mut impl Write) -> io::Result<()> {
        self.closed = true;
        self.leave_screen(writer)
    }
}

pub(crate) enum Report {
    NotKey,
    Unsupported,
    Flags(u32),
    Key { event: TerminalKeyEvent, tap: bool },
}

pub(crate) fn decode(sequence: &[u8], flags: u32) -> Report {
    let Some(body) = sequence.strip_prefix(b"\x1b[") else {
        return Report::NotKey;
    };
    let Some((&final_byte, params)) = body.split_last() else {
        return Report::NotKey;
    };
    if final_byte == b'~' && matches!(params, b"200" | b"201") {
        return Report::NotKey;
    }
    if final_byte == b'u' && params.starts_with(b"?") {
        return std::str::from_utf8(&params[1..]).ok().and_then(|s| s.parse().ok()).map(Report::Flags).unwrap_or(Report::Unsupported);
    }
    // Legacy terminals continue to use the byte path. CSI-u is unambiguous
    // even before negotiation, unlike cursor-position and other CSI replies.
    if final_byte != b'u' && (flags == 0 || !matches!(final_byte, b'A' | b'B' | b'C' | b'D' | b'F' | b'H' | b'P' | b'Q' | b'S' | b'~')) {
        return Report::NotKey;
    }
    parse(params, final_byte, flags).unwrap_or(Report::Unsupported)
}

fn parse(params: &[u8], final_byte: u8, flags: u32) -> Option<Report> {
    let text = std::str::from_utf8(params).ok()?;
    let fields: Vec<_> = text.split(';').collect();
    if fields.len() > 3 {
        return None;
    }
    let codes: Vec<_> = fields[0].split(':').collect();
    if codes.len() > 3 || (final_byte != b'u' && codes.len() != 1) {
        return None;
    }
    let number = |s: &str| -> Option<u32> {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            None
        } else {
            s.parse().ok()
        }
    };
    let code = if fields[0].is_empty() && final_byte != b'u' { 1 } else { number(codes[0])? };
    let mods_event: Vec<_> = fields.get(1).copied().unwrap_or("").split(':').collect();
    if mods_event.len() > 2 {
        return None;
    }
    let mods = if mods_event[0].is_empty() { 0 } else { number(mods_event[0])?.checked_sub(1)? };
    // Hyper and Meta have no counterpart in the pinned Ghostty encoder.
    if mods & !0xcf != 0 {
        return None;
    }
    let mut modifiers = Mods::empty();
    for (wire, model) in [(1, Mods::SHIFT), (2, Mods::ALT), (4, Mods::CTRL), (8, Mods::SUPER), (64, Mods::CAPS_LOCK), (128, Mods::NUM_LOCK)]
    {
        if mods & wire != 0 {
            modifiers |= model;
        }
    }
    let action = match mods_event.get(1).map(|s| number(s)).unwrap_or(Some(1))? {
        1 => TerminalKeyAction::Press,
        2 => TerminalKeyAction::Repeat,
        3 => TerminalKeyAction::Release,
        _ => return None,
    };
    let key = if final_byte == b'u' { unicode_key(code)? } else { legacy_key(code, final_byte)? };
    let alternate = |index| -> Option<Option<u32>> {
        match codes.get(index) {
            None | Some(&"") => Some(None),
            Some(s) => Some(Some(number(s)?)),
        }
    };
    let shifted = alternate(1)?;
    if let Some(c) = shifted {
        char::from_u32(c)?;
    }
    let base = alternate(2)?;
    if let Some(c) = base {
        char::from_u32(c)?;
    }
    // A base-layout alternate is evidence of a printable physical location;
    // never infer one from the logical character when the report omits it.
    let physical_key = base.and_then(physical_from_base);
    let generated_text = if let Some(text) = fields.get(2).filter(|text| !text.is_empty()) {
        let mut out = String::new();
        for value in text.split(':') {
            out.push(char::from_u32(number(value)?)?);
        }
        Some(out)
    } else if let TerminalKey::UnicodeScalar(c) = key {
        if action != TerminalKeyAction::Release && c != 0 && !modifiers.intersects(Mods::CTRL | Mods::ALT | Mods::SUPER) {
            let c = if modifiers.contains(Mods::SHIFT) {
                shifted.unwrap_or_else(|| if (b'a' as u32..=b'z' as u32).contains(&c) { c - 32 } else { c })
            } else {
                c
            };
            Some(char::from_u32(c)?.to_string())
        } else {
            None
        }
    } else {
        None
    };
    if code == 0 && generated_text.is_none() {
        return None;
    }
    let no_release = flags & 2 == 0 || (flags & 8 == 0 && matches!(key, TerminalKey::Named(Named::Enter | Named::Tab | Named::Backspace)));
    let tap = code == 0 || (no_release && mods_event.len() == 1);
    Some(Report::Key {
        event: TerminalKeyEvent {
            key,
            action,
            modifiers,
            consumed_modifiers: Mods::empty(),
            generated_text,
            physical_key,
            platform_keycode: 0,
        },
        tap,
    })
}

fn legacy_key(code: u32, final_byte: u8) -> Option<TerminalKey> {
    let named = if final_byte == b'~' {
        match code {
            2 => Named::Insert,
            3 => Named::Delete,
            5 => Named::PageUp,
            6 => Named::PageDown,
            7 => Named::Home,
            8 => Named::End,
            11..=15 => Named::Function((code - 10) as u8),
            17..=21 => Named::Function((code - 11) as u8),
            23..=26 => Named::Function((code - 12) as u8),
            28..=29 => Named::Function((code - 13) as u8),
            31..=34 => Named::Function((code - 14) as u8),
            _ => return None,
        }
    } else {
        if code != 1 {
            return None;
        }
        match final_byte {
            b'A' => Named::ArrowUp,
            b'B' => Named::ArrowDown,
            b'C' => Named::ArrowRight,
            b'D' => Named::ArrowLeft,
            b'H' => Named::Home,
            b'F' => Named::End,
            b'P' => Named::Function(1),
            b'Q' => Named::Function(2),
            b'S' => Named::Function(4),
            _ => return None,
        }
    };
    Some(TerminalKey::Named(named))
}
fn unicode_key(code: u32) -> Option<TerminalKey> {
    let named = match code {
        8 | 127 => Some(Named::Backspace),
        9 => Some(Named::Tab),
        13 => Some(Named::Enter),
        27 => Some(Named::Escape),
        57344 => Some(Named::Escape),
        57345 => Some(Named::Enter),
        57346 => Some(Named::Tab),
        57347 => Some(Named::Backspace),
        57348 => Some(Named::Insert),
        57349 => Some(Named::Delete),
        57350 => Some(Named::ArrowLeft),
        57351 => Some(Named::ArrowRight),
        57352 => Some(Named::ArrowUp),
        57353 => Some(Named::ArrowDown),
        57354 => Some(Named::PageUp),
        57355 => Some(Named::PageDown),
        57356 => Some(Named::Home),
        57357 => Some(Named::End),
        57364..=57388 => Some(Named::Function((code - 57363) as u8)),
        _ => None,
    };
    if let Some(named) = named {
        return Some(TerminalKey::Named(named));
    }
    let functional = match code {
        57358 => Some("CapsLock"),
        57359 => Some("ScrollLock"),
        57360 => Some("NumLock"),
        57361 => Some("PrintScreen"),
        57362 => Some("Pause"),
        57399 => Some("Numpad0"),
        57400 => Some("Numpad1"),
        57401 => Some("Numpad2"),
        57402 => Some("Numpad3"),
        57403 => Some("Numpad4"),
        57404 => Some("Numpad5"),
        57405 => Some("Numpad6"),
        57406 => Some("Numpad7"),
        57407 => Some("Numpad8"),
        57408 => Some("Numpad9"),
        57409 => Some("NumpadDecimal"),
        57410 => Some("NumpadDivide"),
        57411 => Some("NumpadMultiply"),
        57412 => Some("NumpadSubtract"),
        57413 => Some("NumpadAdd"),
        57414 => Some("NumpadEnter"),
        57415 => Some("NumpadEqual"),
        57416 => Some("NumpadSeparator"),
        57417 => Some("NumpadLeft"),
        57418 => Some("NumpadRight"),
        57419 => Some("NumpadUp"),
        57420 => Some("NumpadDown"),
        57421 => Some("NumpadPageUp"),
        57422 => Some("NumpadPageDown"),
        57423 => Some("NumpadHome"),
        57424 => Some("NumpadEnd"),
        57425 => Some("NumpadInsert"),
        57426 => Some("NumpadDelete"),
        57427 => Some("NumpadBegin"),
        57441 => Some("ShiftLeft"),
        57447 => Some("ShiftRight"),
        57442 => Some("ControlLeft"),
        57448 => Some("ControlRight"),
        57444 => Some("MetaLeft"),
        57450 => Some("MetaRight"),
        57443 => Some("AltLeft"),
        57449 => Some("AltRight"),
        57363 => Some("ContextMenu"),
        _ => None,
    };
    if let Some(name) = functional {
        return Some(TerminalKey::Code(name.into()));
    }
    if (code != 0 && code < 32) || code == 127 || (57344..=63743).contains(&code) {
        return None;
    }
    char::from_u32(code)?;
    Some(TerminalKey::UnicodeScalar(code))
}
fn physical_from_base(code: u32) -> Option<String> {
    let c = char::from_u32(code)?;
    if c.is_ascii_alphabetic() {
        return Some(format!("Key{}", c.to_ascii_uppercase()));
    }
    if c.is_ascii_digit() {
        return Some(format!("Digit{c}"));
    }
    Some(
        match c {
            '`' => "Backquote",
            '-' => "Minus",
            '=' => "Equal",
            '[' => "BracketLeft",
            ']' => "BracketRight",
            '\\' => "Backslash",
            ';' => "Semicolon",
            '\'' => "Quote",
            ',' => "Comma",
            '.' => "Period",
            '/' => "Slash",
            ' ' => "Space",
            _ => return None,
        }
        .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(bytes: &[u8], flags: u32) -> (TerminalKeyEvent, bool) {
        match decode(bytes, flags) {
            Report::Key { event, tap } => (event, tap),
            _ => panic!("not a key"),
        }
    }
    #[test]
    fn fields_preserve_identity_modifiers_actions_and_associated_text() {
        let (event, tap) = key(b"\x1b[122:90:119;194:2;90:769u", FLAGS);
        assert!(!tap);
        assert_eq!(event.key, TerminalKey::UnicodeScalar('z' as u32));
        assert_eq!(event.physical_key.as_deref(), Some("KeyW"));
        assert_eq!(event.generated_text.as_deref(), Some("Z\u{301}"));
        assert_eq!(event.action, TerminalKeyAction::Repeat);
        assert_eq!(event.modifiers, Mods::SHIFT | Mods::CAPS_LOCK | Mods::NUM_LOCK);
        assert!(key(b"\x1b[97u", FLAGS).0.physical_key.is_none());
        assert_eq!(key(b"\x1b[97;7:3u", FLAGS).0.modifiers, Mods::ALT | Mods::CTRL);
    }
    #[test]
    fn functional_forms_and_partial_enhancements() {
        for (bytes, named) in
            [(b"\x1b[1;5A".as_slice(), Named::ArrowUp), (b"\x1b[13;2~", Named::Function(3)), (b"\x1b[57388u", Named::Function(25))]
        {
            assert_eq!(key(bytes, FLAGS).0.key, TerminalKey::Named(named));
        }
        assert_eq!(key(b"\x1b[57441;2u", FLAGS).0.key, TerminalKey::Code("ShiftLeft".into()));
        assert_eq!(key(b"\x1b[57414u", FLAGS).0.key, TerminalKey::Code("NumpadEnter".into()));
        assert!(key(b"\x1b[27u", 1).1);
        assert!(key(b"\x1b[13u", 3).1, "Enter lacks key-up unless report-all is enabled");
        assert!(!key(b"\x1b[13u", FLAGS).1);
        assert!(matches!(decode(b"\x1b[A", 0), Report::NotKey));
        assert!(matches!(decode(b"\x1b[1;2R", FLAGS), Report::NotKey), "cursor query reply is not F3");
    }
    #[test]
    fn invalid_and_unsupported_reports_are_not_raw_application_input() {
        for sequence in [
            b"\x1b[97;0u".as_slice(),
            b"\x1b[97;1:4u",
            b"\x1b[55296u",
            b"\x1b[97;17u",
            b"\x1b[97;1;55296u",
            b"\x1b[97;1;65;66u",
            b"\x1b[57389u",
            b"\x1b[?oopsu",
        ] {
            assert!(matches!(decode(sequence, FLAGS), Report::Unsupported), "{sequence:?}");
        }
    }
    #[test]
    fn mode_pushes_are_owned_balanced_and_cannot_restart_after_cleanup() {
        let mut mode = KeyboardMode::default();
        let mut out = Vec::new();
        mode.enter_screen(&mut out).unwrap();
        assert!(out.is_empty(), "do not enable without a support reply");
        assert!(mode.enable(&mut out).unwrap());
        assert!(!mode.enable(&mut out).unwrap());
        mode.leave_screen(&mut out).unwrap();
        mode.enter_screen(&mut out).unwrap();
        mode.close(&mut out).unwrap();
        mode.close(&mut out).unwrap();
        mode.enter_screen(&mut out).unwrap();
        assert_eq!(out, b"\x1b[>31u\x1b[?u\x1b[<u\x1b[>31u\x1b[?u\x1b[<u");
    }
}
