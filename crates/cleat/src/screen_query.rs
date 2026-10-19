use terminal_screen::*;

use crate::provider::{TerminalCellFlags as Flags, TerminalCellWidth, TerminalCursorStyle};

// Convert full render data without losing styles or physical row metadata.
// The full render update supplies row metadata and resolved cell styles.
pub fn grid_from_update(update: crate::provider::TerminalRenderUpdate) -> Result<ScreenGrid, String> {
    let generation = update.render_generation;
    let mut cells = vec![Cell::default(); usize::from(update.cols) * usize::from(update.rows)];
    let mut metadata = vec![RowMetadata::default(); usize::from(update.rows)];
    for row in update.ops.iter().flat_map(|op| &op.rows) {
        if row.row >= update.rows || row.cells.len() > usize::from(update.cols) {
            return Err(format!("render row {} with {} cells is outside {}x{} screen", row.row, row.cells.len(), update.cols, update.rows));
        }
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
    .map_err(|error| error.to_string())
}

/// Stable version-one CLI and HTTP screen query response. Coordinates are cells.
pub fn query(tree: &ScreenTree, selector: Option<&Selector>) -> serde_json::Value {
    let node_json = |id: NodeId, captures: std::collections::BTreeMap<String, String>| {
        let node = tree.node(id).expect("tree-owned id");
        serde_json::json!({
            "id": id.0, "parent": node.parent().map(|p| p.0),
            "children": node.children().iter().map(|c| c.0).collect::<Vec<_>>(),
            "element": node.element, "roles": node.roles, "attributes": node.attributes,
            "bounds": {"col":node.bounds.col,"row":node.bounds.row,"width":node.bounds.width,"height":node.bounds.height},
            "text": node.text, "confidence": node.confidence, "captures": captures
        })
    };
    let nodes: Vec<_> = match selector {
        Some(selector) => selector.evaluate_with_captures(tree).into_iter().map(|m| node_json(m.node, m.captures)).collect(),
        None => tree.document_order().into_iter().map(|id| node_json(id, Default::default())).collect(),
    };
    serde_json::json!({"schema_version":1,"selector_syntax":"stable","producer":"graph","generation":tree.generation,"nodes":nodes})
}

pub(crate) fn error(code: &str, message: impl std::fmt::Display) -> serde_json::Value {
    serde_json::json!({"error":{"code":code,"message":message.to_string()}})
}

/// Encode query values bytewise, including selector punctuation and Unicode.
pub(crate) fn encode(value: &str) -> String {
    value.bytes().map(|b| format!("%{b:02X}")).collect()
}

pub(crate) fn parameters(query: &str) -> Result<std::collections::BTreeMap<String, String>, String> {
    fn decode(value: &str) -> Result<String, String> {
        let mut bytes = Vec::new();
        let mut input = value.bytes();
        while let Some(b) = input.next() {
            bytes.push(match b {
                b'+' => b' ',
                b'%' => {
                    let hi = input.next().and_then(|b| (b as char).to_digit(16));
                    let lo = input.next().and_then(|b| (b as char).to_digit(16));
                    match (hi, lo) {
                        (Some(h), Some(l)) => (h * 16 + l) as u8,
                        _ => return Err("invalid percent escape".into()),
                    }
                }
                b => b,
            });
        }
        String::from_utf8(bytes).map_err(|e| e.to_string())
    }
    query
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|part| {
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            Ok((decode(key)?, decode(value)?))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The CLI percent encoder must round-trip selector punctuation, Unicode,
    // whitespace, empty values and every ASCII byte through HTTP query parsing.
    #[test]
    fn query_value_round_trip() {
        let cases = [String::new(), "region:matches('(?P<x>é + & % 😀)')".into(), (0..=127).map(char::from).collect()];
        for value in cases {
            assert_eq!(parameters(&format!("select={}", encode(&value))).unwrap()["select"], value);
        }
        for invalid in ["select=%", "select=%Q1", "select=%FF"] {
            assert!(parameters(invalid).is_err());
        }
    }

    // Provider rows outside the declared grid must return an error, never panic
    // or spill cells into the next row. Generate valid and off-by-one bounds.
    #[test]
    fn provider_row_bounds_are_checked() {
        use crate::provider::{TerminalRenderCell, TerminalRenderRow, TerminalRenderUpdate, TerminalRenderUpdateOp};
        for rows in 1..=3 {
            for cols in 1..=3 {
                for row in [0, rows - 1, rows, rows + 1] {
                    for width in [0, cols, cols + 1] {
                        let update = TerminalRenderUpdate {
                            rows,
                            cols,
                            ops: vec![TerminalRenderUpdateOp {
                                rows: vec![TerminalRenderRow {
                                    row,
                                    cells: vec![TerminalRenderCell::default(); usize::from(width)],
                                    ..Default::default()
                                }],
                                ..Default::default()
                            }],
                            ..Default::default()
                        };
                        assert_eq!(grid_from_update(update).is_ok(), row < rows && width <= cols);
                    }
                }
            }
        }
    }

    // Screen queries preserve generation, provenance, links and combined named
    // captures, and a valid unmatched selector returns an empty match set.
    #[test]
    fn output_contract() {
        let grid = ScreenGrid::new(
            3,
            1,
            "abc".chars().map(|c| Cell { grapheme: c.to_string(), ..Default::default() }).collect(),
            vec![RowMetadata::default()],
            Cursor::default(),
            42,
        )
        .unwrap();
        let tree = analyze(&grid);
        let all = query(&tree, None);
        assert_eq!(all["generation"], 42);
        assert_eq!(all["selector_syntax"], "stable");
        assert!(all["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| node["element"] == "region" && node["attributes"]["provenance"] == "graph"));
        let matched = query(&tree, Some(&Selector::parse("span:matches(/(?P<a>a)(?P<bc>bc)/)").unwrap()));
        assert_eq!(matched["nodes"][0]["captures"], serde_json::json!({"a":"a","bc":"bc"}));
        assert_eq!(matched["nodes"][0]["bounds"]["width"], 3);
        assert_eq!(query(&tree, Some(&Selector::parse("dialog").unwrap()))["nodes"], serde_json::json!([]));
    }
}

// #316's release budget includes building the selector tree and evaluation,
// not only graph partitioning. Keep wall-clock timing out of ordinary CI.
#[cfg(test)]
#[test]
#[ignore = "release acceptance; run with --release --ignored --nocapture"]
fn selector_evaluation_budget() {
    if cfg!(debug_assertions) {
        panic!("this budget is for release builds");
    }
    let grid = ScreenGrid::new(
        200,
        50,
        vec![Cell { grapheme: "+".into(), ..Default::default() }; 10000],
        vec![RowMetadata::default(); 50],
        Cursor::default(),
        1,
    )
    .unwrap();
    let selector = Selector::parse("region:has-text('+'):last-match").unwrap();
    for _ in 0..5 {
        std::hint::black_box(selector.evaluate(&analyze(&grid)));
    }
    let start = std::time::Instant::now();
    for _ in 0..100 {
        std::hint::black_box(selector.evaluate(&analyze(&grid)));
    }
    let ms = start.elapsed().as_secs_f64() * 10.0;
    eprintln!("200x50 adversarial tree + selector: {ms:.3}ms/evaluation");
    assert!(ms < 5.0, "{ms}ms exceeds 5ms");
}
