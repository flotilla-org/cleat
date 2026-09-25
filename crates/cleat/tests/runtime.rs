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
    assert!(layout.session_dir("demo").exists());
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

#[test]
fn generations_allocate_adopt_and_preserve_logical_coordinates() {
    let temp = tempfile::tempdir().unwrap();
    let layout = RuntimeLayout::new(temp.path().to_path_buf());
    let first = layout.prepare_generation().unwrap();
    assert_eq!(first.daemon_name(), "default@1");
    assert_eq!(layout.generation(), Some(1));
    assert_eq!(first.session_coordinates("a").unwrap().daemon_name(), "default");
    // A not-yet-registered generation is shared by concurrent starters.
    assert_eq!(layout.prepare_generation().unwrap().generation(), Some(1));
    std::fs::write(first.daemon_pid_path(), "999999999").unwrap();
    let second = layout.prepare_generation().unwrap();
    assert_eq!(second.generation(), Some(2));
    assert_eq!(first.generation(), Some(1));
    assert_eq!(layout.generation_names().unwrap(), ["default@1", "default@2"]);
    assert_eq!(layout.resolved().unwrap().daemon_name(), "default@2");
}

#[test]
fn dead_legacy_directory_is_adopted_without_losing_recordings() {
    let temp = tempfile::tempdir().unwrap();
    let layout = RuntimeLayout::new(temp.path().to_path_buf());
    std::fs::create_dir_all(layout.session_dir("old")).unwrap();
    std::fs::write(layout.session_dir("old").join("recording.cast"), "history").unwrap();
    std::fs::write(layout.daemon_pid_path(), "999999999").unwrap();
    assert_eq!(layout.prepare_generation().unwrap().generation(), Some(1));
    assert_eq!(std::fs::read_to_string(layout.session_dir("old").join("recording.cast")).unwrap(), "history");
    assert_eq!(cleat::runtime::hosting_epoch(&layout.session_dir("old")).unwrap(), 1);
}

#[test]
fn generation_names_are_strict_and_session_names_remain_unchanged() {
    for name in ["default@0", "default@01", "default@-1", "default@1@2", "../default@1", "default@18446744073709551616"] {
        assert!(RuntimeLayout::new(PathBuf::from("unused")).with_daemon(name.into()).is_err(), "{name}");
    }
    assert!(cleat::runtime::validate_runtime_name("session@1").is_err());
}

#[test]
fn concurrent_first_use_allocates_one_generation_and_missing_alias_recovers_it() {
    let temp = tempfile::tempdir().unwrap();
    let layout = RuntimeLayout::new(temp.path().to_path_buf());
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let layout = &layout;
            scope.spawn(move || assert_eq!(layout.prepare_generation().unwrap().generation(), Some(1)));
        }
    });
    std::fs::remove_file(temp.path().join("default")).unwrap();
    assert_eq!(layout.prepare_generation().unwrap().generation(), Some(1));
    assert_eq!(layout.generation_names().unwrap(), ["default@1"]);
}

#[test]
fn hosting_epoch_is_created_once_and_rejects_corruption() {
    let temp = tempfile::tempdir().unwrap();
    let layout = RuntimeLayout::new(temp.path().to_path_buf());
    layout.create_session(Some("a".into()), VtEngineKind::Passthrough, None, None).unwrap();
    let epoch = layout.session_dir("a").join("epoch");
    assert_eq!(std::fs::read_to_string(&epoch).unwrap().trim(), "1");
    std::fs::write(&epoch, "7\n").unwrap();
    layout.create_session(Some("a".into()), VtEngineKind::Passthrough, None, None).unwrap();
    assert_eq!(cleat::runtime::hosting_epoch(&layout.session_dir("a")).unwrap(), 7);
    std::fs::write(epoch, "0").unwrap();
    assert!(cleat::runtime::hosting_epoch(&layout.session_dir("a")).is_err());
}
