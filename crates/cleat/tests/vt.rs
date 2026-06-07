#[cfg(feature = "ghostty-vt")]
use cleat::provider::{
    DirtyState, TerminalRenderUpdateOpKind, TerminalStyleColorTag, TerminalViewportKind, ViewportCommand, ViewportCommandOutcome,
};
use cleat::vt::{passthrough::PassthroughVtEngine, ClientCapabilities, ColorLevel, VtEngine};

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
