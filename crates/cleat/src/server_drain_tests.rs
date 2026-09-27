use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
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
    _lifetime: std::fs::File,
    fail_drain: Arc<AtomicBool>,
    drain_requests: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl OldDaemon {
    fn start(layout: &RuntimeLayout, supports_drain: bool) -> Self {
        let lifetime = layout.try_lock_daemon_lifetime().unwrap().unwrap();
        layout.ensure_daemon_dirs().unwrap();
        let listener = bind_session_listener(&layout.socket_path()).unwrap();
        set_listener_nonblocking(&listener, true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let fail_drain = Arc::new(AtomicBool::new(false));
        let drain_requests = Arc::new(AtomicUsize::new(0));
        let fail = fail_drain.clone();
        let requests = drain_requests.clone();
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
                            requests.fetch_add(1, Ordering::SeqCst);
                            if fail.load(Ordering::SeqCst) {
                                (StatusCode::INTERNAL_SERVER_ERROR, serde_json::json!({"error": "injected drain failure"}))
                            } else {
                                (StatusCode::OK, serde_json::json!({"drain_state": "draining", "session_count": 0}))
                            }
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
        Self { _lifetime: lifetime, stop, worker: Some(worker), fail_drain, drain_requests }
    }
}

impl Drop for OldDaemon {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn wait_until(label: &str, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(Instant::now() < deadline, "timed out waiting for {label}");
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
        assert!(!temp.path().join("default@2").exists());
        assert_eq!(layout.allocate_generation().unwrap().generation(), Some(3));
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
        wait_until("successor health", || successor_service.daemon_status_at("/healthz").is_ok());
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
        wait_until("drained daemon exit", || {
            !old_layout.daemon_dir().exists() || !is_session_daemon_alive(old_layout.root(), old_layout.daemon_name())
        });
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

#[test]
fn auto_start_cannot_resurrect_a_retired_generation_during_or_after_cleanup() {
    for registration_remains in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let layout = RuntimeLayout::new(temp.path().to_path_buf());
        let old = layout.prepare_generation().unwrap();
        if registration_remains {
            // Model the cleanup window: our process is live, but its socket is gone.
            std::fs::write(old.daemon_pid_path(), std::process::id().to_string()).unwrap();
        } else {
            std::fs::remove_dir_all(old.daemon_dir()).unwrap();
        }
        layout.clone().with_daemon("default@2".into()).unwrap().ensure_daemon_dirs().unwrap();
        layout.set_current_generation(2).unwrap();
        let error = crate::session::ensure_daemon_started(&old).unwrap_err();
        assert!(error.contains("retired"), "{error}");
        assert_eq!(old.daemon_dir().exists(), registration_remains);
        assert_eq!(layout.generation(), Some(2));
    }
}

#[test]
fn later_drain_retries_failed_retirement_without_another_successor() {
    assert_failed_retirement_is_retried(|service| assert!(!service.drain().unwrap().changed));
}

#[cfg(unix)]
#[test]
fn later_handover_retries_failed_retirement_without_another_successor() {
    assert_failed_retirement_is_retried(|service| {
        let report = service.handover(Default::default()).unwrap();
        assert!(!report.drain.changed);
        assert!(report.moved.is_empty());
        assert!(report.stayed.is_empty());
    });
}

fn assert_failed_retirement_is_retried(recover: impl FnOnce(&SessionService)) {
    let temp = tempfile::tempdir().unwrap();
    let layout = RuntimeLayout::new(temp.path().to_path_buf());
    let old = layout.prepare_generation().unwrap();
    let host = OldDaemon::start(&old, true);
    host.fail_drain.store(true, Ordering::SeqCst);
    let service = SessionService::new(layout.clone());
    assert!(service.drain().unwrap_err().contains("injected drain failure"));
    assert_eq!(layout.generation(), Some(2));
    assert_eq!(service.daemon_build_status().unwrap().build, Some(crate::build_info::BuildInfo::current()));
    // Recovery is retried even though the successor already matches our build.
    assert!(service.drain().unwrap_err().contains("injected drain failure"));
    host.fail_drain.store(false, Ordering::SeqCst);
    recover(&service);
    assert_eq!(host.drain_requests.load(Ordering::SeqCst), 3);
    assert_eq!(layout.generation(), Some(2));
    assert!(!temp.path().join("default@3").exists());
    assert!(crate::generation_recovery::GenerationRecovery::load(&layout).unwrap().retirement.is_none());
    assert!(!service.drain().unwrap().changed);
    assert_eq!(host.drain_requests.load(Ordering::SeqCst), 3);
    service.daemon_request(Method::POST, "/drain").unwrap();
}

#[test]
fn recovery_before_publication_preserves_the_old_host() {
    let temp = tempfile::tempdir().unwrap();
    let layout = RuntimeLayout::new(temp.path().to_path_buf());
    let old = layout.prepare_generation().unwrap();
    let host = OldDaemon::start(&old, true);
    let reserved = layout.allocate_unpublished_generation().unwrap();
    let mut recovery = crate::generation_recovery::GenerationRecovery::load(&layout).unwrap();
    recovery.retirement = Some(crate::generation_recovery::Retirement { old: old.daemon_name().into(), successor: 2 });
    recovery.save(&layout).unwrap();
    let service = SessionService::new(layout.clone());
    service.resume_retirement(&mut recovery).unwrap();
    recovery.reclaim(&layout).unwrap();
    assert_eq!(host.drain_requests.load(Ordering::SeqCst), 0);
    assert_eq!(layout.generation(), Some(1));
    assert!(!reserved.daemon_dir().exists());
    assert_eq!(layout.allocate_generation().unwrap().generation(), Some(3));
}

#[test]
fn recovery_after_publication_retries_retirement_and_preserves_successor() {
    let temp = tempfile::tempdir().unwrap();
    let layout = RuntimeLayout::new(temp.path().to_path_buf());
    let old = layout.prepare_generation().unwrap();
    let host = OldDaemon::start(&old, true);
    let successor = layout.allocate_unpublished_generation().unwrap();
    let mut recovery = crate::generation_recovery::GenerationRecovery::load(&layout).unwrap();
    recovery.retirement = Some(crate::generation_recovery::Retirement { old: old.daemon_name().into(), successor: 2 });
    recovery.save(&layout).unwrap();
    layout.set_current_generation(2).unwrap();
    // Crash before clearing the unpublished reservation from the journal.
    let mut recovery = crate::generation_recovery::GenerationRecovery::load(&layout).unwrap();
    SessionService::new(layout.clone()).resume_retirement(&mut recovery).unwrap();
    recovery.reclaim(&layout).unwrap();
    assert_eq!(host.drain_requests.load(Ordering::SeqCst), 1);
    assert!(successor.daemon_dir().exists());
    assert!(recovery.unpublished.is_empty());
}

#[test]
fn pending_retirement_completes_when_the_old_host_already_exited() {
    let temp = tempfile::tempdir().unwrap();
    let layout = RuntimeLayout::new(temp.path().to_path_buf());
    let old = layout.prepare_generation().unwrap();
    let successor = layout.allocate_unpublished_generation().unwrap();
    let mut recovery = crate::generation_recovery::GenerationRecovery::load(&layout).unwrap();
    recovery.retirement = Some(crate::generation_recovery::Retirement { old: old.daemon_name().into(), successor: 2 });
    recovery.save(&layout).unwrap();
    layout.set_current_generation(2).unwrap();
    // A draining daemon may exit before its acknowledgment reaches the caller.
    std::fs::remove_dir_all(old.daemon_dir()).unwrap();
    SessionService::new(layout.clone()).resume_retirement(&mut recovery).unwrap();
    assert!(recovery.retirement.is_none());
    assert!(crate::generation_recovery::GenerationRecovery::load(&layout).unwrap().retirement.is_none());
    assert!(successor.daemon_dir().exists());
    assert_eq!(layout.generation(), Some(2));
}

#[cfg(unix)]
#[test]
fn handover_from_a_pre_handover_daemon_keeps_the_alias_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    let layout = RuntimeLayout::new(temp.path().to_path_buf());
    let old = layout.prepare_generation().unwrap();
    let _host = OldDaemon::start(&old, true);
    let error = SessionService::new(layout.clone()).handover(Default::default()).unwrap_err();
    assert!(error.contains("use server drain") && error.contains("alias unchanged"), "{error}");
    assert_eq!(layout.generation(), Some(1));
    SessionService::new(layout.with_daemon("default@2".into()).unwrap()).daemon_request(Method::POST, "/drain").unwrap();
}

#[cfg(unix)]
#[test]
fn response_deadline_reader_consumes_a_response_after_peer_close() {
    let (mut reader, mut writer) = std::os::unix::net::UnixStream::pair().unwrap();
    http_uds::write_json(&mut writer, StatusCode::OK, &serde_json::json!({"ok": true})).unwrap();
    drop(writer);
    let response =
        http_uds::read_response(&mut DaemonResponseReader { stream: &mut reader, deadline: Instant::now() + Duration::from_secs(1) })
            .unwrap();
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&response.body).unwrap(), serde_json::json!({"ok": true}));
}

#[cfg(unix)]
#[test]
fn response_deadline_is_not_extended_by_trickling_bytes() {
    use std::io::Read;
    let (mut reader, mut writer) = std::os::unix::net::UnixStream::pair().unwrap();
    let worker = thread::spawn(move || {
        for _ in 0..100 {
            if writer.write_all(b"x").is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
    });
    let start = Instant::now();
    let mut bytes = Vec::new();
    let error =
        DaemonResponseReader { stream: &mut reader, deadline: start + Duration::from_millis(60) }.read_to_end(&mut bytes).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert!(!bytes.is_empty());
    assert!(start.elapsed() < Duration::from_millis(500));
    drop(reader);
    worker.join().unwrap();
}

#[test]
fn recovery_and_new_retirement_both_report_unsupported_old_daemons() {
    let temp = tempfile::tempdir().unwrap();
    let layout = RuntimeLayout::new(temp.path().to_path_buf());
    let first = layout.prepare_generation().unwrap();
    let _first_host = OldDaemon::start(&first, false);
    let second = layout.allocate_generation().unwrap();
    let _second_host = OldDaemon::start(&second, false);
    layout.set_current_generation(2).unwrap();
    let mut recovery = crate::generation_recovery::GenerationRecovery::load(&layout).unwrap();
    recovery.retirement = Some(crate::generation_recovery::Retirement { old: first.daemon_name().into(), successor: 2 });
    recovery.save(&layout).unwrap();
    let service = SessionService::new(layout.clone());
    let report = service.drain().unwrap();
    assert!(report.changed);
    let warning = report.warning.unwrap();
    assert!(warning.contains("default@1 cannot be told to drain"), "{warning}");
    assert!(warning.contains("default@2 cannot be told to drain"), "{warning}");
    assert_eq!(layout.generation(), Some(3));
    service.daemon_request(Method::POST, "/drain").unwrap();
}
