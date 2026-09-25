#![cfg(unix)]
use std::{fs, time::Duration};

use cleat::{
    hosting_epoch,
    recording::SessionRecorder,
    runtime::RuntimeLayout,
    server::{EndBound, SessionService, StartBound},
};

#[test]
fn epoch_defaults_increments_and_rejects_corruption() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(hosting_epoch::read(dir.path()).unwrap(), 1);
    assert_eq!(hosting_epoch::increment(dir.path()).unwrap(), 2);
    assert_eq!(hosting_epoch::read(dir.path()).unwrap(), 2);
    assert_eq!(hosting_epoch::increment(dir.path()).unwrap(), 3);
    for invalid in ["0", "-1", "garbage", "18446744073709551615"] {
        fs::write(dir.path().join("epoch"), invalid).unwrap();
        assert!(hosting_epoch::increment(dir.path()).is_err());
        assert_eq!(fs::read_to_string(dir.path().join("epoch")).unwrap(), invalid);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}

#[test]
fn transferred_marker_and_snapshot_survive_capture_slicing() {
    let dir = tempfile::tempdir().unwrap();
    let layout = RuntimeLayout::new(dir.path().into());
    let session = layout.session_dir("transfer");
    fs::create_dir_all(&session).unwrap();
    let mut recorder = SessionRecorder::new(&session, 80, 24, "passthrough").unwrap();
    recorder.output(b"before", Duration::from_secs(1));
    recorder.flush();
    let transfer_offset = recorder.bytes_written();
    recorder.transferred(2, "embedded:test", Duration::from_secs(2));
    recorder.write_snapshot("\x1b[Hbefore", "passthrough", 80, 24, Duration::from_secs(2));
    recorder.output(b"after", Duration::from_secs(3));
    recorder.flush();
    let cast = session.join("session.cast");
    let events = cleat::cast_reader::read_all_events_since(&cast, 0).unwrap();
    let marker = events.iter().find(|e| e.code == cleat::asciicast::EventCode::Marker).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&marker.data).unwrap(),
        serde_json::json!({"event":"transferred", "epoch":2, "address":"embedded:test"})
    );
    let snapshot = events.iter().find(|e| e.code == cleat::asciicast::EventCode::Custom('S')).unwrap();
    let payload: cleat::recording::ReplaySnapshot = serde_json::from_str(&snapshot.data).unwrap();
    assert_eq!(payload.state, "\x1b[Hbefore");
    let service = SessionService::new(layout);
    let (output, _) = service.capture_slice_raw("transfer", StartBound::Offset(0), EndBound::EndOfRecording).unwrap();
    assert_eq!(output, "beforeafter");
    let (output, _) = service.capture_slice_raw("transfer", StartBound::Offset(transfer_offset), EndBound::EndOfRecording).unwrap();
    assert_eq!(output, "after");
    assert_eq!(cleat::cast_reader::read_all_events_since(&cast, 0).unwrap(), events);
}
