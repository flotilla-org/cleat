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

/// Read all output (`"o"`) events whose starting byte offset is in `[start, end)`.
///
/// When `start` is 0, the header line is skipped automatically.
/// Returns an empty vec if `start >= end` or `start` is at/beyond EOF.
pub fn read_output_between(path: &Path, start: u64, end: u64) -> Result<Vec<Event>, String> {
    if start >= end {
        return Ok(Vec::new());
    }
    read_events_between(path, start, end, Some(EventCode::Output))
}

fn read_events_between(path: &Path, start: u64, end: u64, filter: Option<EventCode>) -> Result<Vec<Event>, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("open {path:?}: {e}"))?;
    let mut reader = BufReader::new(file);

    if start > 0 {
        reader.seek(SeekFrom::Start(start)).map_err(|e| format!("seek: {e}"))?;
    }

    let mut events = Vec::new();
    let mut line = String::new();
    let mut byte_pos = start;
    let mut prev_time = Duration::ZERO;
    let mut first_line = start == 0;

    loop {
        if byte_pos >= end {
            break;
        }
        line.clear();
        let n = reader.read_line(&mut line).map_err(|e| format!("read line: {e}"))?;
        if n == 0 {
            break;
        }
        byte_pos += n as u64;

        if first_line {
            first_line = false;
            continue;
        }

        let trimmed = line.trim_end_matches('\n');
        if trimmed.is_empty() {
            continue;
        }
        match decode_event(trimmed, &mut prev_time) {
            Ok(event) => {
                let matches_filter = filter.as_ref().is_none_or(|code| &event.code == code);
                if matches_filter {
                    events.push(event);
                }
            }
            Err(_) => continue,
        }
    }

    Ok(events)
}

/// Scan output events starting at `start` and return the byte offset of the
/// first byte *after* the last output event before the first inter-event gap
/// whose duration is ≥ `threshold`.
///
/// Returns `Ok(None)` if no such gap is found before EOF.
///
/// Only output (`"o"`) events participate in gap detection. Non-output events
/// (markers, snapshots, etc.) are skipped.
pub fn find_idle_gap_after(path: &Path, start: u64, threshold: Duration) -> Result<Option<u64>, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("open {path:?}: {e}"))?;
    let mut reader = BufReader::new(file);

    if start > 0 {
        reader.seek(SeekFrom::Start(start)).map_err(|e| format!("seek: {e}"))?;
    }

    let mut line = String::new();
    let mut byte_pos = start;
    let mut prev_time = Duration::ZERO;
    let mut first_line = start == 0;
    let mut last_output_end: Option<u64> = None;
    let mut last_output_time: Option<Duration> = None;

    loop {
        line.clear();
        let n = reader.read_line(&mut line).map_err(|e| format!("read line: {e}"))?;
        if n == 0 {
            break;
        }
        byte_pos += n as u64;

        if first_line {
            first_line = false;
            continue;
        }

        let trimmed = line.trim_end_matches('\n');
        if trimmed.is_empty() {
            continue;
        }

        match decode_event(trimmed, &mut prev_time) {
            Ok(event) => {
                if event.code != EventCode::Output {
                    continue;
                }
                if let Some(prev_t) = last_output_time {
                    let gap = event.time.saturating_sub(prev_t);
                    if gap >= threshold {
                        return Ok(last_output_end);
                    }
                }
                last_output_time = Some(event.time);
                last_output_end = Some(byte_pos);
            }
            Err(_) => continue,
        }
    }

    Ok(None)
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

#[cfg(test)]
mod tests_between_and_idle {
    use std::io::Write;

    use tempfile::NamedTempFile;

    use super::*;

    fn write_cast(lines: &[&str]) -> NamedTempFile {
        let mut f = NamedTempFile::new().expect("tempfile");
        for line in lines {
            writeln!(f, "{line}").expect("write line");
        }
        f.flush().expect("flush");
        f
    }

    // Minimal asciicast v3 header + three output events.
    // EVT_A at delta 0.0s (cumulative 0.0s)
    // EVT_B at delta 0.1s (cumulative 0.1s)
    // EVT_C at delta 0.3s (cumulative 0.4s)
    // Gaps: A->B = 0.1s, B->C = 0.3s.
    const HEADER: &str = r#"{"version":3,"term":{"cols":80,"rows":24}}"#;
    const EVT_A: &str = r#"[0.0,"o","a"]"#;
    const EVT_B: &str = r#"[0.1,"o","b"]"#;
    const EVT_C: &str = r#"[0.3,"o","c"]"#;

    #[test]
    fn read_between_returns_events_in_range() {
        let f = write_cast(&[HEADER, EVT_A, EVT_B, EVT_C]);
        let header_len = (HEADER.len() + 1) as u64;
        let a_len = (EVT_A.len() + 1) as u64;
        let b_len = (EVT_B.len() + 1) as u64;

        // Range covers A and B only (ends just before C).
        let events = read_output_between(f.path(), header_len, header_len + a_len + b_len).expect("read range");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "a");
        assert_eq!(events[1].data, "b");
    }

    #[test]
    fn read_between_empty_when_start_equals_end() {
        let f = write_cast(&[HEADER, EVT_A]);
        let header_len = (HEADER.len() + 1) as u64;
        let events = read_output_between(f.path(), header_len, header_len).expect("read");
        assert!(events.is_empty());
    }

    #[test]
    fn find_idle_gap_detects_gap_above_threshold() {
        let f = write_cast(&[HEADER, EVT_A, EVT_B, EVT_C]);
        // B->C gap is 0.3s which exceeds threshold 0.2s.
        // Result should be byte offset after B.
        let header_len = (HEADER.len() + 1) as u64;
        let a_len = (EVT_A.len() + 1) as u64;
        let b_len = (EVT_B.len() + 1) as u64;
        let end = find_idle_gap_after(f.path(), header_len, Duration::from_millis(200)).expect("find gap");
        assert_eq!(end, Some(header_len + a_len + b_len));
    }

    #[test]
    fn find_idle_gap_returns_none_when_no_gap_big_enough() {
        let f = write_cast(&[HEADER, EVT_A, EVT_B, EVT_C]);
        let header_len = (HEADER.len() + 1) as u64;
        // Threshold 1s; no gap in fixture is that large.
        let end = find_idle_gap_after(f.path(), header_len, Duration::from_secs(1)).expect("find gap");
        assert_eq!(end, None);
    }

    #[test]
    fn find_idle_gap_returns_none_on_empty_range() {
        let f = write_cast(&[HEADER]);
        let header_len = (HEADER.len() + 1) as u64;
        let end = find_idle_gap_after(f.path(), header_len, Duration::from_millis(100)).expect("find gap");
        assert_eq!(end, None);
    }
}
