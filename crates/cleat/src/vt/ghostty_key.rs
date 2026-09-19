//! Key encoder bindings for the pinned libghostty-vt prefix.
use std::{ffi::c_void, ptr};

use super::ghostty_ffi::*;
use crate::provider::{TerminalKey, TerminalKeyAction, TerminalKeyEvent};

type Handle = *mut c_void;
unsafe extern "C" {
    fn ghostty_key_encoder_new(allocator: *const GhosttyAllocator, out: *mut Handle) -> GhosttyResult;
    fn ghostty_key_encoder_free(encoder: Handle);
    fn ghostty_key_event_new(allocator: *const GhosttyAllocator, out: *mut Handle) -> GhosttyResult;
    fn ghostty_key_event_free(event: Handle);
    fn ghostty_key_encoder_setopt_from_terminal(encoder: Handle, terminal: GhosttyTerminal);
    fn ghostty_key_encoder_setopt(encoder: Handle, option: u32, value: *const c_void);
    fn ghostty_key_event_set_action(event: Handle, action: u32);
    fn ghostty_key_event_set_key(event: Handle, key: u32);
    fn ghostty_key_event_set_mods(event: Handle, mods: u16);
    fn ghostty_key_event_set_consumed_mods(event: Handle, mods: u16);
    fn ghostty_key_event_set_utf8(event: Handle, text: *const u8, len: usize);
    fn ghostty_key_event_set_unshifted_codepoint(event: Handle, codepoint: u32);
    fn ghostty_key_encoder_encode(encoder: Handle, event: Handle, out: *mut u8, len: usize, written: *mut usize) -> GhosttyResult;
}

pub(super) struct KeyEncoder {
    encoder: Handle,
    event: Handle,
}
// Confined to the session actor, like TerminalHandle and MouseEncoder.
unsafe impl Send for KeyEncoder {}
impl KeyEncoder {
    pub fn new() -> Result<Self, String> {
        let mut encoder = ptr::null_mut();
        let mut event = ptr::null_mut();
        if unsafe { ghostty_key_encoder_new(ptr::null(), &mut encoder) } != GhosttyResult::Success {
            return Err("create Ghostty key encoder".into());
        }
        if unsafe { ghostty_key_event_new(ptr::null(), &mut event) } != GhosttyResult::Success {
            unsafe { ghostty_key_encoder_free(encoder) };
            return Err("create Ghostty key event".into());
        }
        Ok(Self { encoder, event })
    }
    pub fn encode(&mut self, terminal: GhosttyTerminal, input: &TerminalKeyEvent) -> Result<Vec<u8>, String> {
        if let TerminalKey::UnicodeScalar(c) = input.key {
            if char::from_u32(c).is_none() {
                return Err("invalid key Unicode scalar".into());
            }
        }
        let logical = crate::keyboard::functional_name(&input.key);
        let physical = input.physical_key.as_deref().map(key_code).transpose()?.unwrap_or(0);
        // Ghostty has one key slot, plus an unshifted Unicode scalar. A
        // remapped functional location must not override the logical key.
        // Printable locations can supply Kitty's base-layout alternate value.
        let key = if let Some(name) = logical.as_deref() {
            key_code(name)?
        } else if matches!(physical, 0..=50 | 63) {
            // unidentified, writing-system keys, Space
            physical
        } else {
            0
        };
        let codepoint = match input.key {
            TerminalKey::UnicodeScalar(c) => c,
            _ => 0,
        };
        let fallback = char::from_u32(codepoint).filter(|_| codepoint != 0).map(|c| c.to_string());
        let text = input.generated_text.as_deref().or(fallback.as_deref()).unwrap_or("");
        // Native APIs sometimes supply already-transformed Ctrl text or macOS
        // function-key PUA. Ghostty must derive these from identity/modifiers.
        let text = if text.chars().any(|c| c.is_control() || ('\u{f700}'..='\u{f8ff}').contains(&c)) { "" } else { text };
        unsafe {
            ghostty_key_encoder_setopt_from_terminal(self.encoder, terminal);
            // The source has already decided which Alt modifiers were consumed
            // by text production. Encode the remaining Alt as a terminal modifier.
            let option_as_alt: u32 = 1;
            ghostty_key_encoder_setopt(self.encoder, 6, (&option_as_alt as *const u32).cast());
            ghostty_key_event_set_action(self.event, match input.action {
                TerminalKeyAction::Release => 0,
                TerminalKeyAction::Press => 1,
                TerminalKeyAction::Repeat => 2,
            });
            ghostty_key_event_set_key(self.event, key);
            ghostty_key_event_set_mods(self.event, input.modifiers.bits());
            ghostty_key_event_set_consumed_mods(self.event, input.consumed_modifiers.bits());
            ghostty_key_event_set_unshifted_codepoint(self.event, codepoint);
            ghostty_key_event_set_utf8(self.event, text.as_ptr(), text.len());
        }
        let mut bytes = vec![0; 128];
        let mut written = 0;
        let mut result = unsafe { ghostty_key_encoder_encode(self.encoder, self.event, bytes.as_mut_ptr(), bytes.len(), &mut written) };
        if result == GhosttyResult::OutOfSpace {
            bytes.resize(written, 0);
            result = unsafe { ghostty_key_encoder_encode(self.encoder, self.event, bytes.as_mut_ptr(), bytes.len(), &mut written) };
        }
        // Do not retain a borrowed caller pointer between events.
        unsafe { ghostty_key_event_set_utf8(self.event, ptr::null(), 0) };
        if result != GhosttyResult::Success {
            return Err(format!("encode Ghostty key: {result:?}"));
        }
        bytes.truncate(written);
        Ok(bytes)
    }
}
impl Drop for KeyEncoder {
    fn drop(&mut self) {
        unsafe {
            ghostty_key_event_free(self.event);
            ghostty_key_encoder_free(self.encoder);
        }
    }
}

// Numeric values mirror GhosttyKey in the pinned vt/key/event.h. Only W3C
// names cross the cleat API/wire boundary; Ghostty numbers stay private here.
fn key_code(name: &str) -> Result<u32, String> {
    match name {
        "Unidentified" => Ok(0),
        "Backquote" => Ok(1),
        "Backslash" => Ok(2),
        "BracketLeft" => Ok(3),
        "BracketRight" => Ok(4),
        "Comma" => Ok(5),
        "Digit0" => Ok(6),
        "Digit1" => Ok(7),
        "Digit2" => Ok(8),
        "Digit3" => Ok(9),
        "Digit4" => Ok(10),
        "Digit5" => Ok(11),
        "Digit6" => Ok(12),
        "Digit7" => Ok(13),
        "Digit8" => Ok(14),
        "Digit9" => Ok(15),
        "Equal" => Ok(16),
        "IntlBackslash" => Ok(17),
        "IntlRo" => Ok(18),
        "IntlYen" => Ok(19),
        "KeyA" => Ok(20),
        "KeyB" => Ok(21),
        "KeyC" => Ok(22),
        "KeyD" => Ok(23),
        "KeyE" => Ok(24),
        "KeyF" => Ok(25),
        "KeyG" => Ok(26),
        "KeyH" => Ok(27),
        "KeyI" => Ok(28),
        "KeyJ" => Ok(29),
        "KeyK" => Ok(30),
        "KeyL" => Ok(31),
        "KeyM" => Ok(32),
        "KeyN" => Ok(33),
        "KeyO" => Ok(34),
        "KeyP" => Ok(35),
        "KeyQ" => Ok(36),
        "KeyR" => Ok(37),
        "KeyS" => Ok(38),
        "KeyT" => Ok(39),
        "KeyU" => Ok(40),
        "KeyV" => Ok(41),
        "KeyW" => Ok(42),
        "KeyX" => Ok(43),
        "KeyY" => Ok(44),
        "KeyZ" => Ok(45),
        "Minus" => Ok(46),
        "Period" => Ok(47),
        "Quote" => Ok(48),
        "Semicolon" => Ok(49),
        "Slash" => Ok(50),
        "AltLeft" => Ok(51),
        "AltRight" => Ok(52),
        "Backspace" => Ok(53),
        "CapsLock" => Ok(54),
        "ContextMenu" => Ok(55),
        "ControlLeft" => Ok(56),
        "ControlRight" => Ok(57),
        "Enter" => Ok(58),
        "MetaLeft" => Ok(59),
        "MetaRight" => Ok(60),
        "ShiftLeft" => Ok(61),
        "ShiftRight" => Ok(62),
        "Space" => Ok(63),
        "Tab" => Ok(64),
        "Convert" => Ok(65),
        "KanaMode" => Ok(66),
        "NonConvert" => Ok(67),
        "Delete" => Ok(68),
        "End" => Ok(69),
        "Help" => Ok(70),
        "Home" => Ok(71),
        "Insert" => Ok(72),
        "PageDown" => Ok(73),
        "PageUp" => Ok(74),
        "ArrowDown" => Ok(75),
        "ArrowLeft" => Ok(76),
        "ArrowRight" => Ok(77),
        "ArrowUp" => Ok(78),
        "NumLock" => Ok(79),
        "Numpad0" => Ok(80),
        "Numpad1" => Ok(81),
        "Numpad2" => Ok(82),
        "Numpad3" => Ok(83),
        "Numpad4" => Ok(84),
        "Numpad5" => Ok(85),
        "Numpad6" => Ok(86),
        "Numpad7" => Ok(87),
        "Numpad8" => Ok(88),
        "Numpad9" => Ok(89),
        "NumpadAdd" => Ok(90),
        "NumpadBackspace" => Ok(91),
        "NumpadClear" => Ok(92),
        "NumpadClearEntry" => Ok(93),
        "NumpadComma" => Ok(94),
        "NumpadDecimal" => Ok(95),
        "NumpadDivide" => Ok(96),
        "NumpadEnter" => Ok(97),
        "NumpadEqual" => Ok(98),
        "NumpadMemoryAdd" => Ok(99),
        "NumpadMemoryClear" => Ok(100),
        "NumpadMemoryRecall" => Ok(101),
        "NumpadMemoryStore" => Ok(102),
        "NumpadMemorySubtract" => Ok(103),
        "NumpadMultiply" => Ok(104),
        "NumpadParenLeft" => Ok(105),
        "NumpadParenRight" => Ok(106),
        "NumpadSubtract" => Ok(107),
        "NumpadSeparator" => Ok(108),
        "NumpadUp" => Ok(109),
        "NumpadDown" => Ok(110),
        "NumpadRight" => Ok(111),
        "NumpadLeft" => Ok(112),
        "NumpadBegin" => Ok(113),
        "NumpadHome" => Ok(114),
        "NumpadEnd" => Ok(115),
        "NumpadInsert" => Ok(116),
        "NumpadDelete" => Ok(117),
        "NumpadPageUp" => Ok(118),
        "NumpadPageDown" => Ok(119),
        "Escape" => Ok(120),
        "F1" => Ok(121),
        "F2" => Ok(122),
        "F3" => Ok(123),
        "F4" => Ok(124),
        "F5" => Ok(125),
        "F6" => Ok(126),
        "F7" => Ok(127),
        "F8" => Ok(128),
        "F9" => Ok(129),
        "F10" => Ok(130),
        "F11" => Ok(131),
        "F12" => Ok(132),
        "F13" => Ok(133),
        "F14" => Ok(134),
        "F15" => Ok(135),
        "F16" => Ok(136),
        "F17" => Ok(137),
        "F18" => Ok(138),
        "F19" => Ok(139),
        "F20" => Ok(140),
        "F21" => Ok(141),
        "F22" => Ok(142),
        "F23" => Ok(143),
        "F24" => Ok(144),
        "F25" => Ok(145),
        "Fn" => Ok(146),
        "FnLock" => Ok(147),
        "PrintScreen" => Ok(148),
        "ScrollLock" => Ok(149),
        "Pause" => Ok(150),
        "BrowserBack" => Ok(151),
        "BrowserFavorites" => Ok(152),
        "BrowserForward" => Ok(153),
        "BrowserHome" => Ok(154),
        "BrowserRefresh" => Ok(155),
        "BrowserSearch" => Ok(156),
        "BrowserStop" => Ok(157),
        "Eject" => Ok(158),
        "LaunchApp1" => Ok(159),
        "LaunchApp2" => Ok(160),
        "LaunchMail" => Ok(161),
        "MediaPlayPause" => Ok(162),
        "MediaSelect" => Ok(163),
        "MediaStop" => Ok(164),
        "MediaTrackNext" => Ok(165),
        "MediaTrackPrevious" => Ok(166),
        "Power" => Ok(167),
        "Sleep" => Ok(168),
        "AudioVolumeDown" => Ok(169),
        "AudioVolumeMute" => Ok(170),
        "AudioVolumeUp" => Ok(171),
        "WakeUp" => Ok(172),
        "Copy" => Ok(173),
        "Cut" => Ok(174),
        "Paste" => Ok(175),
        _ => Err(format!("unsupported physical/functional key: {name}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        provider::{TerminalModifiers as Mods, TerminalNamedKey},
        vt::{ghostty::GhosttyVtEngine, VtEngine},
    };
    fn key(key: TerminalKey, action: TerminalKeyAction, modifiers: Mods) -> TerminalKeyEvent {
        TerminalKeyEvent {
            key,
            action,
            modifiers,
            consumed_modifiers: Mods::empty(),
            generated_text: None,
            physical_key: None,
            platform_keycode: 0,
        }
    }
    #[test]
    fn live_modes_choose_legacy_or_kitty_and_preserve_actions() {
        let mut vt = GhosttyVtEngine::new(80, 24);
        let mut event = key(TerminalKey::UnicodeScalar('a' as u32), TerminalKeyAction::Press, Mods::CTRL);
        assert_eq!(vt.encode_key(&event).unwrap(), b"\x01");
        event.action = TerminalKeyAction::Release;
        assert!(vt.encode_key(&event).unwrap().is_empty());
        vt.feed(b"\x1b[>31u").unwrap();
        event.action = TerminalKeyAction::Press;
        assert_eq!(vt.encode_key(&event).unwrap(), b"\x1b[97;5u");
        event.action = TerminalKeyAction::Repeat;
        assert_eq!(vt.encode_key(&event).unwrap(), b"\x1b[97;5:2u");
        event.action = TerminalKeyAction::Release;
        assert_eq!(vt.encode_key(&event).unwrap(), b"\x1b[97;5:3u");
        vt.feed(b"\x1b[<u").unwrap();
        assert!(vt.encode_key(&event).unwrap().is_empty());
    }
    #[test]
    fn application_cursor_mode_and_extended_functional_keys() {
        let mut vt = GhosttyVtEngine::new(80, 24);
        let up = key(TerminalKey::Named(TerminalNamedKey::ArrowUp), TerminalKeyAction::Press, Mods::empty());
        assert_eq!(vt.encode_key(&up).unwrap(), b"\x1b[A");
        vt.feed(b"\x1b[?1h").unwrap();
        assert_eq!(vt.encode_key(&up).unwrap(), b"\x1bOA");
        vt.feed(b"\x1b[>31u").unwrap();
        let shift = key(TerminalKey::Code("ShiftLeft".into()), TerminalKeyAction::Press, Mods::SHIFT);
        assert_eq!(vt.encode_key(&shift).unwrap(), b"\x1b[57441;2u");
    }
}

#[cfg(test)]
mod rich_tests {
    use super::*;
    use crate::{
        provider::TerminalModifiers as Mods,
        vt::{ghostty::GhosttyVtEngine, VtEngine},
    };
    #[test]
    fn remapped_functional_location_does_not_override_logical_key() {
        let mut vt = GhosttyVtEngine::new(80, 24);
        let mut event = TerminalKeyEvent {
            key: TerminalKey::Named(crate::provider::TerminalNamedKey::Escape),
            physical_key: Some("CapsLock".into()),
            generated_text: None,
            action: TerminalKeyAction::Press,
            modifiers: Mods::empty(),
            consumed_modifiers: Mods::empty(),
            platform_keycode: 0,
        };
        assert_eq!(vt.encode_key(&event).unwrap(), b"\x1b");
        vt.feed(b"\x1b[>31u").unwrap();
        assert_eq!(vt.encode_key(&event).unwrap(), b"\x1b[27u");
        event.key = TerminalKey::UnicodeScalar('a' as u32);
        assert_eq!(vt.encode_key(&event).unwrap(), b"\x1b[97;;97u");
    }

    #[test]
    fn physical_location_does_not_replace_logical_character_and_text_is_preserved() {
        let mut vt = GhosttyVtEngine::new(80, 24);
        vt.feed(b"\x1b[>31u").unwrap();
        let mut event = TerminalKeyEvent {
            key: TerminalKey::UnicodeScalar('z' as u32),
            physical_key: Some("KeyW".into()),
            generated_text: Some("Z".into()),
            action: TerminalKeyAction::Press,
            modifiers: Mods::SHIFT,
            consumed_modifiers: Mods::empty(),
            platform_keycode: 42,
        };
        assert_eq!(vt.encode_key(&event).unwrap(), b"\x1b[122:90:119;2;90u");
        event.generated_text = Some("\x1a".into());
        event.modifiers = Mods::CTRL;
        assert_eq!(vt.encode_key(&event).unwrap(), b"\x1b[122::119;5u");
    }
}
