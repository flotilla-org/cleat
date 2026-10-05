#![cfg(feature = "ghostty-vt")]

use cleat::{
    provider::{DirtyState, TerminalCellFlags as Flags, TerminalCellWidth, TerminalCursorStyle},
    vt::{ghostty::GhosttyVtEngine, VtEngine},
};
use terminal_screen::*;

// Test-local producer adapter: terminal-screen itself has no cleat/VT dependency.
// The full render update supplies row metadata and resolved cell styles.
fn snapshot(vt: &mut GhosttyVtEngine, generation: u64) -> ScreenGrid {
    let update = vt.render_update(DirtyState::Full).unwrap();
    let mut cells = vec![Cell::default(); usize::from(update.cols) * usize::from(update.rows)];
    let mut metadata = vec![RowMetadata::default(); usize::from(update.rows)];
    for row in update.ops.iter().flat_map(|op| &op.rows) {
        metadata[usize::from(row.row)] = RowMetadata {
            soft_wrap: row.wrap,
            wrap_continuation: row.wrap_continuation,
            semantic_prompt: match row.semantic_prompt {
                1 => SemanticPrompt::Prompt,
                2 => SemanticPrompt::Continuation,
                _ => SemanticPrompt::None,
            },
        };
        for (col, cell) in row.cells.iter().enumerate() {
            let style = &cell.style;
            let flags = style.flags;
            cells[usize::from(row.row) * usize::from(update.cols) + col] = Cell {
                grapheme: cell.graphemes.iter().filter_map(|cp| char::from_u32(*cp)).collect(),
                width: match style.width {
                    TerminalCellWidth::Narrow => CellWidth::Narrow,
                    TerminalCellWidth::Wide => CellWidth::Wide,
                    TerminalCellWidth::SpacerTail => CellWidth::SpacerTail,
                    TerminalCellWidth::SpacerHead => CellWidth::SpacerHead,
                },
                semantic: match style.semantic {
                    1 => SemanticContent::Input,
                    2 => SemanticContent::Prompt,
                    _ => SemanticContent::Output,
                },
                style: Style {
                    fg: Rgb(style.resolved_fg.r, style.resolved_fg.g, style.resolved_fg.b),
                    bg: Rgb(style.resolved_bg.r, style.resolved_bg.g, style.resolved_bg.b),
                    bold: flags.contains(Flags::BOLD),
                    faint: flags.contains(Flags::FAINT),
                    inverse: flags.contains(Flags::INVERSE),
                    invisible: flags.contains(Flags::INVISIBLE),
                    italic: flags.contains(Flags::ITALIC),
                    blink: flags.contains(Flags::BLINK),
                    strikethrough: flags.contains(Flags::STRIKETHROUGH),
                    overline: flags.contains(Flags::OVERLINE),
                    underline_style: style.underline_style,
                    protected: style.protected,
                    hyperlink: (!style.hyperlink_uri.is_empty()).then(|| String::from_utf8_lossy(&style.hyperlink_uri).into_owned()),
                    ..Style::default()
                },
            };
        }
    }
    ScreenGrid::new(
        update.cols,
        update.rows,
        cells,
        metadata,
        Cursor {
            col: update.cursor.col,
            row: update.cursor.row,
            visible: update.cursor.visible,
            blinking: update.cursor.blink,
            wide_tail: update.cursor.wide_tail,
            style: match update.cursor.style {
                TerminalCursorStyle::Bar => CursorStyle::Bar,
                TerminalCursorStyle::Block => CursorStyle::Block,
                TerminalCursorStyle::Underline => CursorStyle::Underline,
                TerminalCursorStyle::BlockHollow => CursorStyle::Hollow,
            },
            // Current cleat render snapshots do not expose password-input state.
            password_input: false,
        },
        generation,
    )
    .unwrap()
}

fn selected_text(grid: &ScreenGrid, selector: &str) -> String {
    analyze(grid).select(selector).unwrap().iter().map(|node| node.text.as_str()).collect::<String>().trim().to_string()
}

// #23 acceptance, amended by governor guidance: replay Codex 0.160 through
// the real VT. Exclude faint placeholder and bold marker for empty/draft values.
// Separately preserve marker-inclusive structural evidence; do not strip any
// application prefix in the structural tree (recognizer extraction is #313).
#[test]
fn recorded_codex_placeholder_and_draft() {
    let cast = include_str!("../../../docs/design/semantic-prompt-evidence/osc133-codex-normal.cast");
    let mut lines = cast.lines();
    let header: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["version"], 3);
    let mut vt = GhosttyVtEngine::new(header["term"]["cols"].as_u64().unwrap() as u16, header["term"]["rows"].as_u64().unwrap() as u16);
    let mut empty_checked = false;
    let mut generation = 0;
    for line in lines {
        let event: serde_json::Value = serde_json::from_str(line).unwrap();
        if event[1] == "i" && event[2] == "draft probe" {
            let grid = snapshot(&mut vt, generation);
            let tree = analyze(&grid);
            assert_eq!(tree.select("row:has(cursor)").unwrap().len(), 1);
            assert!(!tree.select("row:has(cursor) span[faint]").unwrap().is_empty(), "placeholder style is present");
            assert_eq!(selected_text(&grid, "row:has(cursor) span:not([faint])"), "›");
            // The authorized acceptance query excludes the bold marker by style.
            assert_eq!(selected_text(&grid, "row:has(cursor) span:not([faint]):not([bold])"), "");
            empty_checked = true;
        }
        if event[1] == "o" {
            vt.feed(event[2].as_str().unwrap().as_bytes()).unwrap();
            generation += 1;
        }
    }
    assert!(empty_checked, "the recording must contain the named draft checkpoint");
    let grid = snapshot(&mut vt, generation);
    assert_eq!(selected_text(&grid, "row:has(cursor) span:not([faint])"), "› draft probe");
    assert_eq!(selected_text(&grid, "row:has(cursor) span:not([faint]):not([bold])"), "draft probe");
    assert_eq!(analyze(&grid).select("row:has(cursor)").unwrap()[0].bounds.row, 36);
}
