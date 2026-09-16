//! Packet input → PTY → VT → rendered update round-trip measurement.
//! --history adds two browsing watchers and continuous PTY output.
//! Build the matching cleat binary and set CARGO_BIN_EXE_cleat to its path.
#[cfg(unix)]
fn main() {
    use std::{
        io::{Read, Write},
        os::unix::net::UnixStream,
        time::{Duration, Instant},
    };

    use cleat::{
        packet::{ChannelRole, PacketClient, PacketFrame},
        provider::TerminalInputEvent,
        runtime::{RuntimeLayout, TerminalSize},
        server::SessionService,
        vt::VtEngineKind,
    };
    let browsing = std::env::args().any(|arg| arg == "--history");
    let temp = tempfile::Builder::new().prefix("cleat-latency-").tempdir_in("/tmp").unwrap();
    let temp = BenchmarkEnvironment(temp);
    let layout = RuntimeLayout::new(temp.0.path().to_path_buf());
    let service = SessionService::new(layout.clone());
    service
        .create_with_size(
            Some("latency".into()),
            Some(VtEngineKind::Ghostty),
            None,
            Some(if browsing {
                "sh -c 'stty raw -echo; i=0; while [ $i -lt 80 ]; do echo seed-$i; i=$((i+1)); done; printf READY; (while sleep 0.01; do printf \"tick\\r\\n\"; done) & exec cat'".into()
            } else { "sh -c 'stty raw -echo; printf READY; exec cat'".into() }),
            false,
            TerminalSize { cols: 120, rows: 40 },
        )
        .unwrap();
    let ready = Instant::now();
    while !service.capture("latency").unwrap().contains("READY") {
        assert!(ready.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut stream = UnixStream::connect(layout.socket_path()).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    stream
        .write_all(b"POST /connect HTTP/1.1\r\nHost: cleat\r\nContent-Length: 0\r\nConnection: Upgrade\r\nUpgrade: cleat-packet/1\r\n\r\n")
        .unwrap();
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        header.push(byte[0]);
    }
    assert!(header.starts_with(b"HTTP/1.1 101"));
    PacketFrame::read(&mut stream).unwrap();
    PacketFrame::read(&mut stream).unwrap();
    let mut client = PacketClient::new(stream);
    client.open_channel(1, "latency", ChannelRole::Controller).unwrap();
    let initial = client.read_render(1).unwrap();
    client.ack(1, initial.update.render_generation).unwrap();
    if browsing {
        for channel in [2, 3] {
            client.open_channel(channel, "latency", ChannelRole::Watcher).unwrap();
            loop {
                let frame = client.read_frame().unwrap();
                if frame.msg_type == cleat::packet::MSG_SESSION_RENDER {
                    let update = frame.decode::<cleat::packet::RenderPacket>().unwrap().update;
                    client.ack(frame.channel, update.render_generation).unwrap();
                    if frame.channel == channel {
                        break;
                    }
                }
            }
            client
                .viewport(
                    channel,
                    if channel == 2 { cleat::provider::ViewportCommand::Top } else { cleat::provider::ViewportCommand::DeltaRows(-10) },
                )
                .unwrap();
        }
    }
    let mut history_frames = 0;
    let mut samples = Vec::new();
    let iterations = std::env::var("CLEAT_LATENCY_SAMPLES").ok().and_then(|value| value.parse::<usize>().ok()).unwrap_or(500).max(1);
    let warmup = iterations.min(100);
    for n in 0..iterations + warmup {
        if n % 100 == 0 {
            eprintln!("sample {n}, history frames {history_frames}");
        }
        let start = Instant::now();
        let marker = format!("marker-{n:04}");
        let bytes = if browsing { format!("{marker}\r\n").into_bytes() } else { vec![b'\r', if n % 2 == 0 { b'A' } else { b'B' }] };
        client.input(1, TerminalInputEvent::RawBytes(bytes)).unwrap();
        let elapsed = loop {
            let frame = client.read_frame().unwrap();
            if frame.msg_type != cleat::packet::MSG_SESSION_RENDER {
                continue;
            }
            let update = frame.decode::<cleat::packet::RenderPacket>().unwrap().update;
            client.ack(frame.channel, update.render_generation).unwrap();
            if frame.channel != 1 {
                history_frames += 1;
                if frame.channel == 3 {
                    for command in [cleat::provider::ViewportCommand::Bottom, cleat::provider::ViewportCommand::DeltaRows(-10)] {
                        client.viewport(3, command).unwrap();
                    }
                }
                continue;
            }
            if !browsing {
                break start.elapsed().as_micros();
            }
            let text: String = update
                .ops
                .iter()
                .flat_map(|op| &op.rows)
                .flat_map(|row| &row.cells)
                .flat_map(|cell| &cell.graphemes)
                .filter_map(|cp| char::from_u32(*cp))
                .collect();
            if text.contains(&marker) {
                break start.elapsed().as_micros();
            }
            assert!(start.elapsed() < Duration::from_secs(5), "input marker not rendered");
        };
        if n >= warmup {
            samples.push(elapsed);
        }
    }
    samples.sort_unstable();
    println!(
        "n={} median_us={} p95_us={} history_frames={}",
        samples.len(),
        samples[samples.len() / 2],
        samples[samples.len() * 95 / 100],
        history_frames
    );
    drop(client);
    service.kill("latency").unwrap();
}

#[cfg(not(unix))]
fn main() {
    eprintln!("This measurement harness requires Unix sockets.");
}

#[cfg(unix)]
struct BenchmarkEnvironment(tempfile::TempDir);

#[cfg(unix)]
impl Drop for BenchmarkEnvironment {
    fn drop(&mut self) {
        let service = cleat::server::SessionService::new(cleat::runtime::RuntimeLayout::new(self.0.path().to_path_buf()));
        let _ = service.kill("latency");
        cleat::platform::daemon::terminate_session_daemon_if_expected(self.0.path(), cleat::runtime::DEFAULT_DAEMON_NAME);
    }
}
