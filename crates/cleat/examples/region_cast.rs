//! SPIKE (#319): turn a cleat recording carrying the draft region OSC into
//! (frame cells, region rectangles) pairs.
//!
//! Draft wire format (docs/spikes/region-ground-truth.md):
//!
//! ```text
//! ESC ] 7701 ; B ; f=<frame> ; o=a|c ST   begin snapshot (o=c: y is relative to the cursor row here)
//! ESC ] 7701 ; R ; id=..;parent=..;kind=..;name=..;x=..;y=..;w=..;h=.. ST
//! ESC ] 7701 ; E ST                        commit: replaces the previous snapshot
//! ESC ] 7701 ; X ST                        clear all declared regions
//! ```
//!
//! Ghostty drops unknown OSCs, so the scanner reads the raw cast bytes (as
//! docs/design/semantic-prompt-evidence/scan-casts.py does) and feeds the
//! bytes between records to a Ghostty engine, snapshotting the grid at `E`.
//!
//! Usage:
//!   cargo run -p cleat --example region_cast -- CAST            # JSON lines
//!   cargo run -p cleat --example region_cast -- CAST --frame N  # human view
#[cfg(feature = "ghostty-vt")]
use cleat::vt::{ghostty::GhosttyVtEngine, VtEngine};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Region {
    pub id: String,
    pub parent: Option<String>,
    pub kind: String,
    pub name: Option<String>,
    pub x: i32,
    pub y: i32,
    pub w: u16,
    pub h: u16,
    /// Smallest earlier-declared region that contains this one, when the
    /// emitter gave no explicit parent (frameworks often lose call nesting).
    pub contained_in: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    Begin { frame: u64, cursor_relative: bool },
    Region(Region),
    End,
    Clear,
}

/// Undo the draft escaping (`\xHH`).
fn unescape(v: &str) -> String {
    let bytes = v.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() && bytes[i + 1] == b'x' {
            let hex = std::str::from_utf8(&bytes[i + 2..i + 4]).unwrap_or("");
            if let Ok(b) = u8::from_str_radix(hex, 16) {
                out.push(b);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn parse_record(payload: &str) -> Option<Record> {
    let mut parts = payload.split(';');
    let verb = parts.next()?;
    let mut kv = std::collections::BTreeMap::new();
    for p in parts {
        if let Some((k, v)) = p.split_once('=') {
            kv.insert(k.to_string(), unescape(v));
        }
    }
    let num = |k: &str| kv.get(k).and_then(|v| v.parse::<i64>().ok());
    match verb {
        "B" => Some(Record::Begin { frame: num("f").unwrap_or(0) as u64, cursor_relative: kv.get("o").map(String::as_str) == Some("c") }),
        "R" => Some(Record::Region(Region {
            id: kv.get("id")?.clone(),
            parent: kv.get("parent").cloned(),
            kind: kv.get("kind").cloned().unwrap_or_default(),
            name: kv.get("name").cloned(),
            x: num("x")? as i32,
            y: num("y")? as i32,
            w: num("w")?.clamp(0, u16::MAX as i64) as u16,
            h: num("h")?.clamp(0, u16::MAX as i64) as u16,
            contained_in: None,
        })),
        "E" => Some(Record::End),
        "X" => Some(Record::Clear),
        _ => None,
    }
}

/// Splits an output stream into terminal bytes and 7701 records, tolerating
/// records split across PTY reads.
#[derive(Default)]
pub struct Scanner {
    carry: String,
}

pub enum Piece {
    Bytes(String),
    Record(Record),
}

const INTRO: &str = "\x1b]7701;";

impl Scanner {
    pub fn push(&mut self, data: &str) -> Vec<Piece> {
        self.carry.push_str(data);
        let buf = std::mem::take(&mut self.carry);
        let mut out = Vec::new();
        let mut rest = buf.as_str();
        loop {
            match rest.find(INTRO) {
                Some(start) => {
                    let body = &rest[start + INTRO.len()..];
                    let end = body.find(['\x07', '\x1b']);
                    match end {
                        Some(e) if body[e..].starts_with('\x07') || body[e..].starts_with("\x1b\\") => {
                            if start > 0 {
                                out.push(Piece::Bytes(rest[..start].to_string()));
                            }
                            if let Some(r) = parse_record(&body[..e]) {
                                out.push(Piece::Record(r));
                            }
                            let term = if body[e..].starts_with('\x07') { 1 } else { 2 };
                            rest = &body[e + term..];
                        }
                        Some(e) if e + 1 < body.len() => {
                            // ESC not followed by '\': malformed; pass it through.
                            out.push(Piece::Bytes(rest[..start + INTRO.len() + e].to_string()));
                            rest = &body[e..];
                        }
                        _ => {
                            // Incomplete record: keep it for the next chunk.
                            if start > 0 {
                                out.push(Piece::Bytes(rest[..start].to_string()));
                            }
                            self.carry = rest[start..].to_string();
                            return out;
                        }
                    }
                }
                None => {
                    // Hold back a possible partial introducer at the tail.
                    let keep = (1..INTRO.len())
                        .rev()
                        .find(|&n| rest.len() >= n && rest.is_char_boundary(rest.len() - n) && INTRO.starts_with(&rest[rest.len() - n..]))
                        .unwrap_or(0);
                    let split = rest.len() - keep;
                    if split > 0 {
                        out.push(Piece::Bytes(rest[..split].to_string()));
                    }
                    self.carry = rest[split..].to_string();
                    return out;
                }
            }
        }
    }
}

fn contains(outer: &Region, inner: &Region) -> bool {
    outer.x <= inner.x
        && outer.y <= inner.y
        && outer.x + outer.w as i32 >= inner.x + inner.w as i32
        && outer.y + outer.h as i32 >= inner.y + inner.h as i32
        && (outer.w, outer.h) != (inner.w, inner.h)
}

/// Fill `contained_in` for regions without an explicit parent.
pub fn infer_containment(regions: &mut [Region]) {
    for i in 0..regions.len() {
        if regions[i].parent.is_some() {
            continue;
        }
        let best = (0..i).filter(|&j| contains(&regions[j], &regions[i])).min_by_key(|&j| regions[j].w as u32 * regions[j].h as u32);
        regions[i].contained_in = best.map(|j| regions[j].id.clone());
    }
}

#[cfg(feature = "ghostty-vt")]
fn run(path: &str, show: Option<u64>) -> Result<(), String> {
    use std::io::BufRead;

    use cleat::asciicast::{decode_event, decode_header, EventCode};
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut lines = std::io::BufReader::new(file).lines();
    let header = decode_header(&lines.next().ok_or("empty cast")?.map_err(|e| e.to_string())?)?;
    let mut engine = GhosttyVtEngine::new(header.cols, header.rows);
    let mut scanner = Scanner::default();
    let mut prev = std::time::Duration::ZERO;
    let mut pending: Option<(u64, i32, Vec<Region>)> = None;
    // A committed snapshot describes the frame drawn in the same synchronized
    // update, so it is paired with the grid when that update ends (or at once
    // when the emitter used no ?2026 bracket).
    let mut ready: Option<(u64, Vec<Region>)> = None;
    let mut committed = 0u64;
    let emit = |engine: &mut GhosttyVtEngine, t: f64, ready: &mut Option<(u64, Vec<Region>)>| -> Result<(), String> {
        if ready.is_none() || engine.synchronized_output_active()? {
            return Ok(());
        }
        let (frame, mut regions) = ready.take().expect("checked");
        infer_containment(&mut regions);
        let grid = engine.screen_grid()?;
        let rows: Vec<String> = (0..grid.rows).map(|r| grid.row_text(r)).collect();
        match show {
            None => print_json(t, frame, &rows, &regions),
            Some(n) if n == frame => print_human(frame, &rows, &regions),
            _ => {}
        }
        Ok(())
    };
    for line in lines {
        let line = line.map_err(|e| e.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        let event = decode_event(&line, &mut prev)?;
        let t = event.time.as_secs_f64();
        match event.code {
            EventCode::Resize => {
                if let Some((c, r)) = event.data.split_once('x') {
                    engine.resize(c.parse().unwrap_or(header.cols), r.parse().unwrap_or(header.rows))?;
                }
                continue;
            }
            EventCode::Output => {}
            _ => continue,
        }
        for piece in scanner.push(&event.data) {
            match piece {
                Piece::Bytes(b) => {
                    engine.feed(b.as_bytes())?;
                    let _ = engine.drain_replies(); // discard replies to the app's own queries
                    emit(&mut engine, t, &mut ready)?;
                }
                Piece::Record(Record::Begin { frame, cursor_relative }) => {
                    let row = if cursor_relative { cursor_row(&mut engine)? } else { 0 };
                    pending = Some((frame, row, Vec::new()));
                }
                Piece::Record(Record::Region(mut r)) => {
                    if let Some((_, origin, regions)) = pending.as_mut() {
                        r.y += *origin;
                        regions.push(r);
                    }
                }
                Piece::Record(Record::End) => {
                    if let Some((f, _, rs)) = pending.take() {
                        committed += 1;
                        // `f` is optional; fall back to the commit ordinal.
                        ready = Some((if f == 0 { committed } else { f }, rs));
                        emit(&mut engine, t, &mut ready)?;
                    }
                }
                Piece::Record(Record::Clear) => {
                    committed += 1;
                    ready = Some((committed, Vec::new()));
                    emit(&mut engine, t, &mut ready)?;
                }
            }
        }
    }
    eprintln!("{committed} region frames");
    Ok(())
}

/// The grid hides the cursor position when the cursor is hidden (as TUIs
/// usually keep it), so ask the engine with a CPR query instead.
#[cfg(feature = "ghostty-vt")]
fn cursor_row(engine: &mut GhosttyVtEngine) -> Result<i32, String> {
    let _ = engine.drain_replies();
    engine.feed(b"\x1b[6n")?;
    let reply = String::from_utf8_lossy(&engine.drain_replies()).into_owned();
    let row = reply.trim_start_matches("\x1b[").split(';').next().and_then(|r| r.parse::<i32>().ok());
    row.map(|r| r - 1).ok_or_else(|| format!("no cursor position report: {reply:?}"))
}

fn crop(rows: &[String], r: &Region, line: i32) -> String {
    let y = r.y + line;
    if y < 0 || y as usize >= rows.len() {
        return String::new();
    }
    rows[y as usize].chars().skip(r.x.max(0) as usize).take(r.w as usize).collect()
}

fn print_human(frame: u64, rows: &[String], regions: &[Region]) {
    println!("frame {frame}");
    for (i, row) in rows.iter().enumerate() {
        println!("{i:>3} |{row}|");
    }
    for r in regions {
        let up = r.parent.as_deref().or(r.contained_in.as_deref()).unwrap_or("-");
        println!(
            "{:<26} {:<10} x={:<3} y={:<3} w={:<3} h={:<3} in={:<20} top={:?}",
            r.id,
            r.kind,
            r.x,
            r.y,
            r.w,
            r.h,
            up,
            crop(rows, r, 0).trim_end()
        );
    }
}

fn print_json(t: f64, frame: u64, rows: &[String], regions: &[Region]) {
    let regions: Vec<serde_json::Value> = regions
        .iter()
        .map(|r| {
            serde_json::json!({
                "id": r.id, "parent": r.parent, "contained_in": r.contained_in, "kind": r.kind,
                "name": r.name, "x": r.x, "y": r.y, "w": r.w, "h": r.h,
            })
        })
        .collect();
    println!("{}", serde_json::json!({ "t": t, "frame": frame, "rows": rows, "regions": regions }));
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(path) = args.get(1) else {
        eprintln!("usage: region_cast CAST [--frame N]");
        std::process::exit(2);
    };
    let show = args.iter().position(|a| a == "--frame").and_then(|i| args.get(i + 1)).and_then(|n| n.parse().ok());
    #[cfg(feature = "ghostty-vt")]
    if let Err(e) = run(path, show) {
        eprintln!("region_cast: {e}");
        std::process::exit(1);
    }
    #[cfg(not(feature = "ghostty-vt"))]
    {
        let _ = (path, show);
        eprintln!("region_cast needs the ghostty-vt feature");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn records(chunks: &[&str]) -> (String, Vec<Record>) {
        let mut s = Scanner::default();
        let (mut bytes, mut recs) = (String::new(), Vec::new());
        for c in chunks {
            for p in s.push(c) {
                match p {
                    Piece::Bytes(b) => bytes.push_str(&b),
                    Piece::Record(r) => recs.push(r),
                }
            }
        }
        (bytes, recs)
    }

    #[test]
    fn records_split_across_reads_are_reassembled() {
        let whole = "ab\x1b]7701;B;f=3;o=a\x1b\\cd\x1b]7701;R;id=x;kind=Block;x=1;y=2;w=3;h=4\x07\x1b]7701;E\x1b\\ef";
        let (b1, r1) = records(&[whole]);
        for split in 1..whole.len() {
            let (b2, r2) = records(&[&whole[..split], &whole[split..]]);
            assert_eq!((&b1, &r1), (&b2, &r2), "split at {split}");
        }
        assert_eq!(b1, "abcdef");
        assert_eq!(r1.len(), 3);
    }

    #[test]
    fn other_oscs_pass_through_to_the_terminal() {
        let (bytes, recs) = records(&["\x1b]133;A\x07\x1b]0;title\x07"]);
        assert_eq!(bytes, "\x1b]133;A\x07\x1b]0;title\x07");
        assert!(recs.is_empty());
    }

    #[test]
    fn escaped_values_round_trip() {
        let Some(Record::Region(r)) = parse_record("R;id=a\\x3bb;name=x\\x3dy;kind=K;x=0;y=-1;w=2;h=1") else { panic!() };
        assert_eq!((r.id.as_str(), r.name.as_deref(), r.y), ("a;b", Some("x=y"), -1));
    }

    #[test]
    fn containment_picks_the_smallest_enclosing_region() {
        let r = |id: &str, x, y, w, h| Region { id: id.into(), x, y, w, h, ..Region::default() };
        let mut rs = vec![r("app", 0, 0, 100, 30), r("panel", 1, 1, 40, 20), r("text", 2, 2, 10, 1), r("popup", 30, 10, 30, 5)];
        infer_containment(&mut rs);
        let got: Vec<_> = rs.iter().map(|r| r.contained_in.as_deref()).collect();
        assert_eq!(got, [None, Some("app"), Some("panel"), Some("app")]);
    }
}
