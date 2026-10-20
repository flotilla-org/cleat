//! Codex-specific extraction through the recognizer trait escape hatch.
//! The stylesheet still owns role and state declarations. This adapter only
//! supplies a cursor-anchored, style-filtered input node to its selectors.
use unicode_width::UnicodeWidthStr;

use super::*;

pub(super) struct Codex(pub Rules);
impl Recognizer for Codex {
    fn id(&self) -> &str {
        &self.0.id
    }
    fn revision(&self) -> u64 {
        self.0.revision
    }
    fn evaluate(&mut self, tree: &ScreenTree, title: Option<&str>) -> Result<Observation, String> {
        let mut input = tree.clone();
        if let Some(node) = composer(tree)? {
            input.add_node(input.root(), node).map_err(|e| e.to_string())?;
        }
        self.0.evaluate(&input, title)
    }
}

fn composer(tree: &ScreenTree) -> Result<Option<Node>, String> {
    let Some(cursor) = tree.select("cursor").map_err(|e| e.to_string())?.first().copied() else { return Ok(None) };
    let mut spans = tree.select("span").map_err(|e| e.to_string())?;
    spans.sort_by_key(|n| (n.bounds.row, n.bounds.col));
    // The last bold marker preceding the cursor excludes submitted history.
    let Some(marker) = spans
        .iter()
        .filter(|n| n.attribute("bold") == Some("true") && n.text.trim() == "›" && n.bounds.col == 0 && n.bounds.row <= cursor.bounds.row)
        .max_by_key(|n| n.bounds.row)
        .copied()
    else {
        return Ok(None);
    };
    let blocks = tree.select("block").map_err(|e| e.to_string())?;
    let block_end = blocks
        .iter()
        .filter(|n| n.bounds.contains(marker.bounds))
        .min_by_key(|n| u32::from(n.bounds.width) * u32::from(n.bounds.height))
        .map_or(cursor.bounds.row + 1, |n| n.bounds.row + n.bounds.height)
        .max(cursor.bounds.row + 1);
    let start = marker.bounds.row;
    let mut lines = Vec::new();
    for row in start..block_end {
        let mut line = String::new();
        for span in spans.iter().filter(|n| n.bounds.row == row && n.attribute("faint") != Some("true")) {
            // The fixed prefix is two single-width characters: the marker and
            // its space separator. Skip only those characters, not draft bytes.
            let skip = 2u16.saturating_sub(span.bounds.col) as usize;
            line.extend(span.text.chars().skip(skip));
        }
        let keep = if row == cursor.bounds.row { cursor.bounds.col.saturating_sub(2) as usize } else { 0 };
        let trimmed = line.trim_end_matches(' ');
        let padding = keep.saturating_sub(trimmed.width());
        line = format!("{trimmed}{}", " ".repeat(padding));
        lines.push(line);
    }
    let root = tree.node(tree.root()).ok_or("Codex input tree has no root")?;
    let parent = marker.parent().ok_or("Codex marker has no producer parent")?;
    let mut node = Node::new(
        "codex-input",
        terminal_screen::Rect { col: 0, row: start, width: root.bounds.width, height: block_end - start },
        lines.join("\n"),
    );
    node.attributes.insert("provenance".into(), provenance(tree, parent));
    Ok(Some(node))
}

#[cfg(all(test, feature = "ghostty-vt"))]
mod tests {
    use super::*;
    use crate::vt::{ghostty::GhosttyVtEngine, VtEngine};

    // #352: wording is never evidence of emptiness. Generate both intensities,
    // both marker weights and cursor visibility over empty, spaces, ordinary
    // and placeholder-prefixed drafts, with the cursor at either end.
    #[test]
    fn style_and_cursor_determine_composer() {
        for text in ["", "   ", "hello", "Ask Codex to do anything with this draft", "rotating placeholder", "漢字", "café"] {
            for faint in [false, true] {
                for bold in [false, true] {
                    for visible in [false, true] {
                        for left in [false, true] {
                            let mut vt = GhosttyVtEngine::new(80, 8);
                            let cursor = if left || faint { 3 } else { text.width() + 3 };
                            vt.feed(
                                format!(
                                    "\x1b[2J\x1b[H{}›\x1b[22m {}{text}\x1b[0m\x1b[1;{cursor}H\x1b[?25{}",
                                    if bold { "\x1b[1m" } else { "" },
                                    if faint { "\x1b[2m" } else { "" },
                                    if visible { "h" } else { "l" }
                                )
                                .as_bytes(),
                            )
                            .unwrap();
                            let tree = vt.recognition_tree(0).unwrap();
                            let actual = composer(&tree).unwrap();
                            if !bold || !visible {
                                assert!(actual.is_none());
                            } else {
                                // A cursor-left run of spaces carries no observable
                                // length evidence; only the visible value is observable.
                                let expected = if faint || (left && text.trim().is_empty()) { "" } else { text };
                                assert_eq!(actual.unwrap().text, expected, "text={text:?}, faint={faint}, left={left}");
                            }
                        }
                    }
                }
            }
        }
    }

    // A multiline draft includes rows after the cursor as well as before it;
    // the producer block, rather than cursor row, bounds the value.
    #[test]
    fn multiline_draft_is_independent_of_cursor_row() {
        for row in [1, 2] {
            let mut vt = GhosttyVtEngine::new(80, 8);
            vt.feed(format!("\x1b[2J\x1b[H\x1b[1m›\x1b[22m first\r\n  second\x1b[{row};3H\x1b[?25h").as_bytes()).unwrap();
            let tree = vt.recognition_tree(0).unwrap();
            assert_eq!(composer(&tree).unwrap().unwrap().text, "first\nsecond");
        }
    }

    // #2927 plain captures carry no style/cursor evidence. They can establish
    // working, but must never invent an empty composer from placeholder words.
    #[test]
    fn plain_2927_captures_do_not_invent_composer() {
        for (capture, working) in [
            (include_str!("../../../../recognizers/fixtures/codex/codex-2927/scrolled-back.txt"), false),
            (include_str!("../../../../recognizers/fixtures/codex/codex-2927/working-after-release.txt"), true),
        ] {
            let mut vt = GhosttyVtEngine::new(184, 40);
            vt.feed(capture.replace('\n', "\r\n").as_bytes()).unwrap();
            let tree = vt.recognition_tree(0).unwrap();
            let mut recognizer =
                Rules::parse(Path::new("codex.toml"), include_str!("../../../../recognizers/codex.toml")).unwrap().into_recognizer();
            let observation = recognizer.evaluate(&tree, None).unwrap();
            assert!(!observation.roles.iter().any(|r| r.role == "composer"));
            assert_eq!(observation.state, working.then_some(ProgramState::Working));
        }
    }
}
