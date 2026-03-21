use std::path::PathBuf;

use cleat::{runtime::RuntimeLayout, vt::VtEngineKind};

#[test]
fn named_sessions_use_supplied_name_as_id() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = RuntimeLayout::new(temp.path().join("runtime"));

    let session = layout
        .create_session(Some("demo".into()), VtEngineKind::Passthrough, Some(PathBuf::from("/repo")), Some("bash".into()))
        .expect("create session");

    assert_eq!(session.id, "demo");
    assert_eq!(session.vt_engine, VtEngineKind::Passthrough);
    assert!(temp.path().join("runtime").join("demo").exists());
}

#[test]
fn unnamed_sessions_get_generated_ids() {
    let temp = tempfile::tempdir().expect("tempdir");
    let layout = RuntimeLayout::new(temp.path().join("runtime"));

    let a = layout.create_session(None, VtEngineKind::Passthrough, None, None).expect("create session a");
    let b = layout.create_session(None, VtEngineKind::Passthrough, None, None).expect("create session b");

    assert_ne!(a.id, b.id);
    assert!(a.id.starts_with("session-"));
    assert!(b.id.starts_with("session-"));
}
