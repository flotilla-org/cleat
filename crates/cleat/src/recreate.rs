//! Recreation from recording: seed a fresh VT engine by replaying a session's
//! recorded output, so a restarted session shows its prior history as
//! scrollback above a freshly-invoked command.
//!
//! This is the irreducible persistence floor — see
//! docs/adr/0001-session-hosting-and-recreation.md. Recreation is **lossy**: the
//! running process is gone; only its recorded output is restored.

use std::path::Path;

use crate::vt::VtEngine;

/// Returns true if `session_dir` holds a recording we can recreate from: a
/// non-empty cast file. A crashed session that is recreatable must not be
/// garbage-collected by the stale-daemon sweep, since respawning it restores its
/// prior output as scrollback. This is the same signal `SessionRuntime::spawn`
/// uses to decide whether to seed the engine.
pub fn session_is_recreatable(session_dir: &Path) -> bool {
    let cast_path = session_dir.join(crate::recording::CAST_FILE_NAME);
    std::fs::metadata(&cast_path).map(|meta| meta.is_file() && meta.len() > 0).unwrap_or(false)
}

/// Replay every recorded output event from `cast_path` into `engine`, instantly
/// (no inter-event delays). Returns the number of output bytes fed.
///
/// Unlike [`crate::replay::play`], which paces playback to a writer, this
/// reconstructs VT state: the recorded output becomes the engine's screen and
/// scrollback. Non-output events (snapshots, gaps, input, exit) are skipped, so
/// replaying the full stream from the start is a complete reconstruction of the
/// visible history. Seeding from the most recent snapshot is a future
/// optimization (ADR 0002); the MVP replays everything.
pub fn seed_engine_from_cast(engine: &mut dyn VtEngine, cast_path: &Path) -> Result<u64, String> {
    let mut bytes_seeded = 0u64;
    for event in crate::cast_reader::iter_output_between(cast_path, 0, u64::MAX)? {
        let event = event?;
        let data = event.data.as_bytes();
        engine.feed(data)?;
        bytes_seeded += data.len() as u64;
    }
    Ok(bytes_seeded)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::{
        recording::{SessionRecorder, CAST_FILE_NAME},
        vt::passthrough::PassthroughVtEngine,
    };

    #[test]
    fn seeds_engine_with_all_recorded_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut recorder = SessionRecorder::new(dir.path(), 80, 24, "passthrough").expect("recorder");
        recorder.output(b"hello ", Duration::from_millis(10));
        recorder.output(b"world", Duration::from_millis(20));
        recorder.flush();
        drop(recorder);

        let cast_path = dir.path().join(CAST_FILE_NAME);
        let mut engine = PassthroughVtEngine::new(80, 24);
        let seeded = seed_engine_from_cast(&mut engine, &cast_path).expect("seed");

        assert_eq!(seeded, 11, "all output bytes are fed");
        assert_eq!(engine.bytes_seen(), 11, "engine received the recorded output");
    }

    #[test]
    fn seeding_a_missing_cast_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut engine = PassthroughVtEngine::new(80, 24);
        let result = seed_engine_from_cast(&mut engine, &dir.path().join("nope.cast"));
        assert!(result.is_err());
    }
}
