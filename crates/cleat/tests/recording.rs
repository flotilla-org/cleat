use std::{fs, time::Duration};

use cleat::{
    asciicast::{decode_event, decode_header},
    recording::SessionRecorder,
};

fn new_recorder(dir: &std::path::Path) -> SessionRecorder {
    SessionRecorder::new(dir, 80, 24, "test-engine").expect("create recorder")
}

#[test]
fn new_recorder_creates_session_cast_with_header() {
    let temp = tempfile::tempdir().expect("tempdir");
    let _recorder = new_recorder(temp.path());

    let cast_path = temp.path().join("session.cast");
    assert!(cast_path.exists(), "session.cast should be created");

    let contents = fs::read_to_string(&cast_path).expect("read cast file");
    let first_line = contents.lines().next().expect("cast file must have at least one line");
    decode_header(first_line).expect("first line should be a valid header");
}

#[test]
fn output_event_written_after_flush() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut recorder = new_recorder(temp.path());

    recorder.output(b"hello", Duration::from_millis(100));
    recorder.flush();

    let contents = fs::read_to_string(temp.path().join("session.cast")).expect("read cast file");
    let lines: Vec<&str> = contents.lines().collect();
    assert_eq!(lines.len(), 2, "should have header + one event");

    let mut prev = Duration::ZERO;
    let event = decode_event(lines[1], &mut prev).expect("second line should be a valid event");
    assert_eq!(event.code, cleat::asciicast::EventCode::Output);
    assert_eq!(event.data, "hello");
}

#[test]
fn consecutive_outputs_coalesced_into_single_event() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut recorder = new_recorder(temp.path());

    recorder.output(b"foo", Duration::from_millis(10));
    recorder.output(b"bar", Duration::from_millis(20));
    recorder.flush();

    let contents = fs::read_to_string(temp.path().join("session.cast")).expect("read cast file");
    let lines: Vec<&str> = contents.lines().collect();
    assert_eq!(lines.len(), 2, "two consecutive outputs should coalesce into one event");

    let mut prev = Duration::ZERO;
    let event = decode_event(lines[1], &mut prev).expect("event should decode");
    assert_eq!(event.data, "foobar");
}

#[test]
fn type_change_flushes_previous_buffer() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut recorder = new_recorder(temp.path());

    recorder.output(b"out", Duration::from_millis(10));
    recorder.input(b"in", Duration::from_millis(20));
    recorder.flush();

    let contents = fs::read_to_string(temp.path().join("session.cast")).expect("read cast file");
    let lines: Vec<&str> = contents.lines().collect();
    assert_eq!(lines.len(), 3, "output then input should produce 2 event lines");

    let mut prev = Duration::ZERO;
    let first_event = decode_event(lines[1], &mut prev).expect("first event");
    let second_event = decode_event(lines[2], &mut prev).expect("second event");

    assert_eq!(first_event.code, cleat::asciicast::EventCode::Output);
    assert_eq!(second_event.code, cleat::asciicast::EventCode::Input);
}

#[test]
fn input_event_recorded() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut recorder = new_recorder(temp.path());

    recorder.input(b"keystroke", Duration::from_millis(50));
    recorder.flush();

    let contents = fs::read_to_string(temp.path().join("session.cast")).expect("read cast file");
    let lines: Vec<&str> = contents.lines().collect();
    assert_eq!(lines.len(), 2);

    let mut prev = Duration::ZERO;
    let event = decode_event(lines[1], &mut prev).expect("event should decode");
    assert_eq!(event.code, cleat::asciicast::EventCode::Input);
    assert_eq!(event.data, "keystroke");
}

#[test]
fn bytes_written_tracks_cast_file_offset() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut recorder = new_recorder(temp.path());

    let header_offset = recorder.bytes_written();
    assert!(header_offset > 0, "header should have written some bytes");

    recorder.output(b"data", Duration::from_millis(10));
    recorder.flush();

    let after_offset = recorder.bytes_written();
    assert!(after_offset > header_offset, "offset should grow after writing an event");

    let actual_size = fs::metadata(temp.path().join("session.cast")).expect("metadata").len();
    assert_eq!(after_offset, actual_size, "bytes_written should match actual file size");
}

#[test]
fn metadata_event_flushes_buffer_and_writes_inline() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut recorder = new_recorder(temp.path());

    recorder.output(b"output data", Duration::from_millis(10));
    recorder.event(cleat::asciicast::EventCode::Custom('s'), "SIGWINCH", Duration::from_millis(20));

    let contents = fs::read_to_string(temp.path().join("session.cast")).expect("read cast file");
    let lines: Vec<&str> = contents.lines().collect();
    // header + flushed output + signal event = 3 lines
    assert_eq!(lines.len(), 3, "output (flushed) + signal event = 2 event lines");

    let mut prev = Duration::ZERO;
    let first_event = decode_event(lines[1], &mut prev).expect("first event");
    let second_event = decode_event(lines[2], &mut prev).expect("second event");

    assert_eq!(first_event.code, cleat::asciicast::EventCode::Output);
    assert_eq!(second_event.code, cleat::asciicast::EventCode::Custom('s'));
}

#[test]
fn gap_event_emitted_on_resume() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut recorder = new_recorder(temp.path());

    recorder.output(b"before gap", Duration::from_millis(10));
    recorder.emit_gap("detach", Duration::from_millis(5000));

    let contents = fs::read_to_string(temp.path().join("session.cast")).expect("read cast file");
    let lines: Vec<&str> = contents.lines().collect();
    assert_eq!(lines.len(), 3, "output (flushed) + gap = 2 event lines");

    let mut prev = Duration::ZERO;
    let _first_event = decode_event(lines[1], &mut prev).expect("first event");
    let gap_event = decode_event(lines[2], &mut prev).expect("gap event");

    assert_eq!(gap_event.code, cleat::asciicast::EventCode::Custom('g'));
    assert!(gap_event.data.contains("detach"), "gap data should contain the reason");
}
