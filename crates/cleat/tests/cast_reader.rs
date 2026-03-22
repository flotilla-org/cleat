use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use cleat::{
    asciicast::{encode_event, encode_header, Event, EventCode, Header},
    cast_reader::{find_nearest_snapshot, read_all_events_since, read_output_since},
};

fn write_cast_file(dir: &Path, events: &[Event]) -> PathBuf {
    let path = dir.join("session.cast");
    let header = Header::default();
    let mut lines = vec![encode_header(&header)];
    let mut prev = Duration::ZERO;
    for event in events {
        lines.push(encode_event(event, &mut prev));
    }
    let content = lines.join("\n") + "\n";
    std::fs::write(&path, content).expect("write cast file");
    path
}

fn out(time_ms: u64, data: &str) -> Event {
    Event { time: Duration::from_millis(time_ms), code: EventCode::Output, data: data.to_string() }
}

fn inp(time_ms: u64, data: &str) -> Event {
    Event { time: Duration::from_millis(time_ms), code: EventCode::Input, data: data.to_string() }
}

fn snapshot(time_ms: u64, data: &str) -> Event {
    Event { time: Duration::from_millis(time_ms), code: EventCode::Custom('S'), data: data.to_string() }
}

fn resize(time_ms: u64, data: &str) -> Event {
    Event { time: Duration::from_millis(time_ms), code: EventCode::Resize, data: data.to_string() }
}

#[test]
fn read_output_since_offset_returns_events_after_cursor() {
    let dir = tempfile::tempdir().expect("tempdir");
    let events = vec![out(100, "first"), out(200, "second"), out(300, "third")];
    let path = write_cast_file(dir.path(), &events);

    // Find the offset after the first event line
    let content = std::fs::read(&path).expect("read file");
    let first_newline = content.iter().position(|&b| b == b'\n').expect("first newline");
    let second_newline = content[first_newline + 1..].iter().position(|&b| b == b'\n').expect("second newline");
    let offset = (first_newline + 1 + second_newline + 1) as u64;

    let result = read_output_since(&path, offset).expect("read output since");
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].data, "second");
    assert_eq!(result[1].data, "third");
}

#[test]
fn read_output_since_zero_returns_all_output() {
    let dir = tempfile::tempdir().expect("tempdir");
    let events = vec![out(100, "alpha"), inp(150, "user-input"), out(200, "beta")];
    let path = write_cast_file(dir.path(), &events);

    let result = read_output_since(&path, 0).expect("read output since zero");
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].data, "alpha");
    assert_eq!(result[1].data, "beta");
}

#[test]
fn read_output_since_beyond_eof_returns_empty() {
    let dir = tempfile::tempdir().expect("tempdir");
    let events = vec![out(100, "hello")];
    let path = write_cast_file(dir.path(), &events);

    let file_size = std::fs::metadata(&path).expect("metadata").len();
    let result = read_output_since(&path, file_size + 100).expect("read beyond eof");
    assert!(result.is_empty());
}

#[test]
fn read_output_since_skips_non_output_events() {
    let dir = tempfile::tempdir().expect("tempdir");
    let events = vec![inp(100, "user-types"), snapshot(150, r#"{"state":"snap"}"#), resize(200, "100x40"), out(300, "visible")];
    let path = write_cast_file(dir.path(), &events);

    let result = read_output_since(&path, 0).expect("read output since");
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].data, "visible");
}

#[test]
fn find_nearest_snapshot_before_offset() {
    let dir = tempfile::tempdir().expect("tempdir");
    let events = vec![
        out(100, "line1"),
        snapshot(200, r#"{"state":"snap1"}"#),
        out(300, "line2"),
        snapshot(400, r#"{"state":"snap2"}"#),
        out(500, "line3"),
    ];
    let path = write_cast_file(dir.path(), &events);

    // Find the byte offset just after the second snapshot line
    let content = std::fs::read(&path).expect("read file");
    // Count through newlines: header + event1 + snap1 + event2 + snap2
    let mut pos = 0usize;
    for _ in 0..5 {
        let nl = content[pos..].iter().position(|&b| b == b'\n').expect("newline") + pos;
        pos = nl + 1;
    }
    let offset_after_snap2 = pos as u64;

    let result = find_nearest_snapshot(&path, offset_after_snap2).expect("find nearest snapshot");
    let (_, data) = result.expect("snapshot found");
    assert_eq!(data, r#"{"state":"snap2"}"#);
}

#[test]
fn find_nearest_snapshot_returns_none_when_no_snapshots() {
    let dir = tempfile::tempdir().expect("tempdir");
    let events = vec![out(100, "hello"), out(200, "world")];
    let path = write_cast_file(dir.path(), &events);

    let file_size = std::fs::metadata(&path).expect("metadata").len();
    let result = find_nearest_snapshot(&path, file_size).expect("find nearest snapshot");
    assert!(result.is_none());
}

#[test]
fn read_all_events_since_returns_all_event_types() {
    let dir = tempfile::tempdir().expect("tempdir");
    let events = vec![out(100, "output-data"), inp(200, "input-data")];
    let path = write_cast_file(dir.path(), &events);

    let result = read_all_events_since(&path, 0).expect("read all events since");
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].code, EventCode::Output);
    assert_eq!(result[0].data, "output-data");
    assert_eq!(result[1].code, EventCode::Input);
    assert_eq!(result[1].data, "input-data");
}
