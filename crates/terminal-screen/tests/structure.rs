use terminal_screen::*;

fn grid(lines: &[&str]) -> ScreenGrid {
    let cols = lines.iter().map(|s| s.chars().count()).max().unwrap_or(0) as u16;
    let cells = lines
        .iter()
        .flat_map(|line| {
            let mut cells = line.chars().map(|c| Cell { grapheme: c.to_string(), ..Cell::default() }).collect::<Vec<_>>();
            cells.resize(usize::from(cols), Cell::default());
            cells
        })
        .collect();
    ScreenGrid::new(cols, lines.len() as u16, cells, vec![RowMetadata::default(); lines.len()], Cursor::default(), 7).unwrap()
}

// #23: style runs conserve every cell's text and geometry, and split precisely
// on style or semantic changes. Generate widths including zero, every flag,
// colour changes, semantic variants and repeated/non-repeated style runs.
#[test]
fn generated_style_runs_conserve_the_grid() {
    for cols in [0, 1, 2, 3, 16, 33] {
        for flag in 0..12 {
            for period in [1, 2, 5] {
                let cells = (0..cols)
                    .map(|col| {
                        let enabled = (col / period) % 2 == 1;
                        let mut cell = Cell { grapheme: char::from(b'a' + (col % 26) as u8).to_string(), ..Cell::default() };
                        match flag {
                            0 => cell.style.bold = enabled,
                            1 => cell.style.faint = enabled,
                            2 => cell.style.inverse = enabled,
                            3 => cell.style.invisible = enabled,
                            4 => cell.style.italic = enabled,
                            5 => cell.style.blink = enabled,
                            6 => cell.style.strikethrough = enabled,
                            7 => cell.style.overline = enabled,
                            8 => cell.style.underline_style = u32::from(enabled),
                            9 => cell.style.fg = Rgb(u8::from(enabled), 0, 0),
                            10 => cell.style.bg = Rgb(0, u8::from(enabled), 0),
                            _ => cell.semantic = if enabled { SemanticContent::Input } else { SemanticContent::Output },
                        }
                        cell
                    })
                    .collect::<Vec<_>>();
                let expected_runs = cells.windows(2).filter(|w| w[0].style != w[1].style || w[0].semantic != w[1].semantic).count()
                    + usize::from(cols != 0);
                let g = ScreenGrid::new(cols, 1, cells, vec![RowMetadata::default()], Cursor::default(), 3).unwrap();
                let tree = analyze(&g);
                let spans = tree.select("span").unwrap();
                assert_eq!(spans.len(), expected_runs, "width={cols}, flag={flag}, period={period}");
                assert_eq!(spans.iter().map(|s| s.text.as_str()).collect::<String>(), g.text(g.bounds()));
                assert_eq!(spans.iter().map(|s| u32::from(s.bounds.width)).sum::<u32>(), u32::from(cols));
                let mut next = 0;
                for span in spans {
                    assert_eq!(span.bounds.col, next);
                    next += span.bounds.width;
                }
            }
        }
    }
}

// #23 and xa11y: :nth is one-based over matching rows, :last is the bottom
// matching row even when bands give physical rows different parents.
#[test]
fn positions_are_match_indices_in_document_order() {
    for rows in 1..=8 {
        let g = grid(&vec!["line"; rows]);
        let t = analyze(&g);
        for index in 1..=rows {
            let selected = t.select(&format!("row:nth({index})")).unwrap();
            assert_eq!(selected.len(), 1);
            assert_eq!(selected[0].bounds.row, (index - 1) as u16);
        }
        assert!(t.select(&format!("row:nth({})", rows + 1)).unwrap().is_empty());
        assert_eq!(t.select("row:last").unwrap()[0].bounds.row, rows as u16 - 1);
        assert_eq!(t.select("row:last").unwrap()[0].attribute("index-from-bottom"), Some("0"));
    }
    let t = analyze(&grid(&["a", "", "z"]));
    assert_eq!(t.select("row:last").unwrap()[0].text, "z");
    assert_eq!(t.select("row:not([text=z]):last").unwrap()[0].bounds.row, 1);
}

// #23: the cursor pseudo-element selects exactly its physical row; hidden
// cursors create no evidence. :not([faint]) excludes only the faint runs.
#[test]
fn cursor_and_boolean_style_filters() {
    for visible in [false, true] {
        for row in 0..3 {
            let mut cells = grid(&[" abc", " abc", " abc"]).cells().to_vec();
            for c in cells.iter_mut().skip(usize::from(row) * 4).take(2) {
                c.style.faint = true;
            }
            let g = ScreenGrid::new(
                4,
                3,
                cells,
                vec![RowMetadata::default(); 3],
                Cursor { col: 2, row, visible, password_input: true, ..Cursor::default() },
                0,
            )
            .unwrap();
            let t = analyze(&g);
            let selected = t.select("row:has(cursor) span:not([faint])").unwrap();
            if visible {
                assert_eq!(selected.iter().map(|n| n.text.as_str()).collect::<String>(), "bc");
            } else {
                assert!(selected.is_empty());
            }
            assert_eq!(t.select("cursor[password-input]").unwrap().len(), usize::from(visible));
            assert_eq!(t.select("row:has(> cursor)").unwrap().len(), usize::from(visible));
            assert!(t.select("span:has(cursor)").unwrap().is_empty());
        }
    }
}

// #23: UTF-8 byte count and scalar count must never substitute for cell
// coordinates. Combining graphemes are retained; wide tails and wrap heads
// add geometry without adding text. Soft wraps stay physical rows.
#[test]
fn wide_combining_graphemes_and_soft_wraps() {
    let cells = vec![
        Cell { grapheme: "e\u{301}".into(), ..Cell::default() },
        Cell { grapheme: "界".into(), width: CellWidth::Wide, ..Cell::default() },
        Cell { width: CellWidth::SpacerTail, ..Cell::default() },
        Cell { width: CellWidth::SpacerHead, ..Cell::default() },
        Cell { grapheme: "👩‍💻".into(), width: CellWidth::Wide, ..Cell::default() },
        Cell { width: CellWidth::SpacerTail, ..Cell::default() },
        Cell::default(),
        Cell::default(),
    ];
    let metadata = vec![RowMetadata { soft_wrap: true, semantic_prompt: SemanticPrompt::Prompt, ..RowMetadata::default() }, RowMetadata {
        wrap_continuation: true,
        semantic_prompt: SemanticPrompt::Continuation,
        ..RowMetadata::default()
    }];
    let g = ScreenGrid::new(4, 2, cells, metadata, Cursor::default(), 1).unwrap();
    let t = analyze(&g);
    assert_eq!(t.select("row[soft-wrap] span").unwrap()[0].text, "e\u{301}界");
    assert_eq!(t.select("row[wrap-continuation] span").unwrap()[0].text, "👩‍💻  ");
    assert_eq!(t.select("span").unwrap()[0].bounds.width, 4);
    assert_eq!(t.select("row[prompt]").unwrap().len(), 2);
    assert_eq!(t.select("screen").unwrap()[0].text, "e\u{301}界\n👩‍💻  ");
}

// #23: semantic input/prompt/output values split otherwise identical styles,
// and semantic attributes can be queried without adding grammar keywords.
#[test]
fn semantic_content_is_observed_per_span() {
    let mut cells = grid(&["abc"]).cells().to_vec();
    cells[0].semantic = SemanticContent::Prompt;
    cells[1].semantic = SemanticContent::Input;
    let t = analyze(&ScreenGrid::new(3, 1, cells, vec![RowMetadata::default()], Cursor::default(), 0).unwrap());
    for (selector, expected) in [("span[prompt]", "a"), ("span[semantic=input]", "b"), ("span[output]", "c")] {
        assert_eq!(t.select(selector).unwrap()[0].text, expected);
    }
}

// March spec: only complete boxes are certain, ASCII boxes are heuristic,
// and recognizer meaning is never inferred by a structural detector.
#[test]
fn boxes_bands_and_spatial_containment() {
    for (top, middle, bottom, confidence) in [
        ("┌───┐", "│abc│", "└───┘", 1.0),
        ("+---+", "|abc|", "+---+", 0.85),
        ("╔═══╗", "║abc║", "╚═══╝", 1.0),
        ("╭───╮", "│abc│", "╰───╯", 1.0),
        ("┏━━━┓", "┃abc┃", "┗━━━┛", 1.0),
    ] {
        let g = grid(&[top, middle, bottom]);
        let t = analyze(&g);
        assert_eq!(detect_boxes(&g).len(), 1);
        assert_eq!(t.select("box").unwrap()[0].confidence, Some(confidence));
        assert_eq!(t.select("box row").unwrap().len(), 3);
        assert!(t.select("dialog").unwrap().is_empty());
        let broken = grid(&[top, " abc ", bottom]);
        assert!(detect_boxes(&broken).is_empty());
    }
    let g = grid(&["┌─ Jobs ─┐", "│        │", "└────────┘", "----------", "          ", "normal row"]);
    let t = analyze(&g);
    assert_eq!(t.select("box[title=Jobs]").unwrap().len(), 1);
    assert_eq!(t.select("band[kind=separator] > row").unwrap()[0].bounds.row, 3);
    assert_eq!(t.select("band[kind=blank]").unwrap().len(), 1);
    assert_eq!(t.select("row").unwrap().len(), 6);
    for id in t.document_order() {
        let node = t.node(id).unwrap();
        if let Some(parent) = node.parent() {
            assert!(t.node(parent).unwrap().bounds.contains(node.bounds));
        }
    }
}

// March spec: uniform styling distinct from neighbouring rows is a band;
// global default styling by itself is not evidence of a status band.
#[test]
fn contrasting_style_bands() {
    let base = grid(&["abc", "def", "ghi"]);
    assert!(detect_bands(&base).is_empty());
    let mut cells = base.cells().to_vec();
    for cell in &mut cells[3..6] {
        cell.style.inverse = true;
    }
    let t = analyze(&ScreenGrid::new(3, 3, cells, vec![RowMetadata::default(); 3], Cursor::default(), 0).unwrap());
    assert_eq!(t.select("band[kind=styled] > row[ index = 1 ]").unwrap().len(), 1);
    assert_eq!(t.select("span[inverse]").unwrap()[0].text, "def");
}

// #23: regions narrower than the screen do not duplicate or partially own a
// full-width physical row; their text and coordinates still describe the box.
#[test]
fn partial_and_nested_boxes_keep_rows_unique() {
    let t = analyze(&grid(&["┌───────┐", "│┌───┐  │", "││abc│  │", "│└───┘  │", "└───────┘"]));
    assert_eq!(t.select("box").unwrap().len(), 2);
    assert_eq!(t.select("box > box").unwrap().len(), 1);
    assert_eq!(t.select("row").unwrap().len(), 5);
    assert_eq!(t.select("box:nth(2)").unwrap()[0].bounds.col, 1);
    assert_eq!(t.select("box:nth(2)").unwrap()[0].text, "┌───┐\n│abc│\n└───┘");
}

// #313 extensibility: roles and arbitrary element/attribute names participate
// in the same selector language; adding them preserves a valid acyclic tree.
#[test]
fn recognizers_extend_the_tree_without_grammar_changes() {
    let mut t = analyze(&grid(&["draft probe"]));
    let row = Selector::parse("row").unwrap().evaluate(&t)[0];
    t.node_mut(row).unwrap().roles.insert("composer".into());
    t.node_mut(row).unwrap().attributes.insert("value".into(), "draft probe".into());
    let mut status = Node::new("status", Rect { col: 0, row: 0, width: 2, height: 1 }, "OK");
    status.attributes.insert("state".into(), "idle".into());
    t.add_node(t.root(), status).unwrap();
    assert_eq!(t.select("composer[value='draft probe'] > span").unwrap().len(), 1);
    assert_eq!(t.select("status[state=idle]").unwrap()[0].text, "OK");
    assert!(t.add_node(NodeId(999), Node::new("dialog", Rect::default(), "")).is_err());
    assert!(t.add_node(row, Node::new("dialog", Rect { col: 11, row: 0, width: 1, height: 1 }, "")).is_err());
}

// #23/xa11y: exact comparisons are case-sensitive; substring comparisons use
// Unicode lowercase. Nested predicates, child/descendant relations, regexes,
// quoted delimiters and selector groups retain their intended scope.
#[test]
fn selector_grammar_and_predicate_combinations() {
    let t = analyze(&grid(&["Alpha 42", "beta, ]>"]));
    for selector in [
        "row[text^='ALPHA']",
        "row[text*='PHA']",
        "row[text$='42']",
        "row:has-text('Alpha')",
        r"row:matches(/Alpha\s+\d+/)",
        "screen > row:nth(1)",
        "screen:has(row:has(span[text^=Alpha])) > row:nth(1)",
    ] {
        assert_eq!(t.select(selector).unwrap()[0].bounds.row, 0, "{selector}");
    }
    assert!(t.select("row[text='alpha 42']").unwrap().is_empty());
    assert_eq!(t.select("row[text='beta, ]>']").unwrap()[0].bounds.row, 1);
    assert_eq!(t.select("row:not(:has-text('Alpha'))").unwrap()[0].bounds.row, 1);
    assert_eq!(t.select("row, row:nth(1), span").unwrap().len(), 4);
    assert_eq!(t.select("row:not(screen > row:nth(2))").unwrap()[0].bounds.row, 0);
    assert!(t.select("screen:has(screen)").unwrap().is_empty());
    assert_eq!(t.select("* > span").unwrap().len(), 2);
}

// Selectors reject malformed and excessively nested input before evaluation.
// Generate unterminated/misplaced delimiters, bad indices, unknown predicates,
// invalid regexes and empty alternatives; Unicode is handled without panic.
#[test]
fn invalid_selectors_are_errors() {
    for bad in [
        "",
        " ",
        "row >",
        "> row",
        "row,,span",
        "row,",
        "row[",
        "row[]",
        "row[text~=a]",
        "row[text='a]",
        "row:nth(0)",
        "row:nth(-1)",
        "row:nth(999999999999999999999999999999)",
        "row:not()",
        "row:has()",
        "row:unknown(a)",
        "row:last()",
        "row:matches(/[a/)",
        "row:matches(/a)",
        "**",
        "*row",
        "row+span",
        "row)",
        "界",
        "row[界]",
    ] {
        assert!(Selector::parse(bad).is_err(), "accepted {bad:?}");
    }
    let nested = format!("{}span{}", "row:has(".repeat(33), ")".repeat(33));
    assert!(Selector::parse(&nested).is_err());
    assert!(Selector::parse(&"a".repeat(16_385)).is_err());
    let t = analyze(&grid(&["a/b (ok) '界'"]));
    assert_eq!(t.select(r"row:matches(/a\/b \(ok\)/)").unwrap().len(), 1);
    assert_eq!(t.select(r"row:has-text('\'界\'')").unwrap().len(), 1);
}

// Grid ownership requires exactly rows*cols cells and one metadata record per
// row. Empty dimensions are valid; a visible cursor must have valid geometry.
#[test]
fn empty_and_invalid_grids() {
    let t = analyze(&grid(&[]));
    assert_eq!(t.select("screen").unwrap().len(), 1);
    assert!(t.select("row, span, cursor, band, box").unwrap().is_empty());
    assert_eq!(analyze(&grid(&["", ""])).select("row").unwrap().len(), 2);
    assert!(ScreenGrid::new(1, 1, vec![], vec![RowMetadata::default()], Cursor::default(), 0).is_err());
    assert!(ScreenGrid::new(1, 1, vec![Cell::default()], vec![], Cursor::default(), 0).is_err());
    assert!(ScreenGrid::new(0, 0, vec![], vec![], Cursor { visible: true, ..Cursor::default() }, 0).is_err());
    let g = grid(&["abc"]);
    assert!(g.cell(3, 0).is_none());
    assert!(g.cell(0, 1).is_none());
    assert!(g.row(1).is_none());
}
