//! Manual measurements of the real packet renderer, excluding VT parsing and transport encoding.
//! Run with `cargo test -p cleat --release --locked packet_repaint_measurements -- --ignored --nocapture`.

use std::{hint::black_box, time::Instant};

use super::PacketTerminalRenderer;
use crate::provider::{
    TerminalCellWidth, TerminalRenderCell, TerminalRenderRow, TerminalRenderUpdate, TerminalRenderUpdateOp, TerminalRenderUpdateOpKind,
};

fn workload(mixed: bool) -> TerminalRenderUpdate {
    let mut cells = Vec::new();
    while cells.len() < 120 {
        // Ten source columns: ASCII, VS16 cloud (legacy narrow), CJK wide + tail,
        // a combining cluster, and more ASCII. Width disagreement is intentional.
        let pattern = if mixed { vec!["a", "b", "☁️", "界", "", "e\u{301}", "c", "d", "e", "f"] } else { vec!["a"; 10] };
        for (index, text) in pattern.into_iter().enumerate() {
            let mut cell = TerminalRenderCell { graphemes: text.chars().map(u32::from).collect(), ..Default::default() };
            if mixed && index == 3 {
                cell.style.width = TerminalCellWidth::Wide;
            } else if mixed && index == 4 {
                cell.style.width = TerminalCellWidth::SpacerTail;
            }
            cells.push(cell);
        }
    }
    TerminalRenderUpdate {
        cols: 120,
        rows: 40,
        ops: vec![TerminalRenderUpdateOp {
            kind: TerminalRenderUpdateOpKind::RowReplace,
            rows: (0..40).map(|row| TerminalRenderRow { row, cells: cells.clone(), ..Default::default() }).collect(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[cfg(unix)]
fn constrained_link(frame: &[u8]) -> (f64, f64) {
    use std::{
        io::{Read, Write},
        os::unix::net::UnixStream,
        thread,
        time::Duration,
    };
    const RATE: f64 = 128.0 * 1024.0;
    const FRAMES: usize = 3;
    let (mut sender, mut receiver) = UnixStream::pair().unwrap();
    let start = Instant::now();
    let reader = thread::spawn(move || {
        let mut received = 0;
        let mut buffer = [0; 1024];
        loop {
            let count = receiver.read(&mut buffer).unwrap();
            if count == 0 {
                break;
            }
            received += count;
            let deadline = Duration::from_secs_f64(received as f64 / RATE);
            thread::sleep(deadline.saturating_sub(start.elapsed()));
        }
        received
    });
    for _ in 0..FRAMES {
        sender.write_all(frame).unwrap();
    }
    drop(sender);
    assert_eq!(reader.join().unwrap(), frame.len() * FRAMES);
    let elapsed = start.elapsed().as_secs_f64();
    (FRAMES as f64 / elapsed, frame.len() as f64 * FRAMES as f64 / elapsed)
}

#[test]
#[ignore = "manual release-mode throughput and rate-limited link measurement"]
fn packet_repaint_measurements() {
    for (name, mixed) in [("dense_ascii", false), ("mixed_unicode", true)] {
        let update = workload(mixed);
        let mut renderer = PacketTerminalRenderer::new(120, 40);
        let mut output = Vec::new();
        for _ in 0..20 {
            output.clear();
            renderer.apply_and_render(&mut output, &update).unwrap();
        }
        let start = Instant::now();
        const FRAMES: usize = 2000;
        for _ in 0..FRAMES {
            output.clear();
            renderer.apply_and_render(&mut output, black_box(&update)).unwrap();
            black_box(&output);
        }
        let elapsed = start.elapsed().as_secs_f64();
        println!(
            "{name}: bytes/repaint={} render_repaints/s={:.1} render_MiB/s={:.2}",
            output.len(),
            FRAMES as f64 / elapsed,
            output.len() as f64 * FRAMES as f64 / elapsed / 1048576.0
        );
        #[cfg(unix)]
        {
            let (fps, bytes) = constrained_link(&output);
            println!("{name}: constrained_128KiB/s_repaints/s={fps:.3} measured_KiB/s={:.2}", bytes / 1024.0);
        }
    }
}
