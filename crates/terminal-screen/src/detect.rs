use crate::{Rect, ScreenGrid};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BandKind {
    Styled,
    Separator,
    Blank,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Band {
    pub rect: Rect,
    pub kind: BandKind,
    pub text: String,
    pub confidence: f32,
}

/// Full-width visual bands, independent of any application's interpretation.
/// Blank rows score 0.5; repeated separators 0.95; uniform, contrasting styles
/// 0.9. The entire screen having one style is not a contrasting band.
pub fn detect_bands(grid: &ScreenGrid) -> Vec<Band> {
    let mut bands = Vec::new();
    for row in 0..grid.rows() {
        let cells = grid.row(row).expect("row iteration stays inside validated grid dimensions");
        if cells.is_empty() {
            continue;
        }
        let rect = Rect { col: 0, row, width: grid.cols(), height: 1 };
        let text = grid.text(rect);
        let mut chars = text.chars();
        let first = chars.next();
        let separator = first.is_some_and(|c| matches!(c, '─' | '═' | '━' | '-' | '_' | '=')) && chars.all(|c| Some(c) == first);
        let uniform = cells.iter().all(|c| c.style == cells[0].style);
        let contrast = [row.checked_sub(1), row.checked_add(1).filter(|r| *r < grid.rows())]
            .into_iter()
            .flatten()
            .any(|r| grid.row(r).expect("neighbour indices are checked against grid dimensions").iter().any(|c| c.style != cells[0].style));
        let kind = if text.trim().is_empty() {
            Some((BandKind::Blank, 0.5))
        } else if separator && grid.cols() >= 2 {
            Some((BandKind::Separator, 0.95))
        } else if uniform && contrast {
            Some((BandKind::Styled, 0.9))
        } else {
            None
        };
        if let Some((kind, confidence)) = kind {
            bands.push(Band { rect, kind, text, confidence });
        }
    }
    bands
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BorderStyle {
    Unicode,
    Ascii,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DetectedBox {
    pub rect: Rect,
    pub title: Option<String>,
    pub border_style: BorderStyle,
    pub confidence: f32,
}

/// Complete single, double, rounded, heavy Unicode or ASCII rectangles.
/// Titles may interrupt the top horizontal edge; all other edges must be whole.
/// Broken borders are omitted, rather than reported as a certain rectangle.
pub fn detect_boxes(grid: &ScreenGrid) -> Vec<DetectedBox> {
    let mut boxes = Vec::new();
    let glyph = |col, row| grid.cell(col, row).map(|c| c.text()).unwrap_or("");
    for top in 0..grid.rows() {
        for left in 0..grid.cols() {
            let (tr, bl, br, horizontal, vertical, style) = match glyph(left, top) {
                "┌" => ("┐", "└", "┘", "─", "│", BorderStyle::Unicode),
                "╔" => ("╗", "╚", "╝", "═", "║", BorderStyle::Unicode),
                "╭" => ("╮", "╰", "╯", "─", "│", BorderStyle::Unicode),
                "┏" => ("┓", "┗", "┛", "━", "┃", BorderStyle::Unicode),
                "+" => ("+", "+", "+", "-", "|", BorderStyle::Ascii),
                _ => continue,
            };
            for right in left.saturating_add(2)..grid.cols() {
                if glyph(right, top) != tr {
                    continue;
                }
                let top_cells = (left + 1..right).map(|x| glyph(x, top)).collect::<Vec<_>>();
                if !top_cells.contains(&horizontal) {
                    continue;
                }
                // A title is printable text, not another interrupted box edge.
                if top_cells.iter().any(|s| s.chars().any(|c| "┌┐└┘╔╗╚╝╭╮╰╯┏┓┗┛│║┃+|".contains(c))) {
                    continue;
                }
                for bottom in top.saturating_add(2)..grid.rows() {
                    if glyph(left, bottom) != bl || glyph(right, bottom) != br {
                        continue;
                    }
                    if !(left + 1..right).all(|x| glyph(x, bottom) == horizontal) {
                        continue;
                    }
                    if !(top + 1..bottom).all(|y| glyph(left, y) == vertical && glyph(right, y) == vertical) {
                        continue;
                    }
                    let title = top_cells.concat().trim_matches(|c: char| c.is_whitespace() || horizontal.contains(c)).to_string();
                    boxes.push(DetectedBox {
                        rect: Rect { col: left, row: top, width: right - left + 1, height: bottom - top + 1 },
                        title: (!title.is_empty()).then_some(title),
                        border_style: style,
                        confidence: if style == BorderStyle::Ascii { 0.85 } else { 1.0 },
                    });
                }
            }
        }
    }
    boxes
}
