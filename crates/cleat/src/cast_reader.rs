//! Reader for `.cast` (asciicast v3 NDJSON) files.
//!
//! Provides helpers for reading events from an arbitrary byte offset within a
//! cast file, and for locating snapshot events.

use std::{
    io::{BufRead, BufReader, Seek, SeekFrom},
    path::Path,
    time::Duration,
};

use crate::asciicast::{decode_event, Event, EventCode};

/// Read all output (`"o"`) events starting from `offset` bytes into the file.
///
/// When `offset` is 0, the header line is skipped automatically.
/// Returns an empty vec if `offset` is at or beyond the end of the file.
///
/// **Note:** When `offset` is nonzero, `Event.time` values are relative to the
/// seek point (not absolute from recording start), since delta accumulation
/// restarts at zero. Use `data` and `code` fields; do not rely on `time`.
pub fn read_output_since(path: &Path, offset: u64) -> Result<Vec<Event>, String> {
    read_events_since(path, offset, Some(EventCode::Output))
}

/// Read all events (any code) starting from `offset` bytes into the file.
///
/// When `offset` is 0, the header line is skipped automatically.
/// Returns an empty vec if `offset` is at or beyond the end of the file.
///
/// **Note:** When `offset` is nonzero, `Event.time` values are relative to the
/// seek point (not absolute from recording start). See [`read_output_since`].
pub fn read_all_events_since(path: &Path, offset: u64) -> Result<Vec<Event>, String> {
    read_events_since(path, offset, None)
}

/// Scan the file from the beginning and return the last snapshot (`"S"` custom
/// event) whose line *starts* before `offset`.
///
/// Returns `None` if no snapshot event is found before `offset`.
pub fn find_nearest_snapshot(path: &Path, offset: u64) -> Result<Option<(u64, String)>, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("open {path:?}: {e}"))?;

    // When offset > file_size, the loop below naturally scans the whole file
    // (byte_pos never reaches offset), returning the last snapshot found.
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut byte_pos: u64 = 0;
    let mut last_snapshot: Option<(u64, String)> = None;
    let mut first_line = true;
    let mut prev_time = Duration::ZERO;

    loop {
        if byte_pos >= offset {
            break;
        }
        line.clear();
        let n = reader.read_line(&mut line).map_err(|e| format!("read line: {e}"))?;
        if n == 0 {
            break; // EOF
        }
        let line_start = byte_pos;
        byte_pos += n as u64;

        if first_line {
            first_line = false;
            continue; // skip header
        }

        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }

        match decode_event(trimmed, &mut prev_time) {
            Ok(event) => {
                if event.code == EventCode::Custom('S') {
                    last_snapshot = Some((line_start, event.data));
                }
            }
            Err(_) => {
                // Skip malformed lines.
            }
        }
    }

    Ok(last_snapshot)
}

/// Internal helper: read events from `path` starting at `offset`.
///
/// If `filter` is `Some(code)`, only events matching that code are returned.
/// If `filter` is `None`, all events are returned.
///
/// When `offset` is 0 the header line is skipped. When `offset` is nonzero the
/// file is seeked to that position and lines are read from there. Note: in the
/// nonzero-offset case `prev_time` starts at `Duration::ZERO`, so the `time`
/// field on returned events is relative to the seek point, not the start of the
/// recording. Callers that only need `data` and `code` are unaffected.
fn read_events_since(path: &Path, offset: u64, filter: Option<EventCode>) -> Result<Vec<Event>, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("open {path:?}: {e}"))?;
    let file_size = file.metadata().map_err(|e| format!("metadata {path:?}: {e}"))?.len();

    if offset >= file_size {
        return Ok(vec![]);
    }

    let mut reader = BufReader::new(file);
    let mut prev_time = Duration::ZERO;
    let mut events = Vec::new();

    if offset == 0 {
        // Skip the header line.
        let mut header_line = String::new();
        reader.read_line(&mut header_line).map_err(|e| format!("read header: {e}"))?;
    } else {
        reader.seek(SeekFrom::Start(offset)).map_err(|e| format!("seek: {e}"))?;
    }

    for line in reader.lines() {
        let line = line.map_err(|e| format!("read line: {e}"))?;
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }

        match decode_event(trimmed, &mut prev_time) {
            Ok(event) => {
                let include = match &filter {
                    Some(code) => &event.code == code,
                    None => true,
                };
                if include {
                    events.push(event);
                }
            }
            Err(_) => {
                // Skip malformed lines.
            }
        }
    }

    Ok(events)
}
