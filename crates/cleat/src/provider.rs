use crate::vt::{self, ScreenGrid};

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct ProviderFeatures: u32 {
        const CELL_SNAPSHOTS = 1 << 0;
        const DAMAGE_ROWS = 1 << 1;
        const STRUCTURED_MOUSE_INPUT = 1 << 2;
        const IMAGE_STATE = 1 << 3;
        const REMOTE_TARGETS = 1 << 4;
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DirtyState {
    #[default]
    Clean,
    Partial,
    Full,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TerminalSnapshot {
    pub cols: u16,
    pub rows: u16,
    pub geometry: TerminalGeometry,
    pub viewport_kind: TerminalViewportKind,
    pub scrollback_offset_rows: u64,
    pub render_generation: u64,
    pub cells: Vec<TerminalCell>,
    pub cursor: TerminalCursor,
    pub dirty: DirtyState,
    pub dirty_rows: Vec<u16>,
}

impl TerminalSnapshot {
    pub fn from_screen_grid(grid: ScreenGrid, dirty: DirtyState) -> Self {
        Self {
            cols: grid.cols,
            rows: grid.rows,
            geometry: TerminalGeometry::default(),
            viewport_kind: TerminalViewportKind::LiveNormal,
            scrollback_offset_rows: 0,
            render_generation: 0,
            cells: grid.cells.into_iter().map(TerminalCell::from_resolved_cell).collect(),
            cursor: TerminalCursor::from_cursor_state(grid.cursor),
            dirty,
            dirty_rows: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TerminalViewportKind {
    #[default]
    LiveNormal,
    LiveAlternate,
    NormalScrollback,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TerminalScrollbackExtent {
    pub normal_scrollback_rows: u64,
    pub live_rows: u16,
    pub alternate_screen: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TerminalGeometry {
    pub cell_width_px: f32,
    pub cell_height_px: f32,
    pub content_x_px: f32,
    pub content_y_px: f32,
    pub content_width_px: f32,
    pub content_height_px: f32,
}

impl TerminalGeometry {
    pub fn from_cell_size(cols: u16, rows: u16, cell_width_px: f32, cell_height_px: f32) -> Self {
        let cell_width_px = positive_finite_or_zero(cell_width_px);
        let cell_height_px = positive_finite_or_zero(cell_height_px);
        Self {
            cell_width_px,
            cell_height_px,
            content_x_px: 0.0,
            content_y_px: 0.0,
            content_width_px: cols as f32 * cell_width_px,
            content_height_px: rows as f32 * cell_height_px,
        }
    }

    pub fn sanitized(self) -> Self {
        Self {
            cell_width_px: positive_finite_or_zero(self.cell_width_px),
            cell_height_px: positive_finite_or_zero(self.cell_height_px),
            content_x_px: finite_or_zero(self.content_x_px),
            content_y_px: finite_or_zero(self.content_y_px),
            content_width_px: positive_finite_or_zero(self.content_width_px),
            content_height_px: positive_finite_or_zero(self.content_height_px),
        }
    }
}

fn positive_finite_or_zero(value: f32) -> f32 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        0.0
    }
}

fn finite_or_zero(value: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        0.0
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TerminalCell {
    pub graphemes: Vec<u32>,
    pub fg: TerminalRgb,
    pub bg: TerminalRgb,
    pub flags: TerminalCellFlags,
    pub width: TerminalCellWidth,
}

impl TerminalCell {
    fn from_resolved_cell(cell: vt::ResolvedCell) -> Self {
        Self {
            graphemes: cell.graphemes,
            fg: TerminalRgb::from_rgb(cell.fg),
            bg: TerminalRgb::from_rgb(cell.bg),
            flags: TerminalCellFlags::from_cell_flags(cell.flags),
            width: TerminalCellWidth::from_cell_width(cell.width),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TerminalRgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl TerminalRgb {
    fn from_rgb(rgb: vt::Rgb) -> Self {
        Self { r: rgb.r, g: rgb.g, b: rgb.b }
    }
}

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct TerminalCellFlags: u32 {
        const BOLD = 1 << 0;
        const ITALIC = 1 << 1;
        const FAINT = 1 << 2;
        const BLINK = 1 << 3;
        const INVERSE = 1 << 4;
        const INVISIBLE = 1 << 5;
        const STRIKETHROUGH = 1 << 6;
        const OVERLINE = 1 << 7;
        const UNDERLINE = 1 << 8;
    }
}

impl TerminalCellFlags {
    fn from_cell_flags(flags: vt::CellFlags) -> Self {
        let mut out = Self::empty();
        if flags.contains(vt::CellFlags::BOLD) {
            out |= Self::BOLD;
        }
        if flags.contains(vt::CellFlags::ITALIC) {
            out |= Self::ITALIC;
        }
        if flags.contains(vt::CellFlags::FAINT) {
            out |= Self::FAINT;
        }
        if flags.contains(vt::CellFlags::BLINK) {
            out |= Self::BLINK;
        }
        if flags.contains(vt::CellFlags::INVERSE) {
            out |= Self::INVERSE;
        }
        if flags.contains(vt::CellFlags::INVISIBLE) {
            out |= Self::INVISIBLE;
        }
        if flags.contains(vt::CellFlags::STRIKETHROUGH) {
            out |= Self::STRIKETHROUGH;
        }
        if flags.contains(vt::CellFlags::OVERLINE) {
            out |= Self::OVERLINE;
        }
        if flags.contains(vt::CellFlags::UNDERLINE) {
            out |= Self::UNDERLINE;
        }
        out
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TerminalCellWidth {
    #[default]
    Narrow,
    Wide,
    SpacerTail,
    SpacerHead,
}

impl TerminalCellWidth {
    fn from_cell_width(width: vt::CellWidth) -> Self {
        match width {
            vt::CellWidth::Narrow => Self::Narrow,
            vt::CellWidth::Wide => Self::Wide,
            vt::CellWidth::SpacerTail => Self::SpacerTail,
            vt::CellWidth::SpacerHead => Self::SpacerHead,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TerminalCursor {
    pub col: u16,
    pub row: u16,
    pub visible: bool,
    pub style: TerminalCursorStyle,
    pub wide_tail: bool,
}

impl TerminalCursor {
    fn from_cursor_state(cursor: vt::CursorState) -> Self {
        Self {
            col: cursor.col,
            row: cursor.row,
            visible: cursor.visible,
            style: TerminalCursorStyle::from_cursor_style(cursor.style),
            wide_tail: cursor.wide_tail,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TerminalCursorStyle {
    Bar,
    #[default]
    Block,
    Underline,
    BlockHollow,
}

impl TerminalCursorStyle {
    fn from_cursor_style(style: vt::CursorStyle) -> Self {
        match style {
            vt::CursorStyle::Bar => Self::Bar,
            vt::CursorStyle::Block => Self::Block,
            vt::CursorStyle::Underline => Self::Underline,
            vt::CursorStyle::BlockHollow => Self::BlockHollow,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum TerminalInputEvent {
    Key(TerminalKeyEvent),
    Text(TerminalTextEvent),
    Mouse(TerminalMouseEvent),
    Focus(TerminalFocusEvent),
    Paste(TerminalPasteEvent),
    Resize(TerminalResizeEvent),
    RawBytes(Vec<u8>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalKeyEvent {
    pub key: TerminalKey,
    pub modifiers: TerminalModifiers,
    pub consumed_modifiers: TerminalModifiers,
    pub action: TerminalKeyAction,
    pub generated_text: Option<String>,
    pub platform_keycode: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TerminalKeyAction {
    #[default]
    Press,
    Repeat,
    Release,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminalKey {
    UnicodeScalar(u32),
    Named(TerminalNamedKey),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalNamedKey {
    Enter,
    Escape,
    Backspace,
    Tab,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Function(u8),
}

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct TerminalModifiers: u16 {
        const SHIFT = 1 << 0;
        const CTRL = 1 << 1;
        const ALT = 1 << 2;
        const SUPER = 1 << 3;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalTextEvent {
    pub text: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TerminalMouseEvent {
    pub kind: TerminalMouseEventKind,
    pub button: Option<TerminalMouseButton>,
    pub buttons: TerminalMouseButtons,
    pub modifiers: TerminalModifiers,
    pub cell_col: u16,
    pub cell_row: u16,
    pub x_px: f32,
    pub y_px: f32,
    pub wheel_delta_x: f32,
    pub wheel_delta_y: f32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalMouseEventKind {
    Press,
    Release,
    Move,
    Wheel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalMouseButton {
    Left,
    Middle,
    Right,
    Back,
    Forward,
}

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct TerminalMouseButtons: u16 {
        const LEFT = 1 << 0;
        const MIDDLE = 1 << 1;
        const RIGHT = 1 << 2;
        const BACK = 1 << 3;
        const FORWARD = 1 << 4;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalFocusEvent {
    pub focused: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalPasteEvent {
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TerminalResizeEvent {
    pub cols: u16,
    pub rows: u16,
    pub cell_width_px: f32,
    pub cell_height_px: f32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vt::{CellFlags, CellWidth, CursorState, CursorStyle, ResolvedCell, Rgb};

    #[test]
    fn snapshot_conversion_preserves_grid_shape_and_cells() {
        let grid = ScreenGrid {
            cols: 2,
            rows: 1,
            cursor: CursorState { col: 1, row: 0, visible: true, style: CursorStyle::Bar, wide_tail: true },
            cells: vec![
                ResolvedCell {
                    graphemes: vec!['Z' as u32, 0x0301],
                    fg: Rgb { r: 10, g: 20, b: 30 },
                    bg: Rgb { r: 40, g: 50, b: 60 },
                    flags: CellFlags::BOLD | CellFlags::ITALIC | CellFlags::UNDERLINE,
                    width: CellWidth::Wide,
                },
                ResolvedCell { width: CellWidth::SpacerTail, ..ResolvedCell::default() },
            ],
        };

        let snapshot = TerminalSnapshot::from_screen_grid(grid, DirtyState::Full);

        assert_eq!(snapshot.cols, 2);
        assert_eq!(snapshot.rows, 1);
        assert_eq!(snapshot.geometry, TerminalGeometry::default());
        assert_eq!(snapshot.viewport_kind, TerminalViewportKind::LiveNormal);
        assert_eq!(snapshot.scrollback_offset_rows, 0);
        assert_eq!(snapshot.render_generation, 0);
        assert_eq!(snapshot.dirty, DirtyState::Full);
        assert!(snapshot.dirty_rows.is_empty());
        assert_eq!(snapshot.cursor.style, TerminalCursorStyle::Bar);
        assert!(snapshot.cursor.visible);
        assert!(snapshot.cursor.wide_tail);
        assert_eq!(snapshot.cells[0].graphemes, vec!['Z' as u32, 0x0301]);
        assert_eq!(snapshot.cells[0].fg, TerminalRgb { r: 10, g: 20, b: 30 });
        assert_eq!(snapshot.cells[0].bg, TerminalRgb { r: 40, g: 50, b: 60 });
        assert_eq!(snapshot.cells[0].flags, TerminalCellFlags::BOLD | TerminalCellFlags::ITALIC | TerminalCellFlags::UNDERLINE);
        assert_eq!(snapshot.cells[0].width, TerminalCellWidth::Wide);
        assert_eq!(snapshot.cells[1].width, TerminalCellWidth::SpacerTail);
    }

    #[test]
    fn structured_input_events_cover_expected_first_provider_surface() {
        let key = TerminalInputEvent::Key(TerminalKeyEvent {
            key: TerminalKey::Named(TerminalNamedKey::Enter),
            modifiers: TerminalModifiers::CTRL | TerminalModifiers::ALT,
            consumed_modifiers: TerminalModifiers::CTRL,
            action: TerminalKeyAction::Press,
            generated_text: None,
            platform_keycode: 36,
        });
        let mouse = TerminalInputEvent::Mouse(TerminalMouseEvent {
            kind: TerminalMouseEventKind::Press,
            button: Some(TerminalMouseButton::Left),
            buttons: TerminalMouseButtons::LEFT,
            modifiers: TerminalModifiers::SHIFT,
            cell_col: 7,
            cell_row: 3,
            x_px: 70.0,
            y_px: 30.0,
            wheel_delta_x: 0.0,
            wheel_delta_y: 0.0,
        });
        let paste = TerminalInputEvent::Paste(TerminalPasteEvent { text: "hello".to_string() });
        let raw = TerminalInputEvent::RawBytes(b"\x1b[A".to_vec());

        assert!(matches!(key, TerminalInputEvent::Key(_)));
        assert!(matches!(mouse, TerminalInputEvent::Mouse(_)));
        assert!(matches!(paste, TerminalInputEvent::Paste(_)));
        assert!(matches!(raw, TerminalInputEvent::RawBytes(_)));
    }
}
