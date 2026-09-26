use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use super::*;
use crate::{
    packet::{DirectoryDelta, PacketFrame, MSG_CONTROL_DIRECTORY_DELTA},
    platform::ipc::{bind_session_listener, set_listener_nonblocking},
};

struct OldDaemon {
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl OldDaemon {
    fn start(layout: &RuntimeLayout, supports_drain: bool) -> Self {
        layout.ensure_daemon_dirs().unwrap();
        let listener = bind_session_listener(&layout.socket_path()).unwrap();
        set_listener_nonblocking(&listener, true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let generation = layout.generation();
        let worker = thread::spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        set_stream_read_timeout(&stream, Some(Duration::from_secs(2))).unwrap();
                        let request = http_uds::read_http_request_for_test(&mut stream);
                        let (status, body) = if request.starts_with("GET / HTTP") || request.starts_with("GET /healthz HTTP") {
                            let mut build = crate::build_info::BuildInfo::current();
                            build.git_sha = Some("previous-build".into());
                            (StatusCode::OK, serde_json::json!({"build": build, "generation": generation}))
                        } else if request.starts_with("GET /sessions HTTP") {
                            (StatusCode::OK, serde_json::json!({"sessions": []}))
                        } else if request.starts_with("POST /drain HTTP") && supports_drain {
                            (StatusCode::OK, serde_json::json!({"drain_state": "draining", "session_count": 0}))
                        } else {
                            (StatusCode::NOT_FOUND, serde_json::json!({"error": "not found"}))
                        };
                        http_uds::write_json(&mut stream, status, &body).unwrap();
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(5)),
                    Err(err) => panic!("accept: {err}"),
                }
            }
        });
        Self { stop, worker: Some(worker) }
    }
}

impl Drop for OldDaemon {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(Instant::now() < deadline, "condition timed out");
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn drain_rolls_only_once_even_with_concurrent_callers() {
    let temp = tempfile::tempdir().unwrap();
    let layout = RuntimeLayout::new(temp.path().to_path_buf());
    let old = layout.prepare_generation().unwrap();
    let _host = OldDaemon::start(&old, true);
    let service = SessionService::new(layout.clone());
    let results = thread::scope(|scope| {
        let first = scope.spawn(|| service.drain().unwrap());
        let second = scope.spawn(|| service.drain().unwrap());
        [first.join().unwrap(), second.join().unwrap()]
    });
    assert_eq!(results.iter().filter(|report| report.changed).count(), 1);
    assert_eq!(layout.generation(), Some(2));
    let report = results.iter().find(|report| report.changed).unwrap();
    assert_eq!(report.old.build.as_ref().unwrap().git_sha.as_deref(), Some("previous-build"));
    assert_eq!(report.current.build.as_ref(), Some(&report.installed));
    assert!(report.warning.is_none());
    assert!(!service.drain().unwrap().changed);
    service.daemon_request(Method::POST, "/drain").unwrap();
}

#[test]
fn legacy_roll_preserves_socket_directory_and_warns_about_unsupported_drain() {
    let temp = tempfile::tempdir().unwrap();
    let layout = RuntimeLayout::new(temp.path().to_path_buf());
    let legacy = layout.clone().with_daemon("default@legacy".into()).unwrap();
    let _host = OldDaemon::start(&legacy, false);
    std::fs::create_dir_all(legacy.session_dir("recorded")).unwrap();
    std::fs::write(legacy.session_dir("recorded").join("session.cast"), "history").unwrap();
    let service = SessionService::new(layout.clone());
    let report = service.drain().unwrap();
    assert!(report.changed);
    assert_eq!(report.old.generation, None);
    assert_eq!(report.current.generation, Some(1));
    assert!(report.warning.unwrap().contains("cannot be told to drain"));
    assert!(legacy.socket_path().exists());
    assert_eq!(service.for_session("recorded").unwrap().session_dir("recorded"), legacy.session_dir("recorded"));
    assert_eq!(std::fs::read_to_string(legacy.session_dir("recorded").join("session.cast")).unwrap(), "history");
    assert!(!service.drain().unwrap().changed);
    assert_eq!(layout.prepare_generation().unwrap().generation(), Some(1));
    service.daemon_request(Method::POST, "/drain").unwrap();
}

#[test]
fn failed_start_or_health_check_never_publishes_successor() {
    for fail_start in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let layout = RuntimeLayout::new(temp.path().to_path_buf());
        let old = layout.prepare_generation().unwrap();
        let _host = OldDaemon::start(&old, true);
        let service = SessionService::new(layout.clone());
        let err = service
            .drain_using(
                |successor| {
                    assert_eq!(layout.generation(), Some(1));
                    assert_eq!(successor.generation(), Some(2));
                    if fail_start {
                        Err("test startup failure".into())
                    } else {
                        Ok(())
                    }
                },
                Duration::from_millis(40),
            )
            .unwrap_err();
        assert!(err.contains(if fail_start { "test startup failure" } else { "failed health check" }), "{err}");
        assert_eq!(layout.generation(), Some(1));
        assert!(service.daemon_build_status().is_ok());
    }
}

#[test]
fn draining_keeps_attachments_rejects_new_ids_and_exits_with_recordings_intact() {
    for record in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let layout = RuntimeLayout::new(temp.path().to_path_buf());
        let service = SessionService::new(layout.clone());
        let command = if cfg!(windows) { "cmd.exe /D /C ping -n 120 127.0.0.1 >NUL" } else { "sleep 120" };
        service.create(Some("old".into()), Some(VtEngineKind::Passthrough), None, Some(command.into()), record).unwrap();
        let old_layout = layout.resolved().unwrap();
        let old_service = SessionService::new(old_layout.clone());
        let (mut subscriber, directory) = crate::provider_daemon::connect_packet_stream(&old_layout, &[]).unwrap();
        set_stream_read_timeout(&subscriber, Some(Duration::from_secs(2))).unwrap();
        assert_eq!(directory.daemon.unwrap().drain_state, "serving");
        assert!(!service.drain().unwrap().changed);
        // Simulate the alias publication; the daemon request/lifecycle is independent
        // of the installed-build comparison covered by the orchestration tests.
        let successor = layout.clone().with_daemon("default@2".into()).unwrap();
        crate::platform::daemon::spawn_daemon_process(successor.root(), successor.daemon_name()).unwrap();
        let successor_service = SessionService::new(successor.clone());
        wait_until(|| successor_service.daemon_status_at("/healthz").is_ok());
        layout.set_current_generation(2).unwrap();
        let response = old_service.daemon_request(Method::POST, "/drain").unwrap();
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(old_service.daemon_status_at("/healthz").unwrap().drain_state, "draining");
        let delta = loop {
            let frame = PacketFrame::read(&mut subscriber).unwrap();
            if frame.msg_type == MSG_CONTROL_DIRECTORY_DELTA {
                let delta = frame.decode::<DirectoryDelta>().unwrap();
                if delta.daemon.is_some() {
                    break delta;
                }
            }
        };
        assert_eq!(delta.daemon.unwrap().drain_state, "draining");
        let (_, snapshot) = crate::provider_daemon::connect_packet_stream(&old_layout, &[]).unwrap();
        assert_eq!(snapshot.daemon.unwrap().drain_state, "draining");
        let old_entry = service.discover_daemons().into_iter().find(|d| d.runtime_root == temp.path() && d.generation == Some(1)).unwrap();
        assert_eq!(old_entry.drain_state, "draining");
        let error =
            old_service.create(Some("refused".into()), Some(VtEngineKind::Passthrough), None, Some(command.into()), false).unwrap_err();
        assert!(error.contains("default@2"), "{error}");
        assert!(!old_layout.session_dir("refused").exists());
        let owner = service.for_session("old").unwrap();
        let (_, attachment) =
            owner.attach(Some("old".into()), Some(VtEngineKind::Passthrough), None, None, false, Default::default()).unwrap();
        drop(attachment);
        service.create(Some("new".into()), Some(VtEngineKind::Passthrough), None, Some(command.into()), false).unwrap();
        let (_, attachment) =
            service.for_session("new").unwrap().attach(Some("new".into()), None, None, None, true, Default::default()).unwrap();
        drop(attachment);
        assert!(successor.session_dir("new").is_dir());
        owner.kill("old").unwrap();
        wait_until(|| !old_layout.daemon_dir().exists() || !is_session_daemon_alive(old_layout.root(), old_layout.daemon_name()));
        if record {
            assert!(old_layout.session_dir("old").join("session.cast").exists());
            assert!(old_layout.daemon_pid_path().exists());
        } else {
            assert!(!old_layout.daemon_dir().exists());
        }
        service.kill("new").unwrap();
        successor_service.daemon_request(Method::POST, "/drain").unwrap();
    }
}

#[test]
fn stalled_status_response_has_a_bounded_deadline() {
    let temp = tempfile::tempdir().unwrap();
    let layout = RuntimeLayout::new(temp.path().to_path_buf());
    layout.ensure_daemon_dirs().unwrap();
    let listener = bind_session_listener(&layout.socket_path()).unwrap();
    set_listener_nonblocking(&listener, true).unwrap();
    let (release, released) = std::sync::mpsc::channel();
    let host = thread::spawn(move || {
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(pair) => break pair,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(5)),
                Err(err) => panic!("accept: {err}"),
            }
        };
        http_uds::read_http_request_for_test(&mut stream);
        // Keep the peer open without responding; the client must time out itself.
        let _ = released.recv_timeout(Duration::from_secs(5));
    });
    let started = Instant::now();
    assert!(SessionService::new(layout).daemon_status_at("/healthz").is_err());
    assert!(started.elapsed() < Duration::from_secs(4));
    release.send(()).unwrap();
    host.join().unwrap();
}

#[test]
fn mismatched_successor_build_does_not_move_alias() {
    let temp = tempfile::tempdir().unwrap();
    let layout = RuntimeLayout::new(temp.path().to_path_buf());
    let old = layout.prepare_generation().unwrap();
    let _host = OldDaemon::start(&old, true);
    let mut successor_host = None;
    let error = SessionService::new(layout.clone())
        .drain_using(
            |successor| {
                successor_host = Some(OldDaemon::start(successor, true));
                Ok(())
            },
            Duration::from_secs(1),
        )
        .unwrap_err();
    assert!(error.contains("build does not match"), "{error}");
    assert_eq!(layout.generation(), Some(1));
}
