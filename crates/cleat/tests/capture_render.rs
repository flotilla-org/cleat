use std::{io::Write, time::Duration};

use cleat::{
    asciicast::{encode_event, encode_header, Event, EventCode, Header},
    recording::CAST_FILE_NAME,
    runtime::RuntimeLayout,
    server::SessionService,
};

fn setup_session_with_cast(root: &std::path::Path, id: &str, events: &[Event]) {
    let session_dir = root.join(id);
    std::fs::create_dir_all(&session_dir).unwrap();
    let path = session_dir.join(CAST_FILE_NAME);
    let mut f = std::fs::File::create(&path).unwrap();
    let header = Header { cols: 80, rows: 24, ..Default::default() };
    writeln!(f, "{}", encode_header(&header)).unwrap();
    let mut prev = Duration::ZERO;
    for event in events {
        writeln!(f, "{}", encode_event(event, &mut prev)).unwrap();
    }
}

#[test]
fn capture_since_text_returns_concatenated_output() {
    let temp = tempfile::tempdir().unwrap();
    let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));

    let events = vec![Event { time: Duration::from_millis(100), code: EventCode::Output, data: "hello ".into() }, Event {
        time: Duration::from_millis(200),
        code: EventCode::Output,
        data: "world".into(),
    }];
    setup_session_with_cast(temp.path(), "sess", &events);

    let result = service.capture_since_text("sess", 0).unwrap();
    assert!(result.contains("hello "));
    assert!(result.contains("world"));
}

#[test]
fn capture_since_text_skips_non_output_events() {
    let temp = tempfile::tempdir().unwrap();
    let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));

    let events = vec![
        Event { time: Duration::from_millis(100), code: EventCode::Input, data: "typed".into() },
        Event { time: Duration::from_millis(200), code: EventCode::Output, data: "visible".into() },
        Event { time: Duration::from_millis(300), code: EventCode::Custom('s'), data: "signal".into() },
    ];
    setup_session_with_cast(temp.path(), "sess", &events);

    let result = service.capture_since_text("sess", 0).unwrap();
    assert!(result.contains("visible"));
    assert!(!result.contains("typed"));
    assert!(!result.contains("signal"));
}

#[test]
fn capture_since_text_returns_empty_at_eof() {
    let temp = tempfile::tempdir().unwrap();
    let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));

    let events = vec![Event { time: Duration::from_millis(100), code: EventCode::Output, data: "done".into() }];
    setup_session_with_cast(temp.path(), "sess", &events);

    let file_size = std::fs::metadata(temp.path().join("sess").join(CAST_FILE_NAME)).unwrap().len();
    let result = service.capture_since_text("sess", file_size).unwrap();
    assert!(result.is_empty());
}

#[test]
fn capture_since_errors_when_no_recording() {
    let temp = tempfile::tempdir().unwrap();
    let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));

    // Create session dir but no .cast file
    std::fs::create_dir_all(temp.path().join("no-rec")).unwrap();

    let err = service.capture_since_text("no-rec", 0).unwrap_err();
    assert!(err.contains("no recording"), "error should mention missing recording: {err}");
}
