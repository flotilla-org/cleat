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

// Match positions are one-based across all matching row slices in preorder.
#[test]
fn positions_are_match_indices_in_document_order() {
    for rows in 1..=8 {
        let g = grid(&vec!["line"; rows]);
        let t = analyze(&g);
        for index in 1..=rows {
            let selected = t.select(&format!("row:nth-match({index})")).unwrap();
            assert_eq!(selected.len(), 1);
            assert_eq!(selected[0].bounds.row, (index - 1) as u16);
        }
        assert!(t.select(&format!("row:nth-match({})", rows + 1)).unwrap().is_empty());
        assert_eq!(t.select("row:last-match").unwrap()[0].bounds.row, rows as u16 - 1);
        assert_eq!(t.select("row:last-match").unwrap()[0].attribute("index-from-bottom"), Some("0"));
    }
    let t = analyze(&grid(&["a", "", "z"]));
    assert_eq!(t.select("row:last-match").unwrap()[0].text, "z");
    assert_eq!(t.select("row:not([text=z]):last-match").unwrap()[0].bounds.row, 1);
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
    assert_eq!(t.select("row[soft-wrap] span").unwrap().iter().map(|n| n.text.as_str()).collect::<String>(), "e\u{301}界");
    assert_eq!(t.select("row[wrap-continuation] span").unwrap().iter().map(|n| n.text.as_str()).collect::<String>(), "👩‍💻  ");
    assert_eq!(t.select("span").unwrap()[0].bounds.width, 4);
    assert!(t.select("row[prompt]").unwrap().iter().all(|n| n.attribute("semantic-prompt") != Some("none")));
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

// #313 extensibility: roles and arbitrary element/attribute names participate
// in the same selector language; adding them preserves a valid acyclic tree.
#[test]
fn recognizers_extend_the_tree_without_grammar_changes() {
    let mut t = analyze(&grid(&["draft probe"]));
    let row = Selector::parse("row").unwrap().evaluate(&t)[0];
    t.annotations_mut(row).unwrap().roles.insert("composer".into());
    t.annotations_mut(row).unwrap().attributes.insert("value".into(), "draft probe".into());
    let mut status = Node::new("status", Rect { col: 0, row: 0, width: 2, height: 1 }, "OK");
    status.attributes.insert("state".into(), "idle".into());
    t.add_node(t.root(), status).unwrap();
    assert!(!t.select("composer[value='draft probe'] > span").unwrap().is_empty());
    assert_eq!(t.select("status[state=idle]").unwrap()[0].text, "OK");
    assert!(t.add_node(NodeId(999), Node::new("dialog", Rect::default(), "")).is_err());
    assert!(t.annotations_mut(NodeId(999)).is_none());
    assert!(t.add_node(t.root(), t.node(row).unwrap().clone()).is_err());
    assert!(t.add_node(row, Node::new("dialog", Rect { col: 11, row: 0, width: 1, height: 1 }, "")).is_err());
}

// Comparisons are case-sensitive unless the explicit i flag opts in. Nested predicates, child/descendant relations, regexes,
// quoted delimiters and selector groups retain their intended scope.
#[test]
fn selector_grammar_and_predicate_combinations() {
    let t = analyze(&grid(&["Alpha 42", "beta, ]>"]));
    for selector in [
        "row[text^='ALPHA' i]",
        "row[text*='PHA' i]",
        "row[text$='42']",
        "row:has-text('Alpha')",
        r"row:matches(/Alpha\s+\d+/)",
        "screen row:nth-match(1)",
        "screen:has(row:has(span[text^=Alpha])) row:nth-match(1)",
    ] {
        assert_eq!(t.select(selector).unwrap()[0].bounds.row, 0, "{selector}");
    }
    assert!(t.select("row[text='alpha 42']").unwrap().is_empty());
    assert_eq!(t.select("row[text='beta, ]>']").unwrap()[0].bounds.row, 1);
    assert_eq!(t.select("row:not(:has-text('Alpha'))").unwrap()[0].bounds.row, 1);
    assert_eq!(t.select("row, row:nth-match(1), span").unwrap().len(), 4);
    assert!(Selector::parse("row:not(screen row:nth-match(2))").is_err());
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
        "row:nth-match(0)",
        "row:nth-match(-1)",
        "row:nth-match(999999999999999999999999999999)",
        "row:not()",
        "row:has()",
        "row:unknown(a)",
        "row:last-match()",
        "row:matches(/[a/)",
        "row:matches(/a)",
        "**",
        "*row",
        "row++span",
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
    assert_eq!(analyze(&grid(&["", ""])).select("row").unwrap().len(), 0);
    assert!(ScreenGrid::new(1, 1, vec![], vec![RowMetadata::default()], Cursor::default(), 0).is_err());
    assert!(ScreenGrid::new(1, 1, vec![Cell::default()], vec![], Cursor::default(), 0).is_err());
    assert!(ScreenGrid::new(0, 0, vec![], vec![], Cursor { visible: true, ..Cursor::default() }, 0).is_err());
    let g = grid(&["abc"]);
    assert!(g.cell(3, 0).is_none());
    assert!(g.cell(0, 1).is_none());
    assert!(g.row(1).is_none());
}

// The parser contract bounds the input and selector levels (including the root),
// rejects over-budget compiled regexes, and locates an invalid nth at its number.
#[test]
fn parser_budget_boundaries_and_error_offsets() {
    assert!(Selector::parse(&"a".repeat(16_384)).is_ok());
    let size_error = Selector::parse(&"a".repeat(16_385)).unwrap_err();
    assert_eq!(size_error.offset, 0);
    assert!(size_error.message.contains("16384"));
    let nested = |levels| format!("{}span{}", "row:has(".repeat(levels), ")".repeat(levels));
    assert!(Selector::parse(&nested(31)).is_ok());
    let depth_error = Selector::parse(&nested(32)).unwrap_err();
    assert!(depth_error.message.contains("nesting exceeds 32"));
    let regex_error = Selector::parse("row:matches(/a{100000}/)").unwrap_err();
    assert!(regex_error.message.starts_with("invalid regex:"));
    assert!(regex_error.message.to_lowercase().contains("size limit"), "{regex_error}");
    assert_eq!(Selector::parse("row:nth-match(0)").unwrap_err().offset, 14);
}

// The README defines global negation, predicate-before-position ordering, and
// group unions in document order. These remain true inside scoped :has queries.
#[test]
fn scoped_negation_positions_and_group_order() {
    let t = analyze(&grid(&["a", "x", "x"]));
    assert!(t.select("screen:has(row:not(screen row))").unwrap().is_empty());
    assert_eq!(t.select("screen:has(row:nth-match(1):has-text('x'))").unwrap().len(), 1);
    assert_eq!(t.select("row:nth-match(1):has-text('x')").unwrap()[0].bounds.row, 1);
    assert_eq!(t.select("row:has-text('x'):nth-match(1)").unwrap()[0].bounds.row, 1);
    assert_eq!(t.select("row:nth-match(2):last-match").unwrap()[0].bounds.row, 1);
    assert_eq!(t.select("row:last-match:nth-match(1)").unwrap()[0].bounds.row, 2);
    assert!(t.select("row:last-match:nth-match(2)").unwrap().is_empty());
    let ids = Selector::parse("span, row:nth-match(1), row, span:nth-match(1)").unwrap().evaluate(&t);
    assert_eq!(ids, Selector::parse("row, span").unwrap().evaluate(&t));
    let bounds = ids
        .iter()
        .map(|id| {
            let n = t.node(*id).unwrap();
            (n.element.as_str(), n.bounds.row)
        })
        .collect::<Vec<_>>();
    assert_eq!(bounds, vec![("row", 0), ("span", 0), ("row", 1), ("span", 1), ("row", 2), ("span", 2)]);
}

// Unquoted values intentionally accept ASCII name tokens, including negative
// integers. Punctuation such as decimal points requires quotes; case semantics
// use the same explicit case flag for every operator, including Unicode.
#[test]
fn attribute_value_tokens_and_unicode_case() {
    let mut t = analyze(&grid(&["ÉCOLE"]));
    t.annotations_mut(t.root()).unwrap().attributes.insert("x".into(), "-1".into());
    t.annotations_mut(t.root()).unwrap().attributes.insert("version".into(), "1.2".into());
    assert_eq!(t.select("screen[x=-1]").unwrap().len(), 1);
    assert!(Selector::parse("screen[version=1.2]").is_err());
    assert_eq!(t.select("screen[version='1.2']").unwrap().len(), 1);
    for selector in ["row[text^='é' i]", "row[text*='éco' i]", "row[text$=cole i]"] {
        assert_eq!(t.select(selector).unwrap().len(), 1);
    }
    assert!(t.select("row[text='école']").unwrap().is_empty());
    assert_eq!(t.select("row[text='ÉCOLE']").unwrap().len(), 1);
}

fn span_heavy_grid(cols: u16, rows: u16) -> ScreenGrid {
    // Generate a style boundary at every column, and put the cursor in the
    // bottom row: this covers maximal span count and the last subtree boundary.
    let cells = (0..u32::from(cols) * u32::from(rows))
        .map(|i| Cell { grapheme: "x".into(), style: Style { faint: i % 2 == 0, ..Style::default() }, ..Cell::default() })
        .collect();
    ScreenGrid::new(
        cols,
        rows,
        cells,
        vec![RowMetadata::default(); usize::from(rows)],
        Cursor { col: cols - 1, row: rows - 1, visible: true, ..Cursor::default() },
        0,
    )
    .unwrap()
}

// #24's per-frame queries must preserve selection on span-heavy screens and
// after role nodes are attached in an allocation order differing from preorder.
#[test]
fn span_heavy_scoped_queries_and_appended_roles() {
    for (cols, rows) in [(2, 1), (20, 5), (200, 50)] {
        let mut t = analyze(&span_heavy_grid(cols, rows));
        let row = Selector::parse("row:last-match").unwrap().evaluate(&t)[0];
        t.add_node(row, Node::new("composer", Rect { col: 0, row: rows - 1, width: cols, height: 1 }, "draft")).unwrap();
        let matches = t.select("row:has(cursor) span:not([faint])").unwrap();
        assert_eq!(matches.len(), usize::from(cols) / 2);
        assert!(matches.iter().all(|node| node.bounds.row == rows - 1));
        assert_eq!(t.select("row:has(> composer) > composer").unwrap().len(), 1);
        assert_eq!(t.select("screen:has(row:has(cursor):not([soft-wrap])) composer").unwrap().len(), 1);
    }
}

// Diagnostic timing uses the same public fixture as the behavior test above;
// no wall-clock threshold affects correctness. Invoke explicitly with nocapture.
#[test]
#[ignore = "diagnostic selector timing, run explicitly with --ignored --nocapture"]
fn selector_timing_span_heavy() {
    let t = analyze(&span_heavy_grid(200, 50));
    for source in ["row:has(cursor) span:not([faint])", "row:has(span:not([faint])) > span:last-match"] {
        let selector = Selector::parse(source).unwrap();
        let start = std::time::Instant::now();
        for _ in 0..10 {
            assert!(!std::hint::black_box(selector.evaluate(&t)).is_empty());
        }
        eprintln!("{source}: {:.3} ms/query", start.elapsed().as_secs_f64() * 100.0);
    }
}

// The README promises inline regex flags such as (?i). Unicode-aware case
// matching must work for both ASCII and non-ASCII text; other flags remain usable.
#[test]
fn regex_inline_flags_include_unicode_case_matching() {
    let t = analyze(&grid(&["ÉCOLE", "Alpha"]));
    for selector in ["row:matches(/(?i)école/)", "row:matches(/(?i)alpha/)", "row:matches(/(?i-u)alpha/)"] {
        assert_eq!(t.select(selector).unwrap().len(), 1, "{selector}");
    }
    assert_eq!(t.select("screen:matches(/(?m)^Alpha$/)").unwrap().len(), 1);
    assert!(t.select("row:matches(/école/)").unwrap().is_empty());
}
