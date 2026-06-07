use crate::vt::{self, ScreenGrid};

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct ProviderFeatures: u32 {
        const CELL_SNAPSHOTS = 1 << 0;
        const DAMAGE_ROWS = 1 << 1;
        const STRUCTURED_MOUSE_INPUT = 1 << 2;
        const IMAGE_STATE = 1 << 3;
        const REMOTE_TARGETS = 1 << 4;
        const RENDER_UPDATES = 1 << 5;
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
    pub scrollbar: TerminalScrollbarState,
    pub render_generation: u64,
    pub cells: Vec<TerminalCell>,
    pub cursor: TerminalCursor,
    pub dirty: DirtyState,
    pub dirty_rows: Vec<u16>,
}

impl TerminalSnapshot {
    pub fn from_screen_grid(grid: ScreenGrid, dirty: DirtyState) -> Self {
        let dirty_rows = grid.dirty_rows.clone();
        Self {
            cols: grid.cols,
            rows: grid.rows,
            geometry: TerminalGeometry::default(),
            viewport_kind: TerminalViewportKind::LiveNormal,
            scrollback_offset_rows: 0,
            scrollbar: TerminalScrollbarState::for_live_viewport(TerminalViewportKind::LiveNormal, grid.rows),
            render_generation: 0,
            cells: grid.cells.into_iter().map(TerminalCell::from_resolved_cell).collect(),
            cursor: TerminalCursor::from_cursor_state(grid.cursor),
            dirty,
            dirty_rows,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TerminalRenderUpdateOpKind {
    #[default]
    FullVisibleReplace,
    RowReplace,
    ScrollCopy,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TerminalRenderUpdateOp {
    pub kind: TerminalRenderUpdateOpKind,
    pub first_row: u16,
    pub row_count: u16,
    pub col_count: u16,
    pub rows: Vec<TerminalRenderRow>,
    pub src_row: u16,
    pub dst_row: u16,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TerminalRenderRow {
    pub row: u16,
    pub col_count: u16,
    pub cells: Vec<TerminalRenderCell>,
    pub wrap: bool,
    pub wrap_continuation: bool,
    pub has_graphemes: bool,
    pub has_styling: bool,
    pub has_hyperlink: bool,
    pub semantic_prompt: u32,
    pub has_kitty_virtual_placeholder: bool,
    pub dirty: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TerminalRenderCell {
    pub graphemes: Vec<u32>,
    pub style: TerminalRenderStyle,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TerminalRenderStyle {
    pub flags: TerminalCellFlags,
    pub width: TerminalCellWidth,
    pub resolved_fg: TerminalRgb,
    pub resolved_bg: TerminalRgb,
    pub fg_color: TerminalStyleColor,
    pub bg_color: TerminalStyleColor,
    pub underline_style: u32,
    pub underline_color: TerminalStyleColor,
    pub protected: bool,
    pub semantic: u32,
    pub has_hyperlink: bool,
    pub hyperlink_id: u64,
    pub content_tag: u32,
    pub has_text: bool,
    pub has_styling: bool,
    pub style_id: u16,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TerminalStyleColor {
    pub tag: TerminalStyleColorTag,
    pub palette_index: u8,
    pub rgb: Option<TerminalRgb>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TerminalStyleColorTag {
    #[default]
    None,
    Palette,
    Rgb,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TerminalRenderUpdate {
    pub cols: u16,
    pub rows: u16,
    pub geometry: TerminalGeometry,
    pub viewport_kind: TerminalViewportKind,
    pub scrollback_offset_rows: u64,
    pub scrollbar: TerminalScrollbarState,
    pub render_generation: u64,
    pub cursor: TerminalCursor,
    pub dirty: DirtyState,
    pub ops: Vec<TerminalRenderUpdateOp>,
    pub image_resources: Vec<TerminalImageResource>,
    pub image_placements: Vec<TerminalImagePlacement>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TerminalImageResource {
    pub image_id: u32,
    pub generation: u64,
    pub width_px: u32,
    pub height_px: u32,
    pub format: u32,
    pub compression: u32,
    pub data_len: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TerminalImagePlacement {
    pub image_id: u32,
    pub generation: u64,
    pub placement_id: u32,
    pub z: i32,
    pub viewport_col: i32,
    pub viewport_row: i32,
    pub grid_cols: u32,
    pub grid_rows: u32,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub source_x: u32,
    pub source_y: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub x_offset_px: u32,
    pub y_offset_px: u32,
}

impl TerminalRenderUpdate {
    pub fn from_snapshot(snapshot: TerminalSnapshot) -> Self {
        let mut ops = Vec::new();
        let cols = snapshot.cols as usize;
        match snapshot.dirty {
            DirtyState::Clean => {}
            DirtyState::Partial if !snapshot.dirty_rows.is_empty() && cols != 0 => {
                for row in snapshot.dirty_rows.iter().copied() {
                    let row_idx = row as usize;
                    if row_idx < snapshot.rows as usize {
                        let start = row_idx * cols;
                        let end = start.saturating_add(cols).min(snapshot.cells.len());
                        ops.push(TerminalRenderUpdateOp {
                            kind: TerminalRenderUpdateOpKind::RowReplace,
                            first_row: row,
                            row_count: 1,
                            col_count: snapshot.cols,
                            rows: vec![TerminalRenderRow::from_cells(row, snapshot.cols, snapshot.cells[start..end].iter().cloned(), true)],
                            src_row: 0,
                            dst_row: 0,
                        });
                    }
                }
            }
            DirtyState::Partial | DirtyState::Full => {
                let rows = rows_from_snapshot_cells(snapshot.cols, snapshot.rows, &snapshot.cells, snapshot.dirty);
                ops.push(TerminalRenderUpdateOp {
                    kind: TerminalRenderUpdateOpKind::FullVisibleReplace,
                    first_row: 0,
                    row_count: snapshot.rows,
                    col_count: snapshot.cols,
                    rows,
                    src_row: 0,
                    dst_row: 0,
                });
            }
        }
        Self {
            cols: snapshot.cols,
            rows: snapshot.rows,
            geometry: snapshot.geometry,
            viewport_kind: snapshot.viewport_kind,
            scrollback_offset_rows: snapshot.scrollback_offset_rows,
            scrollbar: snapshot.scrollbar,
            render_generation: snapshot.render_generation,
            cursor: snapshot.cursor,
            dirty: snapshot.dirty,
            ops,
            image_resources: Vec::new(),
            image_placements: Vec::new(),
        }
    }
}

fn rows_from_snapshot_cells(cols: u16, rows: u16, cells: &[TerminalCell], dirty: DirtyState) -> Vec<TerminalRenderRow> {
    let cols_usize = cols as usize;
    if cols_usize == 0 {
        return Vec::new();
    }
    (0..rows)
        .map(|row| {
            let start = row as usize * cols_usize;
            let end = start.saturating_add(cols_usize).min(cells.len());
            TerminalRenderRow::from_cells(row, cols, cells[start..end].iter().cloned(), dirty != DirtyState::Clean)
        })
        .collect()
}

impl TerminalRenderRow {
    pub fn from_cells(row: u16, col_count: u16, cells: impl IntoIterator<Item = TerminalCell>, dirty: bool) -> Self {
        let cells: Vec<TerminalRenderCell> = cells.into_iter().map(TerminalRenderCell::from_terminal_cell).collect();
        Self {
            row,
            col_count,
            cells,
            wrap: false,
            wrap_continuation: false,
            has_graphemes: false,
            has_styling: false,
            has_hyperlink: false,
            semantic_prompt: 0,
            has_kitty_virtual_placeholder: false,
            dirty,
        }
    }
}

impl TerminalRenderCell {
    fn from_terminal_cell(cell: TerminalCell) -> Self {
        let fg = cell.fg;
        let bg = cell.bg;
        Self {
            graphemes: cell.graphemes,
            style: TerminalRenderStyle {
                flags: cell.flags,
                width: cell.width,
                resolved_fg: fg,
                resolved_bg: bg,
                fg_color: TerminalStyleColor::rgb(fg),
                bg_color: TerminalStyleColor::rgb(bg),
                underline_style: cell.underline_style,
                underline_color: cell.underline_color.map(TerminalStyleColor::rgb).unwrap_or_default(),
                protected: cell.protected,
                semantic: cell.semantic,
                has_hyperlink: cell.has_hyperlink,
                hyperlink_id: 0,
                content_tag: 0,
                has_text: true,
                has_styling: !cell.flags.is_empty(),
                style_id: 0,
            },
        }
    }
}

impl TerminalStyleColor {
    pub fn palette(palette_index: u8) -> Self {
        Self { tag: TerminalStyleColorTag::Palette, palette_index, rgb: None }
    }

    pub fn rgb(rgb: TerminalRgb) -> Self {
        Self { tag: TerminalStyleColorTag::Rgb, palette_index: 0, rgb: Some(rgb) }
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TerminalScrollbarState {
    pub viewport_kind: TerminalViewportKind,
    pub total_rows: u64,
    pub viewport_rows: u16,
    pub viewport_top_row: u64,
    pub at_bottom: bool,
}

impl TerminalScrollbarState {
    pub fn new(viewport_kind: TerminalViewportKind, total_rows: u64, viewport_rows: u16, viewport_top_row: u64) -> Self {
        let bottom_top_row = total_rows.saturating_sub(viewport_rows as u64);
        let viewport_top_row = viewport_top_row.min(bottom_top_row);
        Self { viewport_kind, total_rows, viewport_rows, viewport_top_row, at_bottom: viewport_top_row >= bottom_top_row }
    }

    pub fn for_live_viewport(viewport_kind: TerminalViewportKind, viewport_rows: u16) -> Self {
        Self::new(viewport_kind, viewport_rows as u64, viewport_rows, 0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ViewportCommand {
    Top,
    Bottom,
    DeltaRows(i64),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ViewportCommandOutcome {
    Moved,
    NoOp,
    Unsupported,
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
    pub underline_color: Option<TerminalRgb>,
    pub flags: TerminalCellFlags,
    pub underline_style: u32,
    pub width: TerminalCellWidth,
    pub protected: bool,
    pub semantic: u32,
    pub has_hyperlink: bool,
}

impl TerminalCell {
    fn from_resolved_cell(cell: vt::ResolvedCell) -> Self {
        Self {
            graphemes: cell.graphemes,
            fg: TerminalRgb::from_rgb(cell.fg),
            bg: TerminalRgb::from_rgb(cell.bg),
            underline_color: cell.underline_color.map(TerminalRgb::from_rgb),
            flags: TerminalCellFlags::from_cell_flags(cell.flags),
            underline_style: cell.underline_style,
            width: TerminalCellWidth::from_cell_width(cell.width),
            protected: cell.protected,
            semantic: cell.semantic,
            has_hyperlink: cell.has_hyperlink,
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
    pub blink: bool,
    pub wide_tail: bool,
}

impl TerminalCursor {
    fn from_cursor_state(cursor: vt::CursorState) -> Self {
        Self {
            col: cursor.col,
            row: cursor.row,
            visible: cursor.visible,
            style: TerminalCursorStyle::from_cursor_style(cursor.style),
            blink: cursor.blink,
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
            cursor: CursorState { col: 1, row: 0, visible: true, style: CursorStyle::Bar, blink: true, wide_tail: true },
            dirty_rows: Vec::new(),
            cells: vec![
                ResolvedCell {
                    graphemes: vec!['Z' as u32, 0x0301],
                    fg: Rgb { r: 10, g: 20, b: 30 },
                    bg: Rgb { r: 40, g: 50, b: 60 },
                    underline_color: Some(Rgb { r: 70, g: 80, b: 90 }),
                    flags: CellFlags::BOLD | CellFlags::ITALIC | CellFlags::UNDERLINE,
                    underline_style: 3,
                    width: CellWidth::Wide,
                    protected: true,
                    semantic: 2,
                    has_hyperlink: true,
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
        assert_eq!(snapshot.scrollbar, TerminalScrollbarState::for_live_viewport(TerminalViewportKind::LiveNormal, 1));
        assert_eq!(snapshot.render_generation, 0);
        assert_eq!(snapshot.dirty, DirtyState::Full);
        assert!(snapshot.dirty_rows.is_empty());
        assert_eq!(snapshot.cursor.style, TerminalCursorStyle::Bar);
        assert!(snapshot.cursor.blink);
        assert!(snapshot.cursor.visible);
        assert!(snapshot.cursor.wide_tail);
        assert_eq!(snapshot.cells[0].graphemes, vec!['Z' as u32, 0x0301]);
        assert_eq!(snapshot.cells[0].fg, TerminalRgb { r: 10, g: 20, b: 30 });
        assert_eq!(snapshot.cells[0].bg, TerminalRgb { r: 40, g: 50, b: 60 });
        assert_eq!(snapshot.cells[0].underline_color, Some(TerminalRgb { r: 70, g: 80, b: 90 }));
        assert_eq!(snapshot.cells[0].flags, TerminalCellFlags::BOLD | TerminalCellFlags::ITALIC | TerminalCellFlags::UNDERLINE);
        assert_eq!(snapshot.cells[0].underline_style, 3);
        assert_eq!(snapshot.cells[0].width, TerminalCellWidth::Wide);
        assert!(snapshot.cells[0].protected);
        assert_eq!(snapshot.cells[0].semantic, 2);
        assert!(snapshot.cells[0].has_hyperlink);
        assert_eq!(snapshot.cells[1].width, TerminalCellWidth::SpacerTail);
    }

    #[test]
    fn render_update_uses_row_ops_for_partial_dirty_rows() {
        let snapshot = TerminalSnapshot {
            cols: 2,
            rows: 2,
            dirty: DirtyState::Partial,
            dirty_rows: vec![1],
            cells: vec![
                TerminalCell { graphemes: vec!['a' as u32], ..TerminalCell::default() },
                TerminalCell { graphemes: vec!['b' as u32], ..TerminalCell::default() },
                TerminalCell { graphemes: vec!['c' as u32], ..TerminalCell::default() },
                TerminalCell { graphemes: vec!['d' as u32], ..TerminalCell::default() },
            ],
            ..TerminalSnapshot::default()
        };

        let update = TerminalRenderUpdate::from_snapshot(snapshot);

        assert_eq!(update.ops.len(), 1);
        assert_eq!(update.ops[0].kind, TerminalRenderUpdateOpKind::RowReplace);
        assert_eq!(update.ops[0].first_row, 1);
        assert_eq!(update.ops[0].row_count, 1);
        assert_eq!(update.ops[0].col_count, 2);
        assert_eq!(update.ops[0].rows[0].cells[0].graphemes, vec!['c' as u32]);
        assert_eq!(update.ops[0].rows[0].cells[1].graphemes, vec!['d' as u32]);
    }

    #[test]
    fn render_update_uses_full_replace_when_damage_is_full_or_unknown() {
        let full = TerminalRenderUpdate::from_snapshot(TerminalSnapshot {
            cols: 2,
            rows: 1,
            dirty: DirtyState::Full,
            cells: vec![TerminalCell::default(), TerminalCell::default()],
            ..TerminalSnapshot::default()
        });
        assert_eq!(full.ops.len(), 1);
        assert_eq!(full.ops[0].kind, TerminalRenderUpdateOpKind::FullVisibleReplace);
        assert_eq!(full.ops[0].rows[0].cells.len(), 2);

        let partial_unknown = TerminalRenderUpdate::from_snapshot(TerminalSnapshot {
            cols: 2,
            rows: 1,
            dirty: DirtyState::Partial,
            cells: vec![TerminalCell::default(), TerminalCell::default()],
            ..TerminalSnapshot::default()
        });
        assert_eq!(partial_unknown.ops.len(), 1);
        assert_eq!(partial_unknown.ops[0].kind, TerminalRenderUpdateOpKind::FullVisibleReplace);
    }

    #[test]
    fn scrollbar_state_clamps_to_valid_range() {
        assert_eq!(TerminalScrollbarState::new(TerminalViewportKind::LiveNormal, 10, 3, 99), TerminalScrollbarState {
            viewport_kind: TerminalViewportKind::LiveNormal,
            total_rows: 10,
            viewport_rows: 3,
            viewport_top_row: 7,
            at_bottom: true,
        });
        assert_eq!(TerminalScrollbarState::new(TerminalViewportKind::LiveNormal, 2, 5, 3), TerminalScrollbarState {
            viewport_kind: TerminalViewportKind::LiveNormal,
            total_rows: 2,
            viewport_rows: 5,
            viewport_top_row: 0,
            at_bottom: true,
        });
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
