#![cfg(feature = "ghostty-vt")]

use cleat::vt::{ghostty::GhosttyVtEngine, VtEngine};
use terminal_screen::*;
#[path = "support/screen_grid.rs"]
mod screen_grid;
use screen_grid::snapshot;

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
            assert_eq!(tree.select("block:has(cursor)").unwrap().len(), 1);
            assert!(!tree.select("block:has(cursor) span[faint]").unwrap().is_empty(), "placeholder style is present");
            assert_eq!(selected_text(&grid, "block:has(cursor) span:not([faint])"), "›");
            // The authorized acceptance query excludes the bold marker by style.
            assert_eq!(selected_text(&grid, "block:has(cursor) span:not([faint]):not([bold])"), "");
            empty_checked = true;
        }
        if event[1] == "o" {
            vt.feed(event[2].as_str().unwrap().as_bytes()).unwrap();
            generation += 1;
        }
    }
    assert!(empty_checked, "the recording must contain the named draft checkpoint");
    let grid = snapshot(&mut vt, generation);
    assert_eq!(selected_text(&grid, "block:has(cursor) span:not([faint])"), "› draft probe");
    assert_eq!(selected_text(&grid, "block:has(cursor) span:not([faint]):not([bold])"), "draft probe");
    assert_eq!(analyze(&grid).select("block:has(cursor)").unwrap()[0].bounds.row, 36);
}
