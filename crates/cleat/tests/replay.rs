//! Integration tests for `cleat replay` — path-based invocation only.
//! Session-based replay is exercised in `tests/lifecycle.rs`.

use std::{io::Write, path::PathBuf, time::Duration};

use clap::Parser;
use cleat::{
    asciicast::{encode_event, encode_header, Event, EventCode, Header},
    cli::{Cli, Command},
    replay::{run_replay, ReplayOptions},
    server::{resolve_range_for_path, EndBound, FallbackReason, StartBound},
};

fn write_fixture_cast(dir: &std::path::Path, events: &[Event]) -> PathBuf {
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

fn replay_path_to_bytes(cast_path: &std::path::Path, start: StartBound, end: EndBound) -> Vec<u8> {
    let (so, eo, _) = resolve_range_for_path(cast_path, start, end).expect("resolve");
    let opts = ReplayOptions { speed: 1_000_000.0, max_idle: Some(Duration::ZERO) };
    let mut buf: Vec<u8> = Vec::new();
    run_replay(cast_path, so, eo, &opts, &mut buf, |_| {}).expect("run_replay");
    buf
}

#[test]
fn replay_emits_concatenated_output_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let events = vec![Event { time: Duration::from_millis(100), code: EventCode::Output, data: "hello ".into() }, Event {
        time: Duration::from_millis(200),
        code: EventCode::Output,
        data: "world".into(),
    }];
    let cast = write_fixture_cast(temp.path(), &events);
    let bytes = replay_path_to_bytes(&cast, StartBound::Offset(0), EndBound::EndOfRecording);
    assert_eq!(bytes, b"hello world");
}

#[test]
fn replay_respects_until_offset_end_bound() {
    let temp = tempfile::tempdir().unwrap();
    let events = vec![Event { time: Duration::from_millis(100), code: EventCode::Output, data: "keep".into() }, Event {
        time: Duration::from_millis(200),
        code: EventCode::Output,
        data: "drop".into(),
    }];
    let cast = write_fixture_cast(temp.path(), &events);

    // Compute the offset that's just after the first event line.
    let contents = std::fs::read(&cast).unwrap();
    let header_end = contents.iter().position(|&b| b == b'\n').unwrap() as u64 + 1;
    let first_event_end = contents.iter().skip(header_end as usize).position(|&b| b == b'\n').map(|p| header_end + p as u64 + 1).unwrap();

    let bytes = replay_path_to_bytes(&cast, StartBound::Offset(0), EndBound::Offset(first_event_end));
    assert_eq!(bytes, b"keep");
}

#[test]
fn replay_idle_gap_fallback_reports_no_idle_found() {
    let temp = tempfile::tempdir().unwrap();
    let events = vec![Event { time: Duration::from_millis(10), code: EventCode::Output, data: "a".into() }, Event {
        time: Duration::from_millis(20),
        code: EventCode::Output,
        data: "b".into(),
    }];
    let cast = write_fixture_cast(temp.path(), &events);

    let (_so, eo, status) =
        resolve_range_for_path(&cast, StartBound::Offset(0), EndBound::IdleGap(Duration::from_secs(10))).expect("resolve");

    // No 10s gap in the fixture — fell back to EOF.
    assert_eq!(eo, std::fs::metadata(&cast).unwrap().len());
    assert!(matches!(status, Some(FallbackReason::NoIdleGap(_))));
}

#[test]
fn replay_rejects_end_offset_before_start() {
    let temp = tempfile::tempdir().unwrap();
    let events = vec![Event { time: Duration::from_millis(10), code: EventCode::Output, data: "a".into() }];
    let cast = write_fixture_cast(temp.path(), &events);
    let err = resolve_range_for_path(&cast, StartBound::Offset(100), EndBound::Offset(50)).unwrap_err();
    assert!(err.contains("precedes start"), "unexpected error: {err}");
}

#[test]
fn replay_rejects_marker_bounds_with_path_resolver() {
    let temp = tempfile::tempdir().unwrap();
    let events = vec![Event { time: Duration::from_millis(10), code: EventCode::Output, data: "a".into() }];
    let cast = write_fixture_cast(temp.path(), &events);
    let err = resolve_range_for_path(&cast, StartBound::Marker("x".into()), EndBound::EndOfRecording).unwrap_err();
    assert!(err.contains("marker"), "unexpected error: {err}");
    let err2 = resolve_range_for_path(&cast, StartBound::Offset(0), EndBound::Marker("x".into())).unwrap_err();
    assert!(err2.contains("marker"), "unexpected error: {err2}");
}

// Parse-level tests — verify clap configuration.
// (`replay_requires_path_or_session` lives in `tests/cli.rs` alongside the
// other `replay_*` parse tests; not duplicated here.)

#[test]
fn replay_parse_rejects_since_marker_with_positional_path() {
    let temp = tempfile::tempdir().unwrap();
    let result = Cli::try_parse_from(["cleat", "replay", temp.path().join("fake.cast").to_str().unwrap(), "--since-marker", "a"]);
    assert!(result.is_err(), "--since-marker without --session should be rejected");
}

#[test]
fn replay_parse_full_flag_surface() {
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
