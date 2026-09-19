//! Outer-terminal mouse negotiation. Coordinates leave this module in cell units.
use crate::provider::*;

pub(crate) const QUERY: &[u8] = b"\x1b[16t\x1b[?1016$p";
pub(crate) struct MouseMode {
    pixel_origin: u32,
    supported: bool,
    requested: bool,
    pixels: bool,
    cell: Option<(u16, u16)>,
}
impl Default for MouseMode {
    fn default() -> Self {
        Self { pixel_origin: 1, supported: false, requested: false, pixels: false, cell: None }
    }
}
pub(crate) fn pixel_origin(term: &str, program: &str) -> u32 {
    if matches!(term, "xterm-kitty" | "xterm-ghostty") || matches!(program, "kitty" | "ghostty") {
        0
    } else {
        1
    }
}
pub(crate) enum Reply {
    CellSize(u16, u16),
    EnablePixels,
    Consumed,
}
impl MouseMode {
    pub fn set_pixel_origin(&mut self, origin: u32) {
        self.pixel_origin = origin.min(1);
    }
    pub fn cell_size(&self) -> (u16, u16) {
        self.cell.unwrap_or((1, 1))
    }
    pub fn reply(&mut self, sequence: &[u8]) -> Option<Vec<Reply>> {
        let mut out = Vec::new();
        if let Some(body) = sequence.strip_prefix(b"\x1b[6;").and_then(|s| s.strip_suffix(b"t")) {
            let size = std::str::from_utf8(body)
                .ok()
                .and_then(|s| s.split_once(';'))
                .and_then(|(h, w)| Some((w.parse::<u16>().ok()?, h.parse::<u16>().ok()?)));
            if let Some((w, h)) = size.filter(|(w, h)| *w > 0 && *h > 0) {
                if self.cell != Some((w, h)) {
                    self.cell = Some((w, h));
                    out.push(Reply::CellSize(w, h));
                }
            }
        } else {
            let body = sequence.strip_prefix(b"\x1b[?1016;").and_then(|s| s.strip_suffix(b"$y"))?;
            self.supported = matches!(body, b"1" | b"2" | b"3");
            self.pixels = matches!(body, b"1" | b"3") && self.cell.is_some();
        }
        if self.supported && self.cell.is_some() && !self.requested {
            self.requested = true;
            out.push(Reply::EnablePixels);
        }
        if out.is_empty() {
            out.push(Reply::Consumed);
        }
        Some(out)
    }
    pub fn decode(&self, sequence: &[u8]) -> Option<TerminalMouseEvent> {
        let body = sequence.strip_prefix(b"\x1b[<")?;
        let (&end, params) = body.split_last()?;
        if !matches!(end, b'M' | b'm') {
            return None;
        }
        let mut fields = std::str::from_utf8(params).ok()?.split(';');
        let code: u16 = fields.next()?.parse().ok()?;
        let origin = if self.pixels { self.pixel_origin } else { 1 };
        let coordinate = |field: &str| -> Option<u32> {
            let value = field.parse::<i64>().ok()?.checked_sub(i64::from(origin))?;
            // Kitty can report a release outside the window at negative pixels.
            // Clamp it to the edge so it still ends the attachment's hold.
            u32::try_from(if end == b'm' { value.max(0) } else { value }).ok()
        };
        let x = coordinate(fields.next()?)?;
        let y = coordinate(fields.next()?)?;
        if fields.next().is_some() || code & !255 != 0 {
            return None;
        }
        let wheel = code & 64 != 0;
        if wheel && (end == b'm' || code & (32 | 128) != 0) {
            return None;
        }
        let button = match (code & 128, code & 3) {
            (0, 0) => Some(TerminalMouseButton::Left),
            (0, 1) => Some(TerminalMouseButton::Middle),
            (0, 2) => Some(TerminalMouseButton::Right),
            (128, 0) => Some(TerminalMouseButton::Back),
            (128, 1) => Some(TerminalMouseButton::Forward),
            (0, 3) => None,
            _ => return None,
        };
        let kind = if wheel {
            TerminalMouseEventKind::Wheel
        } else if end == b'm' {
            TerminalMouseEventKind::Release
        } else if code & 32 != 0 {
            TerminalMouseEventKind::Move
        } else {
            TerminalMouseEventKind::Press
        };
        if matches!(kind, TerminalMouseEventKind::Press | TerminalMouseEventKind::Release) && button.is_none() {
            return None;
        }
        let (w, h) = self.cell_size();
        let (x, y) = if self.pixels { (x as f32 / f32::from(w), y as f32 / f32::from(h)) } else { (x as f32 + 0.5, y as f32 + 0.5) };
        if x >= f32::from(u16::MAX) || y >= f32::from(u16::MAX) {
            return None;
        }
        let mut modifiers = TerminalModifiers::empty();
        for (wire, flag) in [(4, TerminalModifiers::SHIFT), (8, TerminalModifiers::ALT), (16, TerminalModifiers::CTRL)] {
            if code & wire != 0 {
                modifiers |= flag;
            }
        }
        Some(TerminalMouseEvent {
            kind,
            button: if wheel { None } else { button },
            buttons: TerminalMouseButtons::empty(),
            modifiers,
            cell_col: x as u16,
            cell_row: y as u16,
            x_px: x,
            y_px: y,
            wheel_delta_x: if wheel && code & 2 != 0 {
                if code & 1 == 0 {
                    1.0
                } else {
                    -1.0
                }
            } else {
                0.0
            },
            wheel_delta_y: if wheel && code & 2 == 0 {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pixels_require_size_support_and_confirmation_and_preserve_subcell_position() {
        for size_first in [true, false] {
            let mut mode = MouseMode::default();
            let queries: [&[u8]; 2] =
                if size_first { [b"\x1b[6;20;10t", b"\x1b[?1016;2$y"] } else { [b"\x1b[?1016;2$y", b"\x1b[6;20;10t"] };
            assert!(!mode.reply(queries[0]).unwrap().iter().any(|r| matches!(r, Reply::EnablePixels)));
            assert!(mode.reply(queries[1]).unwrap().iter().any(|r| matches!(r, Reply::EnablePixels)));
            assert_eq!(mode.decode(b"\x1b[<0;16;26M").unwrap().x_px, 15.5);
            mode.reply(b"\x1b[?1016;1$y").unwrap();
            let event = mode.decode(b"\x1b[<0;16;26M").unwrap();
            assert_eq!((event.cell_col, event.cell_row, event.x_px, event.y_px), (1, 1, 1.5, 1.25));
            mode.reply(b"\x1b[6;40;20t");
            assert_eq!(mode.decode(b"\x1b[<0;16;26M").unwrap().x_px, 0.75);
        }
    }
    #[test]
    fn known_zero_based_terminals_include_the_first_pixel() {
        assert_eq!(pixel_origin("xterm-ghostty", ""), 0);
        assert_eq!(pixel_origin("xterm-kitty", ""), 0);
        assert_eq!(pixel_origin("xterm-256color", "ghostty"), 0);
        assert_eq!(pixel_origin("xterm-256color", ""), 1);
        let mut mode = MouseMode::default();
        mode.set_pixel_origin(0);
        mode.reply(b"\x1b[6;20;10t");
        mode.reply(b"\x1b[?1016;1$y");
        let first = mode.decode(b"\x1b[<0;0;0M").unwrap();
        assert_eq!((first.x_px, first.y_px), (0.0, 0.0));
        let outside = mode.decode(b"\x1b[<0;-10;-20m").unwrap();
        assert_eq!((outside.x_px, outside.y_px), (0.0, 0.0));
        let next = mode.decode(b"\x1b[<0;10;20M").unwrap();
        assert_eq!((next.cell_col, next.cell_row), (1, 1));
    }
    #[test]
    fn fallback_wheels_extended_buttons_and_invalid_reports() {
        let mut mode = MouseMode::default();
        mode.reply(b"\x1b[?1016;0$y");
        mode.reply(b"\x1b[6;20;10t");
        let horizontal = mode.decode(b"\x1b[<66;2;3M").unwrap();
        assert_eq!((horizontal.wheel_delta_x, horizontal.wheel_delta_y), (1.0, 0.0));
        assert_eq!(mode.decode(b"\x1b[<128;2;3M").unwrap().button, Some(TerminalMouseButton::Back));
        assert_eq!(mode.decode(b"\x1b[<0;2;3M").unwrap().x_px, 1.5);
        for invalid in [b"\x1b[<0;0;2M".as_slice(), b"\x1b[<256;1;1M", b"\x1b[<0;1;1;2M", b"\x1b[<64;1;1m"] {
            assert!(mode.decode(invalid).is_none());
        }
    }
}
