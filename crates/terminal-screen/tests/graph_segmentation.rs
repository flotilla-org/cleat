//! Graph segmentation contracts on synthetic panels, rules and styles.

use std::time::Instant;

use terminal_screen::{
    segment::{GraphSegmenter, RegionKind, RegionTree},
    Cell, Cursor, Rgb, RowMetadata, ScreenGrid, Style,
};

/// `style` uses one char per cell: `i` inverse, `t` grey bg tint, `g` green bg,
/// `f` faint, `B` bold, `d` dim fg (#999999); anything else is default.
fn grid(lines: &[&str], style: &[&str], cursor: Option<(u16, u16)>) -> ScreenGrid {
    let cols = lines.iter().map(|s| s.chars().count()).max().unwrap_or(0) as u16;
    let mut cells = Vec::new();
    for (r, line) in lines.iter().enumerate() {
        let sty: Vec<char> = style.get(r).map(|s| s.chars().collect()).unwrap_or_default();
        let mut row: Vec<Cell> = line.chars().map(|c| Cell { grapheme: c.to_string(), ..Cell::default() }).collect();
        row.resize(usize::from(cols), Cell::default());
        for (c, cell) in row.iter_mut().enumerate() {
            let mut s = Style { fg: Rgb(0xff, 0xff, 0xff), ..Style::default() };
            match sty.get(c) {
                Some('i') => s.inverse = true,
                Some('t') => s.bg = Rgb(0x37, 0x37, 0x37),
                Some('g') => s.bg = Rgb(0x00, 0x80, 0x00),
                Some('f') => s.faint = true,
                Some('B') => s.bold = true,
                Some('d') => s.fg = Rgb(0x99, 0x99, 0x99),
                _ => {}
            }
            cell.style = s;
        }
        cells.extend(row);
    }
    let cursor = cursor.map(|(col, row)| Cursor { col, row, visible: true, ..Cursor::default() }).unwrap_or_default();
    ScreenGrid::new(cols, lines.len() as u16, cells, vec![RowMetadata::default(); lines.len()], cursor, 1).unwrap()
}

fn show(name: &str, g: &ScreenGrid, t: &RegionTree) {
    println!("=== {name}  ({} regions)", t.regions.len());
    let text: Vec<String> = (0..g.rows()).map(|r| g.text(terminal_screen::Rect { col: 0, row: r, width: g.cols(), height: 1 })).collect();
    let b = t.render_map(RegionKind::Block);
    let gr = t.render_map(RegionKind::Group);
    let z = t.render_map(RegionKind::Zone);
    let e = t.render_map(RegionKind::Enclosure);
    println!("  {:w$} | {:w$} | {:w$} | {:w$} | enclosure", "text", "block", "group", "zone", w = usize::from(g.cols()));
    for r in 0..usize::from(g.rows()) {
        println!("  {} | {} | {} | {} | {}", text[r], b[r], gr[r], z[r], e[r]);
    }
}

fn seg(g: &ScreenGrid) -> RegionTree {
    GraphSegmenter::default().segment(g)
}

fn sel_text(g: &ScreenGrid, t: &RegionTree, selector: &str) -> Vec<String> {
    t.to_screen_tree(g).select(selector).unwrap().iter().map(|n| n.text.clone()).collect()
}

fn check_leaves_partition(t: &RegionTree) {
    let mut count = vec![0u32; usize::from(t.cols) * usize::from(t.rows)];
    for r in &t.regions {
        if r.children.is_empty() && !r.is(RegionKind::Screen) {
            assert!(r.is(RegionKind::RowSlice));
            assert_eq!(r.bounds.height, 1);
            for c in r.bounds.col..r.bounds.col + r.bounds.width {
                count[usize::from(r.bounds.row) * usize::from(t.cols) + usize::from(c)] += 1;
            }
        }
    }
    assert!(count.iter().all(|&c| c == 1), "every cell in exactly one leaf");
}

#[test]
fn inset_box_composer() {
    let lines = [
        "╭─ Edit ──────────────────────╮",
        "│ Notes about the change      │",
        "│ ╭─────────────────────────╮ │",
        "│ │ > draft probe           │ │",
        "│ ╰─────────────────────────╯ │",
        "╰─────────────────────────────╯",
    ];
    let g = grid(&lines, &[], Some((17, 3)));
    let t = seg(&g);
    show("inset box", &g, &t);
    check_leaves_partition(&t);
    let inner = t.region_at(5, 3, RegionKind::Enclosure).unwrap();
    let outer = t.region_at(3, 1, RegionKind::Enclosure).unwrap();
    assert_ne!(inner, outer);
    // The inner frame touches the outer bottom border, so both frames are one
    // wall component; the inner enclosure nests under the outer by surround rays.
    assert_eq!(t.regions[inner].parent, Some(outer));
    let innermost = "enclosure:has(cursor):not(:has(enclosure))";
    assert_eq!(sel_text(&g, &t, innermost), vec![" > draft probe           "]);
    println!("  {innermost} -> {:?}", sel_text(&g, &t, innermost));
    println!("  frame > enclosure -> {:?}", sel_text(&g, &t, "frame > enclosure"));
}

#[test]
fn lazygit_panels() {
    let lines = [
        "╭─ Files ──────╮╭─ Diff ─────────────────╮",
        "│ M src/a.rs   ││ @@ -1,3 +1,4 @@        │",
        "│ A src/b.rs   ││ -old line              │",
        "╰──────────────╯│ +new line              │",
        "╭─ Branches ───╮│ +another               │",
        "│ * main       ││                        │",
        "│   feature    ││                        │",
        "╰──────────────╯╰────────────────────────╯",
        "q: quit  enter: stage   ?: help           ",
    ];
    let style = ["", "", "", "", "", "  iiiiiiiiiii", "", "", ""];
    let g = grid(&lines, &style, None);
    let t = seg(&g);
    show("lazygit panels", &g, &t);
    check_leaves_partition(&t);
    let files = t.region_at(3, 1, RegionKind::Enclosure).unwrap();
    let branches = t.region_at(3, 5, RegionKind::Enclosure).unwrap();
    let diff = t.region_at(20, 2, RegionKind::Enclosure).unwrap();
    let footer = t.region_at(0, 8, RegionKind::Enclosure).unwrap();
    let all = [files, branches, diff, footer];
    for (i, a) in all.iter().enumerate() {
        for b in &all[i + 1..] {
            assert_ne!(a, b);
        }
    }
    println!("  enclosure:has-text('main') -> {:?}", sel_text(&g, &t, "enclosure:has-text('main')"));
    println!("  zone:has(span[inverse]) -> {:?}", sel_text(&g, &t, "zone:has(span[inverse])"));
}

#[test]
fn table_with_junctions() {
    let lines = [
        "┌──────┬───────┬─────┐",
        "│ name │ size  │ ok  │",
        "├──────┼───────┼─────┤",
        "│ a.rs │ 12 kB │ yes │",
        "│ b.rs │ 3 kB  │ no  │",
        "└──────┴───────┴─────┘",
        "+------+-------+",
        "| x    | y     |",
        "+------+-------+",
    ];
    let g = grid(&lines, &[], None);
    let t = seg(&g);
    show("table with junctions", &g, &t);
    check_leaves_partition(&t);
    assert_ne!(t.region_at(2, 1, RegionKind::Enclosure), t.region_at(9, 1, RegionKind::Enclosure));
    assert_ne!(t.region_at(2, 1, RegionKind::Enclosure), t.region_at(2, 3, RegionKind::Enclosure));
    assert_eq!(t.region_at(2, 3, RegionKind::Enclosure), t.region_at(2, 4, RegionKind::Enclosure));
    assert_ne!(t.region_at(2, 7, RegionKind::Enclosure), t.region_at(9, 7, RegionKind::Enclosure));
}

#[test]
fn tmux_split_and_status() {
    let lines = [
        "$ cargo build                 │top - 10:00 up 3 days      ",
        "   Compiling foo v0.1.0       │Tasks: 200 total           ",
        "    Finished dev              │                           ",
        "$ _                           │  PID USER   %CPU COMMAND  ",
        "                              │  123 rob    12.0 cargo    ",
        "                              │  456 rob     1.0 zsh      ",
        "[0] 0:zsh* 1:top-             \"host\" 10:00 05-Oct-26      ",
    ];
    let style = ["", "", "", "", "", "", "gggggggggggggggggggggggggggggggggggggggggggggggggggggggggggg"];
    let g = grid(&lines, &style, Some((2, 3)));
    let t = seg(&g);
    show("tmux split + status", &g, &t);
    check_leaves_partition(&t);
    let status = t.region_at(0, 6, RegionKind::Zone).unwrap();
    assert_ne!(t.region_at(0, 0, RegionKind::Zone), Some(status));
    // The divider stops at the status row, so both panes are one enclosure;
    // only the status bar's bg change splits them at zone level.
    assert_ne!(t.region_at(0, 0, RegionKind::Zone), t.region_at(35, 0, RegionKind::Zone));
    println!("  zone:has(cursor) -> {:?}", sel_text(&g, &t, "zone:has(cursor)"));
}

#[test]
fn menu_highlight() {
    let lines = ["┌─Menu─────┐", "│ apple    │", "│ banana   │", "│ cherry   │", "└──────────┘"];
    let style = ["", "", " iiiiiiiiii", "", ""];
    let g = grid(&lines, &style, None);
    let t = seg(&g);
    show("menu highlight", &g, &t);
    check_leaves_partition(&t);
    let sel = sel_text(&g, &t, "zone:has(span[inverse])");
    println!("  zone:has(span[inverse]) -> {sel:?}");
    assert_eq!(sel, vec![" banana   "]);
}

#[test]
fn claude_like_and_codex_like() {
    let claude = [
        "⏺ I updated the file and ran the tests.     ",
        "  All 12 passed.                             ",
        "                                             ",
        "✻ Cooked for 2s                              ",
        "─────────────────────────────────────────────",
        "❯ draft probe                                ",
        "─────────────────────────────────────────────",
        "  ⏵⏵ auto mode on (shift+tab to cycle)       ",
    ];
    let cstyle = ["", "dddddddddddddddd", "", "dddddddddddddddd", "", "             i", "", ""];
    let g = grid(&claude, &cstyle, None);
    let t = seg(&g);
    show("claude-like", &g, &t);
    check_leaves_partition(&t);
    println!("  enclosure:has(span[inverse]) -> {:?}", sel_text(&g, &t, "enclosure:has(span[inverse])"));
    let codex = [
        "• Ran cargo test                                  ",
        "  └ ok                                            ",
        "                                                  ",
        "• Working (3s • esc to interrupt)                 ",
        "                                                  ",
        "› draft probe                                     ",
        "                                                  ",
        "  GPT-6.1 default · /tmp/x                        ",
        "                               ⚠ 1 warning · f2   ",
    ];
    let xstyle = ["B", "", "", "Bffffffff", "", "B", "", "ffffffffffffffffffffffff", ""];
    let g = grid(&codex, &xstyle, Some((13, 5)));
    let t = seg(&g);
    show("codex-like", &g, &t);
    check_leaves_partition(&t);
    assert_eq!(sel_text(&g, &t, "block:has(cursor)"), vec!["› draft probe "]);
    println!("  block:has(cursor) -> {:?}", sel_text(&g, &t, "block:has(cursor)"));
    println!("  group:has(cursor) -> {:?}", sel_text(&g, &t, "group:has(cursor)"));
}

fn filled(cols: u16, rows: u16, f: impl Fn(u16, u16) -> (char, Style)) -> ScreenGrid {
    let mut cells = Vec::new();
    for r in 0..rows {
        for c in 0..cols {
            let (ch, style) = f(c, r);
            cells.push(Cell { grapheme: ch.to_string(), style, ..Cell::default() });
        }
    }
    ScreenGrid::new(cols, rows, cells, vec![RowMetadata::default(); usize::from(rows)], Cursor::default(), 1).unwrap()
}

type Painter = Box<dyn Fn(u16, u16) -> (char, Style)>;

#[test]
#[ignore = "timing; run with --release -- --ignored --nocapture"]
fn timing() {
    let prose = "the quick brown fox jumps over the lazy dog   status: ok    ";
    let cases: Vec<(&str, Painter)> = vec![
        (
            "prose + blank rows",
            Box::new(|c, r| {
                if r % 4 == 3 {
                    (' ', Style::default())
                } else {
                    (prose.chars().nth(usize::from(c + r * 7) % prose.len()).unwrap(), Style::default())
                }
            }),
        ),
        ("all '+'", Box::new(|_, _| ('+', Style::default()))),
        ("all '─'", Box::new(|_, _| ('─', Style::default()))),
        (
            "ascii grid +-+|",
            Box::new(|c, r| {
                (
                    if r % 2 == 0 {
                        if c % 2 == 0 {
                            '+'
                        } else {
                            '-'
                        }
                    } else if c % 2 == 0 {
                        '|'
                    } else {
                        ' '
                    },
                    Style::default(),
                )
            }),
        ),
        (
            "every cell new fg/bg",
            Box::new(|c, r| ('x', Style { fg: Rgb(c as u8, r as u8, 0), bg: Rgb(0, c as u8, r as u8), ..Style::default() })),
        ),
        ("blank", Box::new(|_, _| (' ', Style::default()))),
    ];
    let seg = GraphSegmenter::default();
    for (cols, rows) in [(120u16, 40u16), (200, 50)] {
        for (name, f) in &cases {
            let g = filled(cols, rows, f);
            let iters = 50;
            let start = Instant::now();
            let mut regions = 0;
            for _ in 0..iters {
                regions = seg.segment(&g).regions.len();
            }
            let seg_us = start.elapsed().as_micros() as f64 / f64::from(iters);
            let t = seg.segment(&g);
            let start = Instant::now();
            for _ in 0..iters {
                std::hint::black_box(t.to_screen_tree(&g));
            }
            let tree_us = start.elapsed().as_micros() as f64 / f64::from(iters);
            let start = Instant::now();
            for _ in 0..iters {
                std::hint::black_box(terminal_screen::analyze(&g));
            }
            let analyze_us = start.elapsed().as_micros() as f64 / f64::from(iters);
            println!(
                "{cols}x{rows} {name:24} segment {seg_us:8.0} us  to_screen_tree {tree_us:8.0} us  full analysis {analyze_us:9.0} us  regions {regions}"
            );
        }
    }
}

// The producer contract partitions all cells for every generated layout,
// including empty dimensions, thin screens, colours, glyph walls and cursor edges.
#[test]
fn generated_partition_and_switchable_producer_contract() {
    use terminal_screen::segment::{Provenance, RegionProducer};
    for cols in [0, 1, 2, 7, 20] {
        for rows in [0, 1, 2, 5] {
            for pattern in 0..4 {
                let g = filled(cols, rows, |c, r| {
                    let ch = match pattern {
                        0 => '+',
                        1 => {
                            if c % 3 == 0 {
                                '│'
                            } else {
                                'x'
                            }
                        }
                        2 => {
                            if r % 2 == 0 {
                                ' '
                            } else {
                                'a'
                            }
                        }
                        _ => 'x',
                    };
                    (ch, Style { bg: Rgb((c % 2) as u8, (r % 2) as u8, 0), ..Style::default() })
                });
                let producer: &dyn RegionProducer = &GraphSegmenter::default();
                let t = producer.produce(&g, None);
                assert_eq!(producer.provenance(), Provenance::Graph);
                let rebuilt = RegionTree::new(&g, t.regions.clone()).unwrap();
                for r in 0..rows {
                    for c in 0..cols {
                        assert_eq!(t.leaf_at(c, r), rebuilt.leaf_at(c, r));
                    }
                }
                assert_eq!(t.regions[0].cell_count as usize, g.cells().len());
                assert!(t.regions.iter().all(|r| r.provenance == Provenance::Graph));
                let screen = t.to_screen_tree(&g);
                assert!(screen.select("box, band, line").unwrap().is_empty());
                let mut count = vec![0; g.cells().len()];
                for node in screen.select("span").unwrap() {
                    for c in node.bounds.col..node.bounds.col + node.bounds.width {
                        count[usize::from(node.bounds.row) * usize::from(cols) + usize::from(c)] += 1;
                    }
                }
                assert!(count.iter().all(|c| *c == 1));
                if !g.cells().is_empty() {
                    let leaf = t.leaf_at(0, 0);
                    let mut invalid = t.regions.clone();
                    invalid[leaf].cell_count += 1;
                    assert!(RegionTree::new(&g, invalid).is_err());
                    let mut invalid = t.regions.clone();
                    invalid[leaf].confidence = f32::NAN;
                    assert!(RegionTree::new(&g, invalid).is_err());
                    let mut invalid = t.regions.clone();
                    invalid[leaf].bounds.width = 0;
                    assert!(RegionTree::new(&g, invalid).is_err());
                }
            }
        }
    }
}

// Owner acceptance: the adversarial grid that triggered corner-search blowup
// must segment in less than 5ms per frame in an optimized build. Run explicitly
// to avoid making ordinary debug tests or shared CI scheduling a timing gate.
#[test]
#[ignore = "release acceptance: cargo test --release -p terminal-screen adversarial_plus_budget -- --ignored --nocapture"]
fn adversarial_plus_budget() {
    if cfg!(debug_assertions) {
        panic!("this budget is for release builds");
    }
    let g = filled(200, 50, |_, _| ('+', Style::default()));
    let producer = GraphSegmenter::default();
    for _ in 0..5 {
        std::hint::black_box(producer.segment(&g));
    }
    let start = Instant::now();
    for _ in 0..100 {
        std::hint::black_box(producer.segment(&g));
    }
    let per_frame = start.elapsed().as_secs_f64() / 100.0;
    eprintln!("200x50 '+' segmentation: {:.3}ms/frame", per_frame * 1000.0);
    assert!(per_frame < 0.005, "{per_frame}s exceeds 5ms");
}

// Alternate producers use the same validated partition and selector contract;
// provenance stays visible to consumers without changing selector grammar.
#[test]
fn declared_producer_uses_common_tree() {
    use terminal_screen::segment::{FrameHistory, Provenance, Region, RegionProducer};
    struct Declared;
    impl RegionProducer for Declared {
        fn provenance(&self) -> Provenance {
            Provenance::Declared
        }
        fn produce(&self, grid: &ScreenGrid, _history: Option<&FrameHistory<'_>>) -> RegionTree {
            let make = |kind, parent, children, bounds, count| Region {
                kinds: [kind].into(),
                bounds,
                cell_count: count,
                provenance: Provenance::Declared,
                confidence: 1.0,
                evidence: "producer fixture rectangle".into(),
                parent,
                children,
            };
            RegionTree::new(grid, vec![
                make(RegionKind::Screen, None, vec![1], grid.bounds(), 2),
                make(RegionKind::Block, Some(0), vec![2], grid.bounds(), 2),
                make(RegionKind::RowSlice, Some(1), vec![], grid.bounds(), 2),
            ])
            .unwrap()
        }
    }
    let g = grid(&["ok"], &[], None);
    let tree = Declared.produce(&g, None).to_screen_tree(&g);
    assert_eq!(tree.select("region[provenance=declared] > row > span").unwrap()[0].text, "ok");
    let mut invalid = GraphSegmenter::default().segment(&g).regions;
    let duplicate = invalid[0].children[0];
    invalid[0].children.push(duplicate);
    assert!(RegionTree::new(&g, invalid).is_err());
    use terminal_screen::segment::GraphParams;
    for params in
        [GraphParams { block_tau: 1.0, ..GraphParams::default() }, GraphParams { w_bg: f32::NAN, ..GraphParams::default() }, GraphParams {
            gutter_min: 0,
            ..GraphParams::default()
        }]
    {
        assert!(GraphSegmenter::new(params).is_err());
    }
    assert!(GraphSegmenter::new(GraphParams::default()).is_ok());
}
