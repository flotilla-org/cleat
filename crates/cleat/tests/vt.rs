#[cfg(feature = "ghostty-vt")]
use cleat::provider::{
    DirtyState, TerminalRenderUpdateOpKind, TerminalStyleColorTag, TerminalViewportKind, ViewportCommand, ViewportCommandOutcome,
    TERMINAL_IMAGE_PLACEMENT_VIRTUAL,
};
use cleat::vt::{passthrough::PassthroughVtEngine, ClientCapabilities, ColorLevel, VtEngine};
#[cfg(feature = "ghostty-vt")]
use cleat::vt::{MouseReportFormat, MouseTrackingMode};

mod vt_contracts;

#[cfg(feature = "ghostty-vt")]
use std::path::PathBuf;
#[cfg(all(feature = "ghostty-vt", any(target_os = "linux", target_os = "macos")))]
use std::process::Command;

use vt_contracts::{assert_non_replay_contract, assert_replay_contract_placeholder, PassthroughFixture, PlaceholderReplayFixture};
#[cfg(feature = "ghostty-vt")]
use vt_contracts::{assert_replay_contract, GhosttyFixture};

#[test]
fn vt_build_support_message_is_nonempty_and_matches_feature_state() {
    assert!(!cleat::vt::BUILD_SUPPORT_MESSAGE.is_empty());
    #[cfg(feature = "ghostty-vt")]
    assert!(cleat::vt::functional_vt_available());
    #[cfg(not(feature = "ghostty-vt"))]
    assert!(!cleat::vt::functional_vt_available());
}

#[test]
fn vt_passthrough_engine_contract_is_locked() {
    assert_non_replay_contract(&PassthroughFixture);
}

#[test]
fn vt_placeholder_replay_engine_contract_is_locked() {
    assert_replay_contract_placeholder(&PlaceholderReplayFixture);
}

#[test]
fn vt_passthrough_feed_changes_passthrough_local_state() {
    let mut engine = PassthroughVtEngine::new(80, 24);
    assert_eq!(engine.bytes_seen(), 0);

    engine.feed(b"\x1b[31mhello\x1b[0m").expect("feed bytes");
    engine.feed(b" world").expect("feed bytes");

    assert_eq!(engine.bytes_seen(), 20);
}

#[test]
fn vt_passthrough_replay_remains_disabled_for_client_capabilities() {
    let engine = PassthroughVtEngine::new(80, 24);
    let capabilities = ClientCapabilities::new(ColorLevel::TrueColor, true);

    assert_eq!(engine.replay_payload(&capabilities).expect("replay payload"), None);
}

#[test]
fn vt_passthrough_screen_text_is_unsupported() {
    let engine = PassthroughVtEngine::new(80, 24);

    let err = engine.screen_text().expect_err("passthrough should not capture text");

    assert!(err.contains("placeholder/test-only"));
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_engine_contract_is_locked() {
    assert_replay_contract(&GhosttyFixture);
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_formatter_alloc_round_trips_output() {
    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(80, 24);

    engine.feed(b"hello ghostty formatter").expect("feed bytes");

    let replay = engine
        .replay_payload(&ClientCapabilities::new(ColorLevel::TrueColor, false))
        .expect("replay payload")
        .expect("ghostty replay payload");

    let replay_text = String::from_utf8_lossy(&replay);
    assert!(replay_text.contains("hello ghostty formatter"), "unexpected replay payload: {replay_text}");
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_screen_activity_detects_coalesced_changes_but_not_queries() {
    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(80, 24);
    engine.screen_grid().expect("initialize render state");

    engine.feed(b"|").expect("draw initial spinner frame");
    assert_eq!(engine.screen_activity_changed().expect("initial spinner activity"), Some(true));

    engine.feed(b"\x1b[6n").expect("feed cursor-position query while render damage is deferred");
    assert_eq!(engine.screen_activity_changed().expect("query activity"), Some(false));

    engine.feed(b"\r/\r|").expect("draw a complete spinner cycle");
    assert_eq!(engine.screen_activity_changed().expect("coalesced spinner activity"), Some(true));
    let grid = engine.screen_grid().expect("consume deferred spinner activity");
    assert_eq!(grid.row_text(0).trim_end(), "|");
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_screen_text_round_trips_output() {
    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(80, 24);

    engine.feed(b"hello capture").expect("feed bytes");

    let text = engine.screen_text().expect("screen text");
    assert!(text.contains("hello capture"), "unexpected screen text: {text}");
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_blank_engine_does_not_emit_replay_payload() {
    let engine = cleat::vt::ghostty::GhosttyVtEngine::new(80, 24);

    let replay = engine.replay_payload(&ClientCapabilities::new(ColorLevel::TrueColor, false)).expect("replay payload");

    assert_eq!(replay, None, "blank ghostty engine should not emit replay");
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_screen_grid_returns_correct_dimensions() {
    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(40, 5);

    engine.feed(b"hello grid").expect("feed bytes");

    let grid = engine.screen_grid().expect("screen grid");
    assert_eq!(grid.cols, 40);
    assert_eq!(grid.rows, 5);
    assert_eq!(grid.cells.len(), 40 * 5);
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_screen_grid_captures_cell_text() {
    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(40, 5);

    engine.feed(b"Hello").expect("feed bytes");

    let grid = engine.screen_grid().expect("screen grid");
    let text: String = (0..5)
        .map(|col| {
            let cell = grid.cell(col, 0).unwrap();
            if cell.graphemes.is_empty() {
                ' '
            } else {
                char::from_u32(cell.graphemes[0]).unwrap_or('?')
            }
        })
        .collect();
    assert_eq!(text, "Hello");
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_screen_grid_captures_bold_style() {
    use cleat::vt::CellFlags;

    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(40, 5);

    engine.feed(b"\x1b[1mbold\x1b[0m plain").expect("feed bytes");

    let grid = engine.screen_grid().expect("screen grid");
    // 'b' at col 0 should be bold
    assert!(grid.cell(0, 0).unwrap().flags.contains(CellFlags::BOLD));
    // 'p' at col 5 (after "bold ") should not be bold
    assert!(!grid.cell(5, 0).unwrap().flags.contains(CellFlags::BOLD));
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_screen_grid_captures_cursor_position() {
    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(40, 5);

    engine.feed(b"Hello").expect("feed bytes");

    let grid = engine.screen_grid().expect("screen grid");
    assert!(grid.cursor.visible);
    assert_eq!(grid.cursor.col, 5);
    assert_eq!(grid.cursor.row, 0);
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_screen_grid_refreshes_clean_cached_cursor_position() {
    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(40, 5);

    engine.feed(b"Hello").expect("feed bytes");
    let initial = engine.screen_grid().expect("initial grid");
    assert_eq!(initial.cursor.col, 5);

    engine.feed(b"\x1b[2D").expect("move cursor left");
    let moved = engine.screen_grid().expect("moved grid");
    assert_eq!(moved.cursor.col, 3);

    let cached = engine.screen_grid().expect("cached grid");
    assert_eq!(cached.cursor.col, 3);
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_screen_grid_reports_cursor_blink_policy() {
    use cleat::vt::CursorStyle;

    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(40, 5);

    engine.feed(b"\x1b[5 q").expect("set blinking bar cursor");
    let blinking = engine.screen_grid().expect("blinking grid");
    assert_eq!(blinking.cursor.style, CursorStyle::Bar);
    assert!(blinking.cursor.blink);

    engine.feed(b"\x1b[6 q").expect("set steady bar cursor");
    let steady = engine.screen_grid().expect("steady grid");
    assert_eq!(steady.cursor.style, CursorStyle::Bar);
    assert!(!steady.cursor.blink);
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_screen_grid_row_text_returns_row_content() {
    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(10, 3);

    engine.feed(b"line one\r\nline two").expect("feed bytes");

    let grid = engine.screen_grid().expect("screen grid");
    assert_eq!(grid.row_text(0).trim_end(), "line one");
    assert_eq!(grid.row_text(1).trim_end(), "line two");
    assert_eq!(grid.row_text(2).trim_end(), "");
}

#[test]
fn vt_passthrough_screen_grid_returns_error() {
    let mut engine = cleat::vt::passthrough::PassthroughVtEngine::new(80, 24);
    let err = engine.screen_grid().expect_err("passthrough should fail");
    assert!(err.contains("placeholder/test-only"));
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_screen_grid_wide_chars_not_doubled_in_row_text() {
    use cleat::vt::CellWidth;

    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(20, 3);

    // CJK character 字 is a wide (2-column) glyph
    engine.feed("字ab".as_bytes()).expect("feed bytes");

    let grid = engine.screen_grid().expect("screen grid");

    // Col 0 should be the wide char, col 1 should be the spacer tail
    assert_eq!(grid.cell(0, 0).unwrap().width, CellWidth::Wide);
    assert_eq!(grid.cell(1, 0).unwrap().width, CellWidth::SpacerTail);
    assert_eq!(grid.cell(2, 0).unwrap().width, CellWidth::Narrow);

    // row_text should produce "字ab" not "字 ab"
    let text = grid.row_text(0);
    assert!(text.starts_with("字ab"), "expected row_text to start with '字ab', got: {text:?}");
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_screen_grid_returns_cached_when_clean() {
    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(20, 3);

    engine.feed(b"hello").expect("feed bytes");

    let grid1 = engine.screen_grid().expect("first screen_grid");
    assert_eq!(grid1.row_text(0).trim_end(), "hello");

    // Second call with no new input should return cached result
    let grid2 = engine.screen_grid().expect("second screen_grid (cached)");
    assert_eq!(grid1, grid2);
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_scrollbar_and_viewport_commands_track_scrollback() {
    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(20, 3);
    for line in 0..8 {
        engine.feed(format!("line {line}\r\n").as_bytes()).expect("feed line");
    }

    let bottom = engine.scrollbar_state().expect("initial scrollbar");
    assert_eq!(bottom.viewport_kind, TerminalViewportKind::LiveNormal);
    assert!(bottom.total_rows > bottom.viewport_rows as u64, "expected scrollback after feeding lines: {bottom:?}");
    assert!(bottom.at_bottom);

    assert_eq!(engine.scroll_viewport(ViewportCommand::DeltaRows(-2)).expect("scroll up"), ViewportCommandOutcome::Moved);
    let scrolled = engine.scrollbar_state().expect("scrolled scrollbar");
    assert_eq!(scrolled.viewport_kind, TerminalViewportKind::NormalScrollback);
    assert!(scrolled.viewport_top_row < bottom.viewport_top_row);
    assert!(!scrolled.at_bottom);

    assert_eq!(engine.scroll_viewport(ViewportCommand::Bottom).expect("scroll bottom"), ViewportCommandOutcome::Moved);
    let restored = engine.scrollbar_state().expect("restored scrollbar");
    assert_eq!(restored.viewport_kind, TerminalViewportKind::LiveNormal);
    assert!(restored.at_bottom);

    assert_eq!(engine.scroll_viewport(ViewportCommand::Bottom).expect("bottom no-op"), ViewportCommandOutcome::NoOp);
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_cursor_hidden_while_scrolled_out_of_viewport() {
    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(20, 3);
    for line in 0..8 {
        engine.feed(format!("line {line}\r\n").as_bytes()).expect("feed line");
    }

    let live = engine.screen_grid().expect("live grid");
    assert!(live.cursor.visible);
    let live_cursor = live.cursor;

    engine.scroll_viewport(ViewportCommand::Top).expect("scroll to top");
    let scrolled = engine.screen_grid().expect("scrolled grid");
    assert!(!scrolled.cursor.visible, "cursor scrolled out of the viewport must not be drawable: {:?}", scrolled.cursor);

    engine.scroll_viewport(ViewportCommand::Bottom).expect("scroll back to bottom");
    let restored = engine.screen_grid().expect("restored grid");
    assert_eq!(restored.cursor, live_cursor, "returning to the live viewport restores the cursor");
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_cursor_clamps_to_last_column_while_wrap_pending() {
    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(10, 3);

    engine.feed(b"0123456789").expect("fill first row exactly");
    let pending = engine.screen_grid().expect("wrap-pending grid");
    assert!(pending.cursor.visible);
    assert_eq!((pending.cursor.col, pending.cursor.row), (9, 0), "wrap-pending cursor reports the last column");

    engine.feed(b"a").expect("write past the wrap");
    let wrapped = engine.screen_grid().expect("wrapped grid");
    assert_eq!((wrapped.cursor.col, wrapped.cursor.row), (1, 1), "next write lands on the second row");
    assert_eq!(wrapped.row_text(1).trim_end(), "a");
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_cursor_reports_wide_tail_on_wide_glyph_tail_cell() {
    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(10, 3);

    engine.feed("字".as_bytes()).expect("feed wide glyph");
    let after = engine.screen_grid().expect("grid after wide glyph");
    assert_eq!(after.cursor.col, 2, "cursor advances past both cells of a wide glyph");
    assert!(!after.cursor.wide_tail);

    // Park the cursor on the tail (spacer) cell of the wide glyph (1-based col 2).
    engine.feed(b"\x1b[1;2H").expect("move onto tail cell");
    let tail = engine.screen_grid().expect("grid on tail cell");
    assert_eq!(tail.cursor.col, 1);
    assert!(tail.cursor.wide_tail, "cursor on the spacer tail of a wide glyph reports wide_tail");
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_screen_grid_updates_after_new_input() {
    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(20, 3);

    engine.feed(b"aaa").expect("feed bytes");
    let grid1 = engine.screen_grid().expect("first screen_grid");
    assert_eq!(grid1.row_text(0).trim_end(), "aaa");

    engine.feed(b"bbb").expect("feed more bytes");
    let grid2 = engine.screen_grid().expect("second screen_grid after new input");
    assert_eq!(grid2.row_text(0).trim_end(), "aaabbb");
    assert_ne!(grid1, grid2);
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_render_update_full_exposes_rows_and_structured_style() {
    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(20, 3);

    engine.feed(b"\x1b[38;2;10;20;30mX\x1b[0m").expect("feed truecolor text");

    let update = engine.render_update(DirtyState::Full).expect("render update");

    assert_eq!(update.dirty, DirtyState::Full);
    assert_eq!(update.ops.len(), 1);
    assert_eq!(update.ops[0].kind, TerminalRenderUpdateOpKind::FullVisibleReplace);
    assert_eq!(update.ops[0].rows.len(), 3);
    assert_eq!(update.ops[0].rows[0].row, 0);
    assert_eq!(update.ops[0].rows[0].col_count, 20);
    assert!(update.ops[0].rows[0].dirty);

    let cell = &update.ops[0].rows[0].cells[0];
    assert_eq!(cell.graphemes, vec!['X' as u32]);
    assert_eq!(cell.style.resolved_fg.r, 10);
    assert_eq!(cell.style.resolved_fg.g, 20);
    assert_eq!(cell.style.resolved_fg.b, 30);
    assert_eq!(cell.style.fg_color.tag, TerminalStyleColorTag::Rgb);
    assert!(cell.style.has_text);
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_render_update_exposes_mouse_tracking_level_and_format() {
    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(20, 3);

    let initial = engine.render_update(DirtyState::Full).expect("initial render update");
    assert_eq!(initial.terminal_modes.mouse_tracking_mode, MouseTrackingMode::None);
    assert_eq!(initial.terminal_modes.mouse_report_format, MouseReportFormat::Legacy);
    assert!(!initial.terminal_modes.mouse_tracking);

    engine.feed(b"\x1b[?1003h\x1b[?1016h").expect("enable any-event mouse and SGR-pixels");
    let update = engine.render_update(DirtyState::Partial).expect("mouse mode render update");
    assert_eq!(update.terminal_modes.mouse_tracking_mode, MouseTrackingMode::Any);
    assert_eq!(update.terminal_modes.mouse_report_format, MouseReportFormat::SgrPixels);
    assert!(update.terminal_modes.mouse_tracking);
    assert!(update.terminal_modes.mouse_sgr_pixels);
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_render_update_exposes_kitty_image_resource_and_placement() {
    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(20, 3);

    engine.feed(b"\x1b_Ga=T,t=d,f=24,i=1,p=1,s=1,v=2,c=10,r=1;////////\x1b\\").expect("feed kitty image");

    let update = engine.render_update(DirtyState::Full).expect("render update");

    assert_eq!(update.image_resources.len(), 1);
    let resource = &update.image_resources[0];
    assert_eq!(resource.image_id, 1);
    assert!(resource.generation > 0);
    assert_eq!(resource.width_px, 1);
    assert_eq!(resource.height_px, 2);
    assert_eq!(resource.format, 0);
    assert_eq!(resource.compression, 0);
    assert_eq!(resource.data_len, 6);

    assert_eq!(update.image_placements.len(), 1);
    let placement = &update.image_placements[0];
    assert_eq!(placement.image_id, resource.image_id);
    assert_eq!(placement.generation, resource.generation);
    assert_eq!(placement.placement_id, 1);
    assert_eq!(placement.flags, 0);
    assert_eq!(placement.viewport_col, 0);
    assert_eq!(placement.viewport_row, 0);
    assert_eq!(placement.source_width, 1);
    assert_eq!(placement.source_height, 2);

    let mut copied = Vec::new();
    let borrowed = engine
        .with_image_resource_data(resource.image_id, resource.generation, &mut |bytes| {
            copied.extend_from_slice(bytes);
            true
        })
        .expect("borrow image bytes");
    assert!(borrowed);
    assert_eq!(copied.len(), resource.data_len);
    assert_eq!(copied, vec![255; 6]);

    let stale = engine
        .with_image_resource_data(resource.image_id, resource.generation.saturating_add(1), &mut |_| true)
        .expect("stale image generation lookup");
    assert!(!stale);
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_render_update_exposes_kitty_virtual_image_placements() {
    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(10, 4);
    engine.set_cell_size(10, 10).expect("set cell size");

    let transmit = "\x1b_Ga=T,t=d,f=24,i=1,U=1,s=4,v=2,c=4,r=2;////////////////////////////////\x1b\\";
    engine.feed(transmit.as_bytes()).expect("feed virtual kitty image");

    let row0 = "\x1b[38;2;0;0;1m\
        \u{10EEEE}\u{0305}\u{0305}\
        \u{10EEEE}\u{0305}\u{030D}\
        \u{10EEEE}\u{0305}\u{030E}\
        \u{10EEEE}\u{0305}\u{0310}\
        \x1b[39m";
    let row1 = "\x1b[2;1H\x1b[38;2;0;0;1m\
        \u{10EEEE}\u{030D}\u{0305}\
        \u{10EEEE}\u{030D}\u{030D}\
        \u{10EEEE}\u{030D}\u{030E}\
        \u{10EEEE}\u{030D}\u{0310}\
        \x1b[39m";
    engine.feed(row0.as_bytes()).expect("feed first virtual row");
    engine.feed(row1.as_bytes()).expect("feed second virtual row");

    let update = engine.render_update(DirtyState::Full).expect("render update");

    assert_eq!(update.image_resources.len(), 1);
    let resource = &update.image_resources[0];
    assert_eq!(resource.image_id, 1);
    assert!(resource.generation > 0);
    assert_eq!(resource.width_px, 4);
    assert_eq!(resource.height_px, 2);
    assert_eq!(resource.data_len, 24);

    assert_eq!(update.image_placements.len(), 2);
    let first = &update.image_placements[0];
    assert_eq!(first.image_id, resource.image_id);
    assert_eq!(first.generation, resource.generation);
    assert_eq!(first.flags, TERMINAL_IMAGE_PLACEMENT_VIRTUAL);
    assert_eq!(first.viewport_col, 0);
    assert_eq!(first.viewport_row, 0);
    assert_eq!(first.grid_cols, 4);
    assert_eq!(first.grid_rows, 1);
    assert_eq!(first.pixel_width, 40);
    assert_eq!(first.pixel_height, 10);
    assert_eq!(first.source_x, 0);
    assert_eq!(first.source_y, 0);
    assert_eq!(first.source_width, 4);
    assert_eq!(first.source_height, 1);

    let second = &update.image_placements[1];
    assert_eq!(second.image_id, resource.image_id);
    assert_eq!(second.generation, resource.generation);
    assert_eq!(second.flags, TERMINAL_IMAGE_PLACEMENT_VIRTUAL);
    assert_eq!(second.viewport_col, 0);
    assert_eq!(second.viewport_row, 1);
    assert_eq!(second.source_y, 1);
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_kitty_image_placement_pixel_size_uses_cell_size() {
    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(20, 3);
    engine.set_cell_size(8, 16).expect("set cell size");

    engine.feed(b"\x1b_Ga=T,t=d,f=24,i=2,p=1,s=1,v=2,c=10,r=1;////////\x1b\\").expect("feed kitty image");

    let update = engine.render_update(DirtyState::Full).expect("render update");
    assert_eq!(update.image_placements.len(), 1);
    let placement = &update.image_placements[0];
    assert_eq!(placement.grid_cols, 10);
    assert_eq!(placement.grid_rows, 1);
    assert_eq!(placement.pixel_width, 80);
    assert_eq!(placement.pixel_height, 16);
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_kitty_png_image_decodes_to_rgba_resource() {
    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(20, 3);
    engine.set_cell_size(8, 16).expect("set cell size");

    engine
        .feed(
            b"\x1b_Ga=T,t=d,f=100,i=3,p=1;iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==\x1b\\",
        )
        .expect("feed png kitty image");

    let update = engine.render_update(DirtyState::Full).expect("render update");
    assert_eq!(update.image_resources.len(), 1);
    let resource = &update.image_resources[0];
    assert_eq!(resource.image_id, 3);
    assert_eq!(resource.width_px, 1);
    assert_eq!(resource.height_px, 1);
    assert_eq!(resource.format, 1);
    assert_eq!(resource.compression, 0);
    assert_eq!(resource.data_len, 4);
    assert_eq!(update.image_placements.len(), 1);

    let mut copied = Vec::new();
    assert!(engine
        .with_image_resource_data(resource.image_id, resource.generation, &mut |bytes| {
            copied.extend_from_slice(bytes);
            true
        })
        .expect("borrow decoded png bytes"));
    assert_eq!(copied.len(), 4);
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_render_update_partial_emits_row_replace_ops_for_dirty_rows() {
    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(20, 3);

    engine.feed(b"first").expect("feed initial text");
    let full = engine.render_update(DirtyState::Full).expect("initial render update");
    assert_eq!(full.ops[0].kind, TerminalRenderUpdateOpKind::FullVisibleReplace);

    engine.feed(b"\x1b[2;1Hsecond").expect("feed second row text");
    let partial = engine.render_update(DirtyState::Partial).expect("partial render update");

    assert_eq!(partial.dirty, DirtyState::Partial);
    assert!(partial.ops.len() < 3, "partial update should not replace the full visible screen");
    assert!(partial.ops.iter().all(|op| op.kind == TerminalRenderUpdateOpKind::RowReplace));
    let row_one = partial.ops.iter().find(|op| op.first_row == 1).expect("row containing new text should be dirty");
    assert_eq!(row_one.rows.len(), 1);
    assert_eq!(row_one.rows[0].row, 1);
    assert_eq!(row_one.rows[0].cells[0].graphemes, vec!['s' as u32]);
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_screen_grid_resolves_explicit_fg_and_bg_colors() {
    use cleat::vt::Rgb;

    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(40, 5);

    // True-color SGR: red fg on green bg for "R", then blue fg for "B", then reset
    engine.feed(b"\x1b[38;2;255;0;0m\x1b[48;2;0;255;0mR\x1b[38;2;0;0;255mB\x1b[0m").expect("feed SGR color sequence");

    let grid = engine.screen_grid().expect("screen grid");

    let r_cell = grid.cell(0, 0).unwrap();
    assert_eq!(r_cell.fg, Rgb { r: 255, g: 0, b: 0 }, "R cell foreground should be red");
    assert_eq!(r_cell.bg, Rgb { r: 0, g: 255, b: 0 }, "R cell background should be green");

    let b_cell = grid.cell(1, 0).unwrap();
    assert_eq!(b_cell.fg, Rgb { r: 0, g: 0, b: 255 }, "B cell foreground should be blue");
    assert_eq!(b_cell.bg, Rgb { r: 0, g: 255, b: 0 }, "B cell background should still be green");

    // A cell after reset should have default colors, matching an untouched cell
    let default_cell = grid.cell(2, 0).unwrap();
    let untouched_cell = grid.cell(39, 4).unwrap();
    assert_eq!(default_cell.fg, untouched_cell.fg, "post-reset fg should match untouched cell default");
    assert_eq!(default_cell.bg, untouched_cell.bg, "post-reset bg should match untouched cell default");
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_snapshot_and_render_update_resolve_background_only_cells() {
    use cleat::vt::Rgb;

    fn assert_background_only_cell(input: &[u8], expected: Rgb) {
        let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(4, 2);
        engine.feed(input).expect("erase row with configured background");

        let grid = engine.screen_grid().expect("screen grid");
        let grid_cell = grid.cell(0, 0).expect("background-only grid cell");
        assert!(grid_cell.graphemes.is_empty());
        assert_eq!(grid_cell.bg, expected);

        let update = engine.render_update(DirtyState::Full).expect("full render update");
        let render_cell = &update.ops[0].rows[0].cells[0];
        assert!(render_cell.graphemes.is_empty());
        assert_eq!(render_cell.style.resolved_bg.r, expected.r);
        assert_eq!(render_cell.style.resolved_bg.g, expected.g);
        assert_eq!(render_cell.style.resolved_bg.b, expected.b);
    }

    assert_background_only_cell(b"\x1b[48;5;42m\x1b[2K", Rgb { r: 0, g: 215, b: 135 });
    assert_background_only_cell(b"\x1b[48;2;17;34;51m\x1b[2K", Rgb { r: 17, g: 34, b: 51 });
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_screen_grid_uses_configured_default_colors() {
    use cleat::vt::{Rgb, TerminalColors};

    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new_with_colors(4, 2, TerminalColors {
        default_foreground: Some(Rgb { r: 0x12, g: 0x34, b: 0x56 }),
        default_background: Some(Rgb { r: 0xAB, g: 0xCD, b: 0xEF }),
        default_cursor: Some(Rgb { r: 0xFE, g: 0xDC, b: 0xBA }),
    });

    let grid = engine.screen_grid().expect("screen grid");
    let untouched_cell = grid.cell(3, 1).unwrap();
    assert_eq!(untouched_cell.fg, Rgb { r: 0x12, g: 0x34, b: 0x56 });
    assert_eq!(untouched_cell.bg, Rgb { r: 0xAB, g: 0xCD, b: 0xEF });
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_screen_grid_multi_codepoint_grapheme() {
    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(40, 5);

    // e + combining acute accent (U+0065 U+0301) → single grapheme cluster "é"
    engine.feed("e\u{0301}AB".as_bytes()).expect("feed combining character sequence");

    let grid = engine.screen_grid().expect("screen grid");

    // The combined grapheme should occupy col 0 with two codepoints
    let combined_cell = grid.cell(0, 0).unwrap();
    assert!(combined_cell.graphemes.len() > 1, "combined é should have multiple codepoints, got {:?}", combined_cell.graphemes);
    assert_eq!(combined_cell.graphemes[0], 'e' as u32);
    assert_eq!(combined_cell.graphemes[1], 0x0301, "second codepoint should be combining acute accent");

    // 'A' should follow at col 1
    let a_cell = grid.cell(1, 0).unwrap();
    assert_eq!(a_cell.graphemes, vec!['A' as u32]);

    // row_text should reconstruct the full grapheme cluster
    let text = grid.row_text(0);
    assert!(text.starts_with("e\u{0301}AB"), "row_text should contain the full grapheme cluster, got: {text:?}");
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_snapshot_and_render_update_preserve_large_grapheme_clusters() {
    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(40, 5);
    let grapheme = format!("e{}", "\u{0301}".repeat(40));
    let expected: Vec<u32> = grapheme.chars().map(u32::from).collect();

    engine.feed(grapheme.as_bytes()).expect("feed large grapheme cluster");

    let grid = engine.screen_grid().expect("screen grid");
    assert_eq!(grid.cell(0, 0).expect("large grapheme cell").graphemes, expected);

    let update = engine.render_update(DirtyState::Full).expect("full render update");
    assert_eq!(update.ops[0].rows[0].cells[0].graphemes, expected);
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn vt_ghostty_prepared_library_exists() {
    let prefix = PathBuf::from(env!("CLEAT_GHOSTTY_PREFIX"));
    #[cfg(target_os = "windows")]
    {
        let shared_library = shared_library_path(&prefix);
        let import_library = prefix.join("lib").join("ghostty-vt.lib");
        assert!(shared_library.exists(), "expected ghostty DLL at {}", shared_library.display());
        assert!(import_library.exists(), "expected ghostty import library at {}", import_library.display());
    }

    #[cfg(not(target_os = "windows"))]
    {
        let static_library = prefix.join("lib").join(static_library_filename());
        let shared_library = shared_library_path(&prefix);
        assert!(
            static_library.exists() || shared_library.exists(),
            "expected static or shared ghostty library at {} or {}",
            static_library.display(),
            shared_library.display()
        );

        if static_library.exists() {
            return;
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let shared_library = shared_library_path(&prefix);
        let lib_name = shared_library_filename();
        let exe = std::env::current_exe().expect("current test binary");
        let output = inspect_linkage(&exe);
        let linkage = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "failed to inspect test binary linkage for {}\nstdout:\n{}\nstderr:\n{}",
            exe.display(),
            linkage,
            stderr
        );
        assert!(
            linkage.contains(lib_name),
            "expected shared ghostty-vt linkage via {}, but test binary dependencies were:\n{}",
            shared_library.display(),
            linkage
        );
    }
}

#[cfg(all(feature = "ghostty-vt", not(target_os = "windows")))]
fn static_library_filename() -> &'static str {
    "libghostty-vt.a"
}

#[cfg(feature = "ghostty-vt")]
fn shared_library_filename() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        "libghostty-vt.so"
    }
    #[cfg(target_os = "macos")]
    {
        "libghostty-vt.dylib"
    }
    #[cfg(target_os = "windows")]
    {
        "ghostty-vt.dll"
    }
}

#[cfg(feature = "ghostty-vt")]
fn shared_library_path(prefix: &std::path::Path) -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let bin_path = prefix.join("bin").join(shared_library_filename());
        if bin_path.exists() {
            return bin_path;
        }
    }
    prefix.join("lib").join(shared_library_filename())
}

#[cfg(all(feature = "ghostty-vt", any(target_os = "linux", target_os = "macos")))]
fn inspect_linkage(exe: &std::path::Path) -> std::process::Output {
    #[cfg(target_os = "linux")]
    {
        Command::new("ldd").arg(exe).output().expect("run ldd")
    }

    #[cfg(target_os = "macos")]
    {
        Command::new("otool").arg("-L").arg(exe).output().expect("run otool")
    }
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn mouse_encoder_gates_and_encodes_through_libghostty() {
    use cleat::vt::{MouseAction, MouseButton, MouseModifiers};

    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(80, 24);
    engine.set_cell_size(8, 16).expect("set cell size");
    let mods = MouseModifiers::default();
    let press = |engine: &mut cleat::vt::ghostty::GhosttyVtEngine, button| {
        engine.encode_mouse(MouseAction::Press, Some(button), false, mods, 20.0, 40.0).expect("encode mouse")
    };

    // No tracking mode enabled => the encoder reports nothing.
    assert!(press(&mut engine, MouseButton::Left).is_empty(), "no mouse mode should emit no report");

    // Any-event tracking + SGR. A left press at (20,40)px with an 8x16 cell lands
    // on grid cell (2,2) => 1-based SGR coordinates (3,3).
    engine.feed(b"\x1b[?1003h\x1b[?1006h").expect("enable sgr mouse");
    assert_eq!(press(&mut engine, MouseButton::Left), b"\x1b[<0;3;3M");
    // Named-button mapping: Cleat/Ghostty disagree on the numeric order, so this
    // guards the Right->2 / Middle->1 SGR button codes.
    assert_eq!(press(&mut engine, MouseButton::Middle), b"\x1b[<1;3;3M");
    assert_eq!(press(&mut engine, MouseButton::Right), b"\x1b[<2;3;3M");

    // SGR-pixels: coordinates switch from cells to pixels (different from above).
    engine.feed(b"\x1b[?1016h").expect("enable sgr-pixels mouse");
    let pixels = press(&mut engine, MouseButton::Left);
    let text = String::from_utf8_lossy(&pixels);
    assert!(text.starts_with("\x1b[<0;") && text.ends_with('M'), "sgr-pixels format, got {text:?}");
    assert_ne!(pixels.as_slice(), b"\x1b[<0;3;3M", "sgr-pixels should differ from cell coords");
}

#[cfg(feature = "ghostty-vt")]
#[test]
fn encode_paste_brackets_when_mode_2004_enabled() {
    let mut engine = cleat::vt::ghostty::GhosttyVtEngine::new(80, 24);
    assert_eq!(engine.encode_paste(b"hello").expect("encode paste"), b"hello");
    assert_eq!(engine.encode_paste(b"hello\nworld").expect("encode paste"), b"hello\rworld");
    assert_eq!(engine.encode_paste(b"hel\x1blo\x00world").expect("encode paste"), b"hel lo world");

    engine.feed(b"\x1b[?2004h").expect("enable bracketed paste");
    assert_eq!(engine.encode_paste(b"hello").expect("encode paste"), b"\x1b[200~hello\x1b[201~");
    assert_eq!(engine.encode_paste(b"hel\x1blo\x00world").expect("encode paste"), b"\x1b[200~hel lo world\x1b[201~");
}
