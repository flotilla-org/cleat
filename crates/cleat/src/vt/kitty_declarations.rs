//! Retain metadata the pinned Ghostty C API does not expose. This observer
//! never interprets payloads or decides whether a graphics command succeeded:
//! only declarations actually present in Ghostty are admitted to the registry.
use super::ghostty_ffi::KittyVirtualDeclaration;
use crate::provider::TerminalVirtualPlacement;

#[derive(Clone, Debug, Default)]
pub(super) struct Command {
    pub image_id: Option<u32>,
    image_number: Option<u32>,
    pub placement_id: Option<u32>,
    action: u8,
    more: bool,
    delete: u8,
}
impl Command {
    fn parse(header: &[u8]) -> Option<Self> {
        let mut command = Self { action: b't', delete: b'a', ..Self::default() };
        // Mirrors rjwittams/ghostty@c361de9, src/terminal/kitty/graphics_command.zig.
        // Recheck these rules when tools/prepare-ghostty-vt.sh changes its pin.
        // Match the pinned Ghostty Parser's 11-byte key/value buffer and
        // ignore states. A one-byte non-digit is its ASCII numeric value;
        // integer parse failures reject the command, but malformed keys are ignored.
        let mut key = Vec::new();
        let mut value: Vec<u8> = Vec::new();
        let mut current = 0;
        let mut medium = u32::from(b'd');
        let mut state = 0; // key, ignored key, value, ignored value
        for byte in header.iter().copied().chain(b";".iter().copied()) {
            match state {
                0 | 1 if byte == b'=' => {
                    current = key.first().copied().unwrap_or(0);
                    state = if state == 0 && key.len() == 1 { 2 } else { 3 };
                    key.clear();
                }
                0 | 1 if byte == b';' => break,
                0 => {
                    key.push(byte);
                    if key.len() > 11 {
                        state = 1;
                        key.clear();
                    }
                }
                2 if matches!(byte, b',' | b';') => {
                    let number = if value.len() == 1 && !value[0].is_ascii_digit() {
                        u32::from(value[0])
                    } else {
                        let text = std::str::from_utf8(&value).ok()?.replace('_', "");
                        if matches!(current, b'z' | b'H' | b'V') {
                            text.parse::<i32>().ok()? as u32
                        } else {
                            text.parse::<u32>().ok()?
                        }
                    };
                    match current {
                        b'a' => command.action = number.try_into().ok()?,
                        b'd' => command.delete = number.try_into().ok()?,
                        b'i' => command.image_id = Some(number),
                        b'I' => command.image_number = Some(number),
                        b'p' => command.placement_id = Some(number),
                        // Ghostty treats every nonzero direct-transmission m as more.
                        b'm' => command.more = number > 0,
                        b't' => medium = number,
                        _ => {}
                    }
                    value.clear();
                    state = 0;
                    if byte == b';' {
                        break;
                    }
                }
                2 => {
                    value.push(byte);
                    if value.len() > 11 {
                        state = 3;
                        value.clear();
                    }
                }
                3 if byte == b',' => state = 1,
                3 if byte == b';' => break,
                _ => {}
            }
        }
        // Local file/shared-memory media ignore m in Ghostty.
        command.more &= medium == u32::from(b'd');
        Some(command)
    }
    fn targets_explicit(&self, entry: &Entry) -> bool {
        (matches!(self.action, b'p' | b'T') || self.action == b'd' && matches!(self.delete, b'i' | b'I' | b'n' | b'N'))
            && (self.image_id == Some(entry.raw.declaration.image_id)
                || self.image_number.is_some_and(|n| n != 0 && n == entry.raw.image_number))
            && self.placement_id.is_some_and(|p| p != 0 && entry.explicit && entry.original_id == p)
    }
}

pub(super) enum Event {
    Command(Option<Command>),
    Sync,
    Reset,
}
#[derive(Default)]
enum State {
    #[default]
    Ground,
    Escape,
    Csi(Vec<u8>),
    String {
        apc: bool,
        osc: bool,
        header: Vec<u8>,
        payload: bool,
    },
}
#[derive(Default)]
pub(super) struct Observer {
    state: State,
    transmission: Option<Command>,
}
impl Observer {
    /// Returns boundaries at which Ghostty must be sampled, even across writes.
    /// OSC/DCS payloads are ignored so embedded APC-looking text is not counted.
    pub fn byte(&mut self, byte: u8) -> Option<Event> {
        // Ghostty dispatches APC on *leaving* the string state: ESC already
        // executes the command before the trailing backslash arrives. CAN/SUB
        // likewise end an APC and dispatch its accumulated control prefix.
        if let State::String { apc, osc, header, .. } = &self.state {
            if matches!(byte, 0x1b | 0x18 | 0x1a | 0x9c) || (*osc && byte == 7) {
                let kitty = *apc && header.first() == Some(&b'G');
                let command = if kitty && header.len() <= 4096 { Command::parse(&header[1..]) } else { None };
                self.state = if byte == 0x1b { State::Escape } else { State::Ground };
                if kitty {
                    let command = match command {
                        Some(command) if matches!(command.action, b't' | b'T') => {
                            let first = self.transmission.take().unwrap_or_else(|| command.clone());
                            if command.more {
                                self.transmission = Some(first.clone());
                            }
                            Some(first)
                        }
                        other => other,
                    };
                    return Some(Event::Command(command));
                }
                return None;
            }
        }
        if matches!(byte, 0x18 | 0x1a) {
            self.state = State::Ground;
            return None;
        }
        match &mut self.state {
            State::Ground => {
                if byte == 0x1b {
                    self.state = State::Escape;
                }
            }
            State::Escape => {
                self.state = match byte {
                    b'[' => State::Csi(Vec::new()),
                    b'_' | b']' | b'P' | b'^' | b'X' => {
                        State::String { apc: matches!(byte, b'_' | b'^' | b'X'), osc: byte == b']', header: Vec::new(), payload: false }
                    }
                    0x1b => State::Escape,
                    _ => State::Ground,
                };
                if byte == b'c' {
                    self.transmission = None;
                    return Some(Event::Reset);
                }
            }
            State::Csi(params) => {
                if byte == 0x1b {
                    self.state = State::Escape;
                } else if (0x40..=0x7e).contains(&byte) {
                    let screen = matches!(byte, b'h' | b'l')
                        && params.first() == Some(&b'?')
                        && params[1..].split(|b| *b == b';').any(|p| matches!(p, b"47" | b"1047" | b"1049"));
                    self.state = State::Ground;
                    if screen {
                        return Some(Event::Sync);
                    }
                } else if params.len() < 128 {
                    params.push(byte);
                }
            }
            State::String { apc, header, payload, .. } => {
                if byte == b';' {
                    *payload = true;
                } else if *apc && !*payload && header.len() < 4097 {
                    header.push(byte);
                }
            }
        }
        None
    }
}

#[derive(Clone)]
struct Entry {
    raw: KittyVirtualDeclaration,
    handle: u64,
    order: u64,
    original_id: u32,
    explicit: bool,
}
#[derive(Clone, Default)]
pub(super) struct Registry {
    screens: [Vec<Entry>; 2],
    sequence: u64,
}
impl Registry {
    pub fn reset(&mut self) {
        self.screens = Default::default();
    }

    pub fn reconcile(
        &mut self,
        screen: usize,
        raw: Vec<KittyVirtualDeclaration>,
        command: Option<&Command>,
    ) -> Result<Vec<TerminalVirtualPlacement>, String> {
        let entries = &mut self.screens[screen];
        // When namespace-colliding entries have identical geometry, an explicit
        // ID update/deletion must consume the explicit entry, never its unnamed twin.
        entries.sort_by_key(|entry| (command.is_some_and(|c| c.targets_explicit(entry)), entry.order));
        let mut previous = entries.clone();
        let mut next = Vec::with_capacity(raw.len());
        for fresh in raw {
            // Image replacement can preserve a declaration. Its handle stays
            // live while the image generation follows Ghostty's new resource.
            let found = previous.iter().position(|old| same_declaration(&old.raw, &fresh));
            let entry = if let Some(index) = found {
                let mut old = previous.remove(index);
                old.raw = fresh;
                old
            } else {
                let command = command.ok_or("virtual declaration appeared without observed Kitty control metadata")?;
                self.sequence = self.sequence.checked_add(1).ok_or("virtual declaration sequence exhausted")?;
                Entry {
                    raw: fresh,
                    handle: self.sequence,
                    order: self.sequence,
                    original_id: command.placement_id.unwrap_or(0),
                    explicit: command.placement_id.is_some(),
                }
            };
            next.push(entry);
        }
        next.sort_by_key(|entry| entry.order);
        *entries = next;
        Ok(entries
            .iter()
            .map(|entry| {
                let mut declaration = entry.raw.declaration.clone();
                declaration.handle = entry.handle;
                declaration.creation_order = entry.order;
                declaration.placement_id = entry.original_id;
                declaration.placement_id_explicit = entry.explicit;
                declaration
            })
            .collect())
    }
}
fn same_declaration(a: &KittyVirtualDeclaration, b: &KittyVirtualDeclaration) -> bool {
    let mut a = a.clone();
    a.declaration.generation = b.declaration.generation;
    a.image_number = b.image_number;
    a == *b
}

#[cfg(test)]
mod tests {
    use crate::{
        provider::DirtyState,
        vt::{ghostty::GhosttyVtEngine, VtEngine},
    };

    const IMAGE: &[u8] = b"\x1b_Ga=t,i=7,f=32,s=1,v=1;ESIz/w==\x1b\\";
    const OMITTED: &[u8] = b"\x1b_Ga=p,i=7,U=1,c=4,r=2;\x1b\\";
    fn engine() -> GhosttyVtEngine {
        let mut engine = GhosttyVtEngine::new(10, 4);
        engine.set_cell_size(10, 20).unwrap();
        engine.feed(IMAGE).unwrap();
        engine
    }
    fn placeholders() -> Vec<u8> {
        let mut text = String::from("\x1b[38;2;0;0;7m");
        let cols = ['\u{0305}', '\u{030d}', '\u{030e}', '\u{0310}'];
        for row in ['\u{0305}', '\u{030d}'] {
            for col in cols {
                text.push_str(&format!("\u{10eeee}{row}{col}"));
            }
            text.push_str("\x1b[2;1H");
        }
        text.push_str("\x1b[0m");
        text.into_bytes()
    }

    // #317: one original 4x2 declaration exists before cells and accompanies
    // two resolved strips afterward. Deletion removes both atomically.
    #[test]
    fn declaration_precedes_cells_and_survives_as_one_four_by_two() {
        let mut e = engine();
        e.feed(OMITTED).unwrap();
        let before = e.render_update(DirtyState::Full).unwrap();
        assert_eq!(before.virtual_placements.len(), 1);
        let declaration = &before.virtual_placements[0];
        assert_eq!((declaration.columns, declaration.rows), (4, 2));
        assert_eq!((declaration.placement_id, declaration.placement_id_explicit), (0, false));
        assert_eq!(before.image_resources.len(), 1);
        assert!(before.image_placements.is_empty());
        e.feed(&placeholders()).unwrap();
        let drawn = e.render_update(DirtyState::Full).unwrap();
        assert_eq!(drawn.virtual_placements, before.virtual_placements);
        assert_eq!(drawn.image_placements.len(), 2);
        for strip in &drawn.image_placements {
            assert_eq!((strip.grid_cols, strip.grid_rows), (4, 1));
            assert_eq!(strip.generation, declaration.generation);
        }
        e.feed(b"\x1b_Ga=d,d=i,i=7;\x1b\\").unwrap();
        let deleted = e.render_update(DirtyState::Full).unwrap();
        assert!(deleted.virtual_placements.is_empty());
        assert!(deleted.image_placements.is_empty());
        assert!(deleted.image_resources.is_empty());
    }

    // Governor ruling: raw Ghostty ID collisions must never expose internal
    // IDs as explicit p. Preserve creation order and original omission instead.
    #[test]
    fn namespace_collision_probe_preserves_original_ids_and_order() {
        for explicit_first in [false, true] {
            let mut e = engine();
            e.feed(b"\x1b_Ga=p,i=7,U=1,c=1,r=1;\x1b\\").unwrap();
            let omitted = if explicit_first {
                b"\x1b_Ga=p,i=7,U=1,c=2,r=1;\x1b\\".as_slice()
            } else {
                b"\x1b_Ga=p,i=7,U=1,c=4,r=2;\x1b\\".as_slice()
            };
            let explicit = if explicit_first {
                b"\x1b_Ga=p,i=7,p=1,U=1,c=4,r=2;\x1b\\".as_slice()
            } else {
                b"\x1b_Ga=p,i=7,p=1,U=1,c=2,r=1;\x1b\\".as_slice()
            };
            for command in if explicit_first { [explicit, omitted] } else { [omitted, explicit] } {
                e.feed(command).unwrap();
            }
            let declarations = e.virtual_placements();
            assert_eq!(declarations.len(), 3);
            assert_eq!((declarations[0].placement_id, declarations[0].placement_id_explicit), (0, false));
            let first = if explicit_first { (1, true, 4, 2) } else { (0, false, 4, 2) };
            let second = if explicit_first { (0, false, 2, 1) } else { (1, true, 2, 1) };
            assert_eq!(
                (declarations[1].placement_id, declarations[1].placement_id_explicit, declarations[1].columns, declarations[1].rows),
                first
            );
            assert_eq!(
                (declarations[2].placement_id, declarations[2].placement_id_explicit, declarations[2].columns, declarations[2].rows),
                second
            );
            for pair in declarations.windows(2) {
                assert!(pair[0].creation_order < pair[1].creation_order);
                assert_ne!(pair[0].handle, pair[1].handle);
            }
            // Unrelated text and observations must not change opaque handles.
            e.feed(b"text").unwrap();
            assert_eq!(e.virtual_placements(), declarations);
        }
    }

    // #317 lifecycle: updates, same-ID pixel replacement and selective deletion
    // keep generations current, while identical internal/explicit twins remain distinct.
    #[test]
    fn replacement_and_deletion_keep_the_live_declaration_set_consistent() {
        let mut e = engine();
        for command in [
            b"\x1b_Ga=p,i=7,U=1,c=1,r=1;\x1b\\".as_slice(),
            b"\x1b_Ga=p,i=7,p=0,U=1,c=4,r=2;\x1b\\",
            b"\x1b_Ga=p,i=7,p=1,U=1,c=4,r=2;\x1b\\",
        ] {
            e.feed(command).unwrap();
        }
        let before = e.virtual_placements();
        assert_eq!((before[1].placement_id, before[1].placement_id_explicit), (0, true));
        e.feed(b"\x1b_Ga=d,d=i,i=7,p=1;\x1b\\").unwrap();
        let after = e.virtual_placements();
        assert_eq!(after, before[..2]);
        // Same explicit key replacement exposes the new declaration geometry.
        e.feed(b"\x1b_Ga=p,i=7,p=1,U=1,c=3,r=3,z=-5,x=1,y=2,w=3,h=4,X=1,Y=2;\x1b\\").unwrap();
        let updated = e.virtual_placements();
        let p = updated.last().unwrap();
        assert_eq!((p.columns, p.rows, p.z), (3, 3, -5));
        assert_eq!((p.source_x, p.source_y, p.source_width, p.source_height), (1, 2, 3, 4));
        assert_eq!((p.x_offset_px, p.y_offset_px), (1, 2));
        assert!(p.creation_order > before[2].creation_order);
        e.feed(b"\x1b_Ga=t,i=7,f=32,s=1,v=1;RFVm/w==\x1b\\").unwrap();
        let replaced = e.render_update(DirtyState::Full).unwrap();
        for declaration in &replaced.virtual_placements {
            assert_ne!(declaration.generation, before[0].generation);
            assert!(replaced.image_resources.iter().any(|r| r.image_id == declaration.image_id && r.generation == declaration.generation));
        }
        e.feed(b"\x1b_Ga=d,d=I,i=7;\x1b\\").unwrap();
        assert!(e.virtual_placements().is_empty());
        e.feed(IMAGE).unwrap();
        e.feed(OMITTED).unwrap();
        assert!(e.virtual_placements()[0].creation_order > p.creation_order);
    }

    // Geometry updates must not relabel an unchanged omitted declaration when
    // its internal ID collides with an older explicit ID of identical geometry.
    #[test]
    fn replacing_explicit_twins_preserves_omission_in_either_creation_order() {
        for explicit_first in [false, true] {
            let mut e = engine();
            e.feed(b"\x1b_Ga=p,i=7,U=1,c=1,r=1;\x1b\\").unwrap();
            let omitted = b"\x1b_Ga=p,i=7,U=1,c=4,r=2;\x1b\\".as_slice();
            let explicit = b"\x1b_Ga=p,i=7,p=1,U=1,c=4,r=2;\x1b\\".as_slice();
            for command in if explicit_first { [explicit, omitted] } else { [omitted, explicit] } {
                e.feed(command).unwrap();
            }
            let original = e.virtual_placements();
            let unnamed = original.iter().find(|p| !p.placement_id_explicit && p.columns == 4).unwrap().clone();
            e.feed(b"\x1b_Ga=p,i=7,p=1,U=1,c=3,r=2;\x1b\\").unwrap();
            let next = e.virtual_placements();
            assert!(next.contains(&unnamed), "explicit_first={explicit_first}");
            let updated = next.iter().find(|p| p.placement_id_explicit).unwrap();
            assert_eq!((updated.placement_id, updated.columns), (1, 3));
            e.feed(b"\x1b_Ga=d,d=i,i=7,p=1;\x1b\\").unwrap();
            assert!(e.virtual_placements().contains(&unnamed));
            assert!(e.virtual_placements().iter().all(|p| !p.placement_id_explicit));
        }
    }

    // Boundary generator crosses automatic zero geometry, normal dimensions,
    // and u32 maxima. Declared geometry must not become resolved pixel sizes.
    #[test]
    fn declared_geometry_preserves_zero_and_numeric_boundaries() {
        for size in [0, 1, 4, u32::MAX] {
            for z in [i32::MIN, -1, 0, i32::MAX] {
                let mut e = engine();
                e.feed(format!("\x1b_Ga=p,i=7,p={},U=1,c={size},r={size},z={z};\x1b\\", u32::MAX).as_bytes()).unwrap();
                let ps = e.virtual_placements();
                assert_eq!(ps.len(), 1);
                assert_eq!((ps[0].columns, ps[0].rows, ps[0].z), (size, size, z));
                assert_eq!((ps[0].placement_id, ps[0].placement_id_explicit), (u32::MAX, true));
            }
        }
    }

    // Split-position generator spans every byte boundary through controls,
    // headers, ST, payload and placeholder UTF-8. Observation mid-write must
    // not lose metadata. Both omitted p and explicit zero remain distinguishable.
    #[test]
    fn every_write_split_preserves_declaration_metadata() {
        let mut stream = IMAGE.to_vec();
        stream.extend_from_slice(OMITTED);
        stream.extend_from_slice(b"\x1b_Ga=p,i=7,p=0,U=1,c=2,r=1;\x1b\\");
        stream.extend(placeholders());
        let mut reference = GhosttyVtEngine::new(10, 4);
        reference.feed(&stream).unwrap();
        let expected = reference.virtual_placements();
        for split in 0..=stream.len() {
            let mut e = GhosttyVtEngine::new(10, 4);
            e.feed(&stream[..split]).unwrap();
            e.render_update(DirtyState::Full).unwrap();
            e.feed(&stream[split..]).unwrap();
            let actual = e.virtual_placements();
            // Image generation is process-global; compare stable session metadata.
            let normalize = |mut ps: Vec<crate::provider::TerminalVirtualPlacement>| {
                for p in &mut ps {
                    p.generation = 0;
                }
                ps
            };
            assert_eq!(normalize(actual), normalize(expected.clone()), "split={split}");
        }
    }

    // Ghostty accepts ASCII-valued IDs and ignored multi-byte keys. Metadata
    // observation must preserve original p without turning accepted output into an error.
    #[test]
    fn ghostty_accepted_headers_preserve_metadata() {
        for header in ["a=p,i=7,p=A,U=1,c=4,r=2", "a=p,i=7,p=+1,U=1,c=4,r=2", "a=p,i=7,p=1,U=1,c=4,r=2,ignored=value"] {
            let mut e = engine();
            e.feed(format!("\x1b_G{header};\x1b\\").as_bytes()).unwrap();
            let declarations = e.virtual_placements();
            assert_eq!(declarations.len(), 1);
            assert!(declarations[0].placement_id_explicit);
            assert_eq!(declarations[0].placement_id, if header.contains("p=A") { 65 } else { 1 });
        }
        let mut e = engine();
        e.feed(b"\x1b_Ga=T,i=8,p=9,U=1,c=4,r=2,f=32,s=1,v=1,m=2;ESIz\x1b\\").unwrap();
        e.feed(b"\x1b_Gm=0;/w==\x1b\\").unwrap();
        assert_eq!(e.virtual_placements()[0].placement_id, 9);
    }

    // Chunked transmit+display inherits p/U/geometry from the first chunk.
    // OSC/DCS payloads and cancelled APCs cannot invent declarations; RIS
    // and alternate-screen clearing end lifetimes without reusing handles.
    #[test]
    fn chunks_strings_resets_and_screens_preserve_lifetimes() {
        let mut e = engine();
        e.feed(b"\x1b_Ga=T,i=8,p=0,U=1,c=4,r=2,f=32,s=1,v=1,m=1;ESIz\x1b\\").unwrap();
        assert!(e.virtual_placements().is_empty());
        e.feed(b"\x1b_Gm=0;/w==\x1b\\").unwrap();
        let first = e.virtual_placements();
        assert_eq!(first.len(), 1);
        assert_eq!((first[0].image_id, first[0].placement_id, first[0].placement_id_explicit), (8, 0, true));
        e.feed(b"\x1b]0;_Ga=p,i=8,U=1,c=9,r=9;\x07\x1bP_Ga=p,i=8,U=1,c=9,r=9;\x1b\\").unwrap();
        assert_eq!(e.virtual_placements(), first);
        // An invalid unfinished header creates nothing when CAN dispatches it.
        e.feed(b"\x1b_Ga=p,i=8,U=1,c=;\x18").unwrap();
        assert_eq!(e.virtual_placements(), first);
        e.feed(b"\x1b[?1049h").unwrap();
        assert!(e.virtual_placements().is_empty());
        e.feed(b"\x1b[?1049l").unwrap();
        assert_eq!(e.virtual_placements(), first);
        e.feed(b"\x1bc").unwrap();
        assert!(e.virtual_placements().is_empty());
        e.feed(IMAGE).unwrap();
        e.feed(OMITTED).unwrap();
        assert!(e.virtual_placements()[0].handle > first[0].handle);
    }
}
