use std::fs;

use cleat::recording::OutputRecorder;

#[test]
fn new_recorder_creates_output_log() {
    let temp = tempfile::tempdir().expect("tempdir");
    let _recorder = OutputRecorder::new(temp.path()).expect("create recorder");
    assert!(temp.path().join("output.log").exists());
}

#[test]
fn record_appends_bytes_and_tracks_count() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut recorder = OutputRecorder::new(temp.path()).expect("create recorder");

    recorder.record(b"hello").expect("record hello");
    assert_eq!(recorder.bytes_written(), 5);

    recorder.record(b" world").expect("record world");
    assert_eq!(recorder.bytes_written(), 11);

    let contents = fs::read(temp.path().join("output.log")).expect("read log");
    assert_eq!(contents, b"hello world");
}

#[test]
fn bytes_written_starts_at_zero() {
    let temp = tempfile::tempdir().expect("tempdir");
    let recorder = OutputRecorder::new(temp.path()).expect("create recorder");
    assert_eq!(recorder.bytes_written(), 0);
}
