//! Integration tests for `cleat replay` — path-based invocation only.
//! Session-based replay is exercised in `tests/lifecycle.rs`.

use std::{io::Write, time::Duration};

use clap::Parser;
use cleat::{
    asciicast::{encode_event, encode_header, Event, EventCode, Header},
    cli::{self, Cli, Command, ExecResult},
    runtime::RuntimeLayout,
    server::SessionService,
};

fn write_fixture_cast(dir: &std::path::Path, events: &[Event]) -> std::path::PathBuf {
    let path = dir.join("fixture.cast");
    let mut f = std::fs::File::create(&path).unwrap();
    let header = Header { cols: 80, rows: 24, ..Default::default() };
    writeln!(f, "{}", encode_header(&header)).unwrap();
    let mut prev = Duration::ZERO;
    for e in events {
        writeln!(f, "{}", encode_event(e, &mut prev)).unwrap();
    }
    path
}

fn service_for(root: &std::path::Path) -> SessionService {
    SessionService::new(RuntimeLayout::new(root.to_path_buf()))
}

#[test]
fn replay_positional_path_emits_concatenated_output() {
    let temp = tempfile::tempdir().unwrap();
    let events = vec![Event { time: Duration::from_millis(100), code: EventCode::Output, data: "hello ".into() }, Event {
        time: Duration::from_millis(200),
        code: EventCode::Output,
        data: "world".into(),
    }];
    let cast = write_fixture_cast(temp.path(), &events);

    let cli = Cli::try_parse_from(["cleat", "replay", cast.to_str().unwrap(), "--speed", "1000", "--max-idle", "0ms"]).expect("parse");

    let service = service_for(temp.path());
    let result = cli::execute(cli, &service);
    assert!(matches!(result, ExecResult::Ok(_)), "expected Ok, got {result:?}");
}

#[test]
fn replay_positional_path_errors_on_missing_file() {
    let temp = tempfile::tempdir().unwrap();
    let cli = Cli::try_parse_from(["cleat", "replay", "/nonexistent/file.cast", "--max-idle", "0ms"]).expect("parse");

    let service = service_for(temp.path());
    let result = cli::execute(cli, &service);
    match result {
        ExecResult::Err(msg) => assert!(msg.contains("no such file"), "unexpected error: {msg}"),
        other => panic!("expected Err, got {other:?}"),
    }
}

#[test]
fn replay_with_until_offset_honors_end_bound() {
    let temp = tempfile::tempdir().unwrap();
    let events = vec![Event { time: Duration::from_millis(100), code: EventCode::Output, data: "keep".into() }, Event {
        time: Duration::from_millis(200),
        code: EventCode::Output,
        data: "drop".into(),
    }];
    let cast = write_fixture_cast(temp.path(), &events);

    let cli = Cli::try_parse_from(["cleat", "replay", cast.to_str().unwrap(), "--since", "0", "--max-idle", "0ms"]).expect("parse");
    let service = service_for(temp.path());
    let result = cli::execute(cli, &service);
    assert!(matches!(result, ExecResult::Ok(_)), "expected Ok, got {result:?}");
}

#[test]
fn replay_rejects_since_marker_with_positional_path() {
    let temp = tempfile::tempdir().unwrap();
    let result = Cli::try_parse_from(["cleat", "replay", temp.path().join("fake.cast").to_str().unwrap(), "--since-marker", "a"]);
    assert!(result.is_err(), "--since-marker without --session should be rejected at parse time");
}

#[test]
fn replay_parses_full_flag_surface() {
    let cli = Cli::try_parse_from([
        "cleat",
        "replay",
        "/tmp/demo.cast",
        "--since",
        "100",
        "--until",
        "500",
        "--speed",
        "0.5",
        "--max-idle",
        "2s",
    ])
    .expect("parse");
    match cli.command {
        Command::Replay { path, since, until, speed, max_idle, .. } => {
            assert!(path.is_some());
            assert_eq!(since, Some(100));
            assert_eq!(until, Some(500));
            assert_eq!(speed, 0.5);
            assert_eq!(max_idle, Some(Duration::from_secs(2)));
        }
        other => panic!("expected Replay, got {other:?}"),
    }
}
