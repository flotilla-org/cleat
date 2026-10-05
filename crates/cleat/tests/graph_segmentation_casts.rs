#![cfg(feature = "ghostty-vt")]
//! SPIKE (#319): graph segmentation over the #312 recordings. Ignored; run
//! `GRAPH_OUT=dir cargo test -p cleat --test graph_segmentation_casts -- --ignored --nocapture`.

use std::fmt::Write as _;

use cleat::{
    provider::{DirtyState, TerminalCellFlags as Flags, TerminalCellWidth, TerminalCursorStyle},
    vt::{ghostty::GhosttyVtEngine, VtEngine},
};
use terminal_screen::{
    segment::{GraphSegmenter, RegionKind, RegionTree},
    *,
};

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

fn frames(cast: &str) -> Vec<(String, ScreenGrid)> {
    let mut lines = cast.lines();
    let header: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    let mut vt = GhosttyVtEngine::new(header["term"]["cols"].as_u64().unwrap() as u16, header["term"]["rows"].as_u64().unwrap() as u16);
    let events: Vec<serde_json::Value> = lines.map(|l| serde_json::from_str(l).unwrap()).collect();
    // Working checkpoint: midway through the output between the last submit and the next input.
    let inputs: Vec<usize> = events.iter().enumerate().filter(|(_, e)| e[1] == "i").map(|(i, _)| i).collect();
    let mut working = None;
    for w in inputs.windows(2) {
        if events[w[0]][2] == "\r" {
            working = Some((w[0] + w[1]) / 2);
        }
    }
    if let Some(&last) = inputs.last() {
        if events[last][2] == "\r" {
            working = Some((last + events.len()) / 2);
        }
    }
    let mut out = Vec::new();
    let mut generation = 0;
    for (i, event) in events.iter().enumerate() {
        if Some(i) == working {
            out.push(("working".to_string(), snapshot(&mut vt, generation)));
        }
        if event[1] == "i" {
            let input: String = event[2].as_str().unwrap().chars().take(24).collect();
            out.push((format!("before input {input:?}"), snapshot(&mut vt, generation)));
        }
        if event[1] == "o" {
            vt.feed(event[2].as_str().unwrap().as_bytes()).unwrap();
            generation += 1;
        }
    }
    out.push(("final".to_string(), snapshot(&mut vt, generation)));
    out
}

/// Cell the composer is anchored on: visible cursor, else the last 1-cell
/// inverse island (Claude paints its own cursor and hides the real one).
fn anchor(g: &ScreenGrid) -> Option<(u16, u16)> {
    if g.cursor.visible {
        return Some((g.cursor.col, g.cursor.row));
    }
    for r in (0..g.rows()).rev() {
        for c in 0..g.cols() {
            let cell = g.cell(c, r).unwrap();
            let left = c.checked_sub(1).and_then(|x| g.cell(x, r)).is_some_and(|x| x.style.inverse);
            let right = g.cell(c + 1, r).is_some_and(|x| x.style.inverse);
            if cell.style.inverse && !left && !right {
                return Some((c, r));
            }
        }
    }
    None
}

fn dump(name: &str, label: &str, g: &ScreenGrid, t: &RegionTree, out: &mut String) {
    let _ = writeln!(out, "##### {name} — {label} (gen {}) regions={}", g.generation, t.regions.len());
    let b = t.render_map(RegionKind::Block);
    let gr = t.render_map(RegionKind::Group);
    let z = t.render_map(RegionKind::Zone);
    let e = t.render_map(RegionKind::Enclosure);
    let last = (0..g.rows()).rev().find(|&r| !g.text(Rect { col: 0, row: r, width: g.cols(), height: 1 }).trim().is_empty()).unwrap_or(0);
    let _ = writeln!(out, "(per row: text / block / group / zone / enclosure maps; '#' frame, '.' gap)");
    for r in 0..=last {
        let text = g.text(Rect { col: 0, row: r, width: g.cols(), height: 1 });
        let r = usize::from(r);
        let _ = writeln!(out, "{r:2} {text}\n   {}\n   {}\n   {}\n   {}", b[r], gr[r], z[r], e[r]);
    }
    let _ = writeln!(out, "-- regions (non-leaf)");
    for (i, region) in t.regions.iter().enumerate() {
        if region.children.is_empty() {
            continue;
        }
        let kinds: Vec<&str> = region.kinds.iter().map(|k| k.as_str()).collect();
        let text: String = t.text(g, i).split_whitespace().collect::<Vec<_>>().join(" ").chars().take(60).collect();
        let _ = writeln!(
            out,
            "  #{i:<4} {:<22} {:?} conf={:.2} parent={:?} [{}] {text:?}",
            kinds.join("+"),
            (region.bounds.col, region.bounds.row, region.bounds.width, region.bounds.height),
            region.confidence,
            region.parent,
            region.evidence
        );
    }
}

#[test]
#[ignore = "spike dump; set GRAPH_OUT"]
fn graph_segmentation_dump_casts() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/design/semantic-prompt-evidence");
    let out_dir = std::env::var("GRAPH_OUT").ok();
    let mut seg = GraphSegmenter::default();
    // GRAPH_K enables the Felzenszwalb–Huttenlocher adaptive term at block level.
    if let Some(k) = std::env::var("GRAPH_K").ok().and_then(|k| k.parse().ok()) {
        seg.params.block_k = k;
    }
    for name in [
        "osc133-claude-ready-normal",
        "osc133-claude-ready-ax",
        "osc133-claude-wezterm",
        "osc133-codex-normal",
        "osc133-codex-inline",
        "osc133-shell",
    ] {
        let cast = std::fs::read_to_string(dir.join(format!("{name}.cast"))).unwrap();
        let mut out = String::new();
        println!("=== {name}");
        for (label, g) in frames(&cast) {
            let start = std::time::Instant::now();
            let t = seg.segment(&g);
            let us = start.elapsed().as_micros();
            dump(name, &label, &g, &t, &mut out);
            let boxes = analyze(&g).select("box").unwrap().len();
            let summary = match anchor(&g) {
                Some((c, r)) => {
                    let e = t.region_at(c, r, RegionKind::Enclosure).unwrap();
                    let z = t.region_at(c, r, RegionKind::Zone).unwrap();
                    let gq = t.region_at(c, r, RegionKind::Group).unwrap();
                    let leaf = t.leaf_at(c, r);
                    let b = t.ancestor(leaf, RegionKind::Block).or_else(|| t.ancestor(leaf, RegionKind::Gap)).unwrap();
                    let short = |i: usize| {
                        let s: String = t.text(&g, i).split_whitespace().collect::<Vec<_>>().join(" ");
                        let rb = t.regions[i].bounds;
                        format!(
                            "rows {}..{} conf {:.2} {:?}",
                            rb.row,
                            rb.row + rb.height,
                            t.regions[i].confidence,
                            s.chars().take(70).collect::<String>()
                        )
                    };
                    format!(
                        "anchor ({c},{r}) {}\n      enclosure {}\n      zone      {}\n      group     {}\n      block     {}",
                        if g.cursor.visible { "cursor" } else { "inverse-mark" },
                        short(e),
                        short(z),
                        short(gq),
                        short(b)
                    )
                }
                None => "no anchor".into(),
            };
            let enclosures = t.regions.iter().filter(|r| r.is(RegionKind::Enclosure)).count();
            let blocks = t.regions.iter().filter(|r| r.is(RegionKind::Block)).count();
            println!("  [{label}] {us}us(debug) enclosures={enclosures} blocks={blocks} #316-boxes={boxes}\n    {summary}");
        }
        if let Some(d) = &out_dir {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(std::path::Path::new(d).join(format!("{name}.graph.txt")), out).unwrap();
        }
    }
}
