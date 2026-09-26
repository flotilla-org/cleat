//! Socket benchmark and scheduling contracts for the packet output seam.
use std::{os::unix::net::UnixStream, sync::atomic::AtomicUsize};

use super::*;
use crate::{
    image_backing::{ImageBacking, RetainedImage},
    image_delivery::{Image, ImageReceiver},
    provider::TerminalImageResource,
};

fn image() -> Image {
    Arc::new(RetainedImage { image_id: 1, generation: 1, backing: ImageBacking::Bytes(vec![69; 1920 * 1080 * 4]) })
}
fn channel(id: u32, image: Image) -> PacketSessionChannel {
    let update = TerminalRenderUpdate {
        image_resources: vec![TerminalImageResource {
            image_id: 1,
            generation: 1,
            width_px: 1920,
            height_px: 1080,
            format: 1,
            data_len: image.bytes().len(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut resident = HashSet::new();
    let transfer = ImageTransfer::new(id, RenderBundle::live(update, vec![image]), &mut resident).unwrap().local_files(false);
    PacketSessionChannel {
        session_id: "benchmark".into(),
        role: ChannelRole::Watcher,
        requested_role: ChannelRole::Watcher,
        identity: Default::default(),
        denial_reason: None,
        in_flight_generation: Some(1),
        last_sent_generation: 1,
        last_source_generation: 1,
        history: false,
        view_changed: false,
        view_state: Default::default(),
        next_capture: Instant::now(),
        local_images: false,
        image_resident: resident,
        image_transfer: Some(transfer),
    }
}
fn client(id: u64, channels: u32, image: &Image) -> (PacketClient, UnixStream) {
    let (stream, peer) = UnixStream::pair().unwrap();
    stream.set_nonblocking(true).unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    let mut client = PacketClient::new(id, stream, vec![], None, &DirectorySnapshot { daemon: None, sessions: vec![] }, None).unwrap();
    for id in 1..=channels {
        client.channels.insert(id, channel(id, image.clone()));
    }
    (client, peer)
}

#[test]
#[ignore = "manual socket benchmark; run with --release --ignored --nocapture"]
fn benchmark_image_fallback() {
    let scenario = std::env::var("CLEAT_IMAGE_BENCH").unwrap_or_else(|_| "single".into());
    let (connections, channels) = match scenario.as_str() {
        "single" | "roomy" => (1, 1),
        "multi" => (1, 4),
        "slow" => (1, 4),
        "stalled" => (2, 1),
        _ => panic!("unknown scenario"),
    };
    let source = image();
    let done = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(AtomicBool::new(false));
    let start = Instant::now();
    let mut clients = Vec::new();
    let mut readers = Vec::new();
    for index in 0..connections {
        let (client, mut peer) = client(index as u64, channels, &source);
        if scenario == "roomy" {
            use std::os::fd::AsRawFd;
            let size: libc::c_int = 256 * 1024;
            assert_eq!(
                unsafe {
                    libc::setsockopt(
                        client.stream.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_SNDBUF,
                        (&size as *const libc::c_int).cast(),
                        std::mem::size_of_val(&size) as libc::socklen_t,
                    )
                },
                0
            );
        }
        clients.push(client);
        let done = done.clone();
        let finished = finished.clone();
        let scenario = scenario.clone();
        readers.push(thread::spawn(move || {
            if scenario == "stalled" && index == 0 {
                thread::sleep(Duration::from_millis(350));
            }
            let mut receivers: HashMap<u32, ImageReceiver> = HashMap::new();
            let mut completion = Vec::new();
            let mut probe = None;
            let mut latency = None;
            while completion.len() < channels as usize || latency.is_none() {
                let frame = PacketFrame::read(&mut peer).unwrap();
                if probe.is_none() {
                    probe = Some(Instant::now());
                    PacketFrame::new(1, MSG_SESSION_INPUT, &Input { event: TerminalInputEvent::RawBytes(vec![b'x']) })
                        .unwrap()
                        .write(&mut peer)
                        .unwrap();
                }
                match frame.msg_type {
                    crate::packet::MSG_SESSION_IMAGE => receivers.entry(frame.channel).or_default().chunk(frame.decode().unwrap()).unwrap(),
                    MSG_SESSION_RENDER => {
                        let render: RenderPacket = frame.decode().unwrap();
                        let images = receivers.entry(frame.channel).or_default().commit(&render.update.image_resources).unwrap();
                        assert_eq!(images[0].bytes().len(), 1920 * 1080 * 4);
                        completion.push((frame.channel, start.elapsed().as_secs_f64() * 1000.0));
                        done.fetch_add(1, Ordering::SeqCst);
                    }
                    MSG_CONTROL_DIRECTORY_DELTA => {
                        latency = Some(probe.unwrap().elapsed().as_secs_f64() * 1000.0);
                    }
                    _ => panic!("unexpected frame"),
                }
                if scenario == "slow" {
                    thread::sleep(Duration::from_millis(2));
                }
            }
            while !finished.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(1));
            }
            (completion, latency.unwrap())
        }));
    }
    let mut max_queue = 0;
    let mut passes = 0;
    let mut max_service_us = 0;
    while done.load(Ordering::SeqCst) < (connections * channels) as usize {
        assert!(start.elapsed() < Duration::from_secs(15));
        let service = Instant::now();
        for client in &mut clients {
            let mut frames = VecDeque::new();
            assert!(client.drain_input_frames(&mut frames, Duration::ZERO).unwrap());
            for frame in frames {
                assert_eq!(frame.msg_type, MSG_SESSION_INPUT);
                client
                    .enqueue_control(MSG_CONTROL_DIRECTORY_DELTA, &DirectoryDelta {
                        daemon: None,
                        upserted: vec![],
                        removed_session_ids: vec![],
                    })
                    .unwrap();
            }
        }
        flush_packet_clients(&mut clients);
        for client in &clients {
            assert!(!client.dead);
            max_queue = max_queue.max(client.pending_output.len());
        }
        max_service_us = max_service_us.max(service.elapsed().as_micros());
        passes += 1;
        wait_packet_output(&clients);
    }
    finished.store(true, Ordering::SeqCst);
    let results: Vec<_> = readers.into_iter().map(|t| t.join().unwrap()).collect();
    let elapsed = results.iter().flat_map(|r| r.0.iter().map(|c| c.1)).fold(0.0, f64::max);
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) }, 0);
    let usage = unsafe { usage.assume_init() };
    let rss_bytes = if cfg!(target_os = "macos") { usage.ru_maxrss as u64 } else { usage.ru_maxrss as u64 * 1024 };
    eprintln!("scenario={scenario} results_ms={results:?} total_ms={elapsed:.2} payload_MBps={:.2} peak_queue_bytes={max_queue} max_service_us={max_service_us} passes={passes} peak_process_RSS_bytes={rss_bytes}",(connections*channels) as f64*8.2944/(elapsed/1000.0));
}

#[test]
fn image_prefetch_is_bounded_and_rotates_across_service_passes() {
    let (mut client, _peer) = client(1, 12, &image());
    let mut counts = HashMap::<u32, usize>::new();
    for _ in 0..10 {
        client.queue_image_frames(|| Duration::ZERO).unwrap();
        assert!(client.pending_output.len() <= IMAGE_OUTPUT_HIGH_WATER + crate::image_delivery::IMAGE_CHUNK_BYTES + 64);
        let mut bytes = client.pending_output.as_slice().to_vec();
        client.pending_output = PendingOutput::new();
        while let Some(frame) = PacketFrame::read_from_buffer(&mut bytes).unwrap() {
            assert_eq!(frame.msg_type, crate::packet::MSG_SESSION_IMAGE);
            *counts.entry(frame.channel).or_default() += 1;
        }
        let min = (1..=12).map(|id| *counts.get(&id).unwrap_or(&0)).min().unwrap();
        let max = counts.values().copied().max().unwrap();
        assert!(max - min <= 1, "unfair frame counts: {counts:?}");
    }
    assert_eq!(counts.len(), 12);
    // A blocked socket must stop allocating chunks; room remains for control.
    client.pending_output = PendingOutput::from(vec![0; IMAGE_OUTPUT_HIGH_WATER]);
    let cursor = client.image_output_cursor;
    client.queue_image_frames(|| Duration::ZERO).unwrap();
    assert_eq!(client.pending_output.len(), IMAGE_OUTPUT_HIGH_WATER);
    assert_eq!(client.image_output_cursor, cursor);
    client
        .enqueue_control(MSG_CONTROL_DIRECTORY_DELTA, &DirectoryDelta { daemon: None, upserted: vec![], removed_session_ids: vec![] })
        .unwrap();
    assert!(!client.dead);
}

#[test]
fn image_time_budget_yields_without_losing_the_next_channel() {
    let (mut client, _peer) = client(1, 2, &image());
    // Even an already exhausted soft budget must make one frame of progress.
    client.queue_image_frames(|| PACKET_OUTPUT_TIME_BUDGET).unwrap();
    let mut bytes = client.pending_output.as_slice().to_vec();
    let first = PacketFrame::read_from_buffer(&mut bytes).unwrap().unwrap();
    assert!(bytes.is_empty(), "budget permits only one frame");
    client.pending_output = PendingOutput::new();
    client.queue_image_frames(|| PACKET_OUTPUT_TIME_BUDGET).unwrap();
    let mut bytes = client.pending_output.as_slice().to_vec();
    let second = PacketFrame::read_from_buffer(&mut bytes).unwrap().unwrap();
    assert!(bytes.is_empty(), "budget permits only one frame");
    assert_ne!(first.channel, second.channel);
}

#[test]
fn ready_assets_remain_writable_work_but_file_ack_waits_do_not() {
    let (mut client, _peer) = client(1, 1, &image());
    assert!(client.pending_output.is_empty());
    assert!(client.has_pending_output());
    let image = RetainedImage::from_owned(crate::provider::TerminalImageBytes { image_id: 1, generation: 1, bytes: vec![1; 4] });
    let mut ch = channel(1, image);
    ch.image_transfer = ch.image_transfer.take().map(|t| t.local_files(true));
    client.channels.insert(1, ch);
    client.queue_image_frames(|| Duration::ZERO).unwrap();
    assert_eq!(
        PacketFrame::read(&mut std::io::Cursor::new(client.pending_output.as_slice())).unwrap().msg_type,
        crate::packet::MSG_SESSION_IMAGE_FILE
    );
    client.pending_output = PendingOutput::new();
    assert!(!client.has_pending_output());
    client
        .channels
        .get_mut(&1)
        .unwrap()
        .image_transfer
        .as_mut()
        .unwrap()
        .file_result(crate::packet::ImageFileResult { image_id: 1, generation: 1, acquired: true })
        .unwrap();
    assert!(client.has_pending_output());
    client.queue_image_frames(|| Duration::ZERO).unwrap();
    assert_eq!(PacketFrame::read(&mut std::io::Cursor::new(client.pending_output.as_slice())).unwrap().msg_type, MSG_SESSION_RENDER);
    client.pending_output = PendingOutput::new();
    assert!(!client.has_pending_output());
}
