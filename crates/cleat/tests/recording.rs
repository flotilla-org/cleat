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

#[test]
fn take_snapshot_writes_to_snapshots_directory() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut recorder = OutputRecorder::new(temp.path()).expect("create recorder");
    recorder.record(b"hello world").expect("record bytes");

    recorder.write_snapshot(b"screen state data").expect("take snapshot");

    let snapshot_dir = temp.path().join("snapshots");
    assert!(snapshot_dir.exists());
    let entries: Vec<_> = fs::read_dir(&snapshot_dir).expect("read snapshots").collect();
    assert_eq!(entries.len(), 1);
}

#[test]
fn snapshot_filename_includes_byte_offset() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut recorder = OutputRecorder::new(temp.path()).expect("create recorder");
    recorder.record(b"12345").expect("record 5 bytes");

    recorder.write_snapshot(b"snap").expect("take snapshot");

    let snapshot_path = temp.path().join("snapshots").join("at-5.bin");
    assert!(snapshot_path.exists());
    assert_eq!(fs::read(&snapshot_path).expect("read snapshot"), b"snap");
}
