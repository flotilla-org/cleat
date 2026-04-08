use cleat::vt::{passthrough::PassthroughVtEngine, ClientCapabilities, ColorLevel, VtEngine};

mod vt_contracts;

#[cfg(feature = "ghostty-vt")]
use std::{path::PathBuf, process::Command};

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
fn vt_ghostty_links_against_shared_library() {
    let prefix = PathBuf::from(env!("CLEAT_GHOSTTY_PREFIX"));
    let lib_name = shared_library_filename();
    let shared_library = prefix.join("lib").join(lib_name);
    assert!(shared_library.exists(), "expected shared ghostty library at {}", shared_library.display());

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
}

#[cfg(feature = "ghostty-vt")]
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
