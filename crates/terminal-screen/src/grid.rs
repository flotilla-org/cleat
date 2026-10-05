use std::fmt;

/// Resolved RGB colour; selector values use lowercase `#rrggbb`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl fmt::Display for Rgb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{:02x}{:02x}{:02x}", self.0, self.1, self.2)
    }
}

/// Style equality includes all flags and colours, but not cell width.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Style {
    pub fg: Rgb,
    pub bg: Rgb,
    pub underline_color: Option<Rgb>,
    pub bold: bool,
    pub faint: bool,
    pub italic: bool,
    pub blink: bool,
    pub inverse: bool,
    pub invisible: bool,
    pub strikethrough: bool,
    pub overline: bool,
    /// 0: none; other values are producer-defined underline styles.
    pub underline_style: u32,
    pub protected: bool,
    pub hyperlink: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SemanticContent {
    #[default]
    Output,
    Input,
    Prompt,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CellWidth {
    #[default]
    Narrow,
    Wide,
    /// A continuation cell carries geometry but contributes no text.
    SpacerTail,
    /// A wrap padding cell contributes no text.
    SpacerHead,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Cell {
    /// One already-segmented grapheme, potentially containing several scalars.
    /// Empty narrow cells render as a space. No Unicode width is guessed here.
    pub grapheme: String,
    pub style: Style,
    pub semantic: SemanticContent,
    pub width: CellWidth,
}

impl Cell {
    pub fn text(&self) -> &str {
        if matches!(self.width, CellWidth::SpacerHead | CellWidth::SpacerTail) {
            ""
        } else if self.grapheme.is_empty() {
            " "
        } else {
            &self.grapheme
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SemanticPrompt {
    #[default]
    None,
    Prompt,
    Continuation,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RowMetadata {
    pub soft_wrap: bool,
    pub wrap_continuation: bool,
    pub semantic_prompt: SemanticPrompt,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CursorStyle {
    Bar,
    #[default]
    Block,
    Underline,
    Hollow,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Cursor {
    pub col: u16,
    pub row: u16,
    /// Hidden or off-viewport cursors do not produce a cursor pseudo-element.
    pub visible: bool,
    pub style: CursorStyle,
    pub blinking: bool,
    pub password_input: bool,
    pub wide_tail: bool,
}

/// Zero-based, half-open terminal cell coordinates, independent of UTF-8 length.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Rect {
    pub col: u16,
    pub row: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    pub fn contains(self, other: Self) -> bool {
        u32::from(other.col) >= u32::from(self.col)
            && u32::from(other.row) >= u32::from(self.row)
            && u32::from(other.col) + u32::from(other.width) <= u32::from(self.col) + u32::from(self.width)
            && u32::from(other.row) + u32::from(other.height) <= u32::from(self.row) + u32::from(self.height)
    }

    pub(crate) fn area(self) -> u32 {
        u32::from(self.width) * u32::from(self.height)
    }
}

/// An owned, validated, rectangular snapshot. Producers resolve styles first.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScreenGrid {
    cols: u16,
    rows: u16,
    cells: Vec<Cell>,
    row_metadata: Vec<RowMetadata>,
    pub cursor: Cursor,
    pub generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GridError(pub String);

impl fmt::Display for GridError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for GridError {}

impl ScreenGrid {
    pub fn new(
        cols: u16,
        rows: u16,
        cells: Vec<Cell>,
        row_metadata: Vec<RowMetadata>,
        cursor: Cursor,
        generation: u64,
    ) -> Result<Self, GridError> {
        if cells.len() != usize::from(cols) * usize::from(rows) || row_metadata.len() != usize::from(rows) {
            return Err(GridError("cell and metadata counts must match screen dimensions".into()));
        }
        if cursor.visible && (cursor.col >= cols || cursor.row >= rows) {
            return Err(GridError("visible cursor is outside the grid".into()));
        }
        Ok(Self { cols, rows, cells, row_metadata, cursor, generation })
    }

    pub fn cols(&self) -> u16 {
        self.cols
    }
    pub fn rows(&self) -> u16 {
        self.rows
    }
    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }
    pub fn row_metadata(&self) -> &[RowMetadata] {
        &self.row_metadata
    }
    pub fn cell(&self, col: u16, row: u16) -> Option<&Cell> {
        if col >= self.cols || row >= self.rows {
            return None;
        }
        self.cells.get(usize::from(row) * usize::from(self.cols) + usize::from(col))
    }
    pub fn row(&self, row: u16) -> Option<&[Cell]> {
        if row >= self.rows {
            return None;
        }
        let start = usize::from(row) * usize::from(self.cols);
        Some(&self.cells[start..start + usize::from(self.cols)])
    }
    pub fn text(&self, rect: Rect) -> String {
        let mut text = String::new();
        for y in u32::from(rect.row)..(u32::from(rect.row) + u32::from(rect.height)).min(u32::from(self.rows)) {
            if y > u32::from(rect.row) {
                text.push('\n');
            }
            for x in u32::from(rect.col)..(u32::from(rect.col) + u32::from(rect.width)).min(u32::from(self.cols)) {
                text.push_str(self.cell(x as u16, y as u16).unwrap().text());
            }
        }
        text
    }
    pub fn bounds(&self) -> Rect {
        Rect { col: 0, row: 0, width: self.cols, height: self.rows }
    }
}
