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
