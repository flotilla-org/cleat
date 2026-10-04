#![cfg(all(unix, feature = "ghostty-vt"))]
use std::{
    io::{Read, Seek, Write},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use cleat::{
    provider_ffi::*,
    runtime::RuntimeLayout,
    server::SessionService,
    vt::{ghostty::GhosttyVtEngine, VtEngine},
};
// Fixtures share Cleat's global, deliberately nonblocking output-admission
// coordinator. Serialize fixtures, while each scenario runs real concurrent actors.
fn fixture_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}
const COMMAND: &[u8] = b"stty -echo; printf ready; while IFS= read -r line; do case \"$line\" in copy) printf '\\033]52;c;8J+MjSBoZWxsbw==\\007';; clear) printf '\\033]52;s;\\033\\';; screen) printf done;; esac; done";
static WAKES: AtomicUsize = AtomicUsize::new(0);
unsafe extern "C" fn wake(_: *mut std::ffi::c_void) {
    WAKES.fetch_add(1, Ordering::SeqCst);
}
struct Fixture {
    temp: tempfile::TempDir,
    daemon: Option<Child>,
    provider: *mut CleatProvider,
    sessions: Vec<*mut CleatSession>,
}
impl Fixture {
    fn new(backend: u32) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_str().unwrap();
        let daemon = if backend == CLEAT_PROVIDER_BACKEND_DAEMON {
            let child = Command::new(env!("CARGO_BIN_EXE_cleat"))
                .args(["--runtime-root", root, "--server", "default", "serve"])
                .env_remove("CLEAT_DAEMON")
                .env_remove("CLEAT_SESSION")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap();
            wait(|| temp.path().join("default@1/socket").exists());
            Some(child)
        } else {
            None
        };
        let provider = unsafe {
            cleat_provider_open(&CleatProviderDesc {
                abi_version: CLEAT_PROVIDER_ABI_VERSION,
                backend,
                runtime_root: root.as_ptr(),
                runtime_root_len: root.len(),
                ..Default::default()
            })
        };
        assert!(!provider.is_null());
        unsafe {
            cleat_provider_set_wake_callback(provider, Some(wake), std::ptr::null_mut());
        }
        Self { temp, daemon, provider, sessions: Vec::new() }
    }
    fn create(&mut self, id: &[u8], command: &[u8]) -> *mut CleatSession {
        let session = unsafe {
            cleat_session_create(self.provider, &CleatSessionDesc {
                cols: 80,
                rows: 24,
                vt_engine: CLEAT_PROVIDER_VT_GHOSTTY,
                id: id.as_ptr(),
                id_len: id.len(),
                command: command.as_ptr(),
                command_len: command.len(),
                record: true,
                ..Default::default()
            })
        };
        assert!(!session.is_null());
        self.sessions.push(session);
        session
    }
    fn attach(&mut self, id: &[u8], role: u32) -> *mut CleatSession {
        let session = unsafe {
            cleat_session_attach(self.provider, &CleatSessionDesc {
                cols: 80,
                rows: 24,
                id: id.as_ptr(),
                id_len: id.len(),
                role,
                ..Default::default()
            })
        };
        assert!(!session.is_null());
        self.sessions.push(session);
        wait(|| unsafe { cleat_session_role(session) == role });
        session
    }
    fn ready(&self, session: *mut CleatSession, id: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = Vec::new();
        loop {
            let text = consume_render(session);
            if text.contains("ready") {
                return;
            }
            if !text.is_empty() {
                seen.push(text);
            }
            assert!(
                Instant::now() < deadline,
                "ready missing: updates={seen:?}, role={}, capture={:?}, inspect={:?}",
                unsafe { cleat_session_role(session) },
                self.service().capture(id),
                self.service().inspect(id)
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn service(&self) -> SessionService {
        SessionService::new(RuntimeLayout::new(self.temp.path().into()))
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if self.daemon.is_some() {
            if let Ok(sessions) = self.service().list() {
                for session in sessions {
                    let _ = self.service().kill(&session.id);
                }
            }
        }
        unsafe {
            for session in &self.sessions {
                cleat_session_destroy(*session);
            }
            cleat_provider_close(self.provider);
        }
        if let Some(daemon) = &mut self.daemon {
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
    }
}
#[track_caller]
fn wait(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn send(session: *mut CleatSession, text: &str) {
    unsafe {
        assert!(cleat_session_write_bytes(session, text.as_ptr(), text.len()));
    }
}
fn consume_render(session: *mut CleatSession) -> String {
    unsafe {
        let mut update = CleatRenderUpdate::default();
        let mut text = String::new();
        if cleat_session_render_update(session, &mut update) {
            if update.op_count > 0 {
                for op in std::slice::from_raw_parts(update.ops, update.op_count) {
                    if op.cell_count > 0 {
                        for cell in std::slice::from_raw_parts(op.cells, op.cell_count) {
                            if cell.grapheme_count > 0 {
                                for ch in std::slice::from_raw_parts(cell.graphemes, cell.grapheme_count) {
                                    text.push(char::from_u32(*ch).unwrap());
                                }
                            }
                        }
                    }
                }
            }
            cleat_session_mark_observed(session, update.render_generation);
            cleat_session_release_render_update(session, &mut update);
        }
        text
    }
}
fn acquire(session: *mut CleatSession) -> *const CleatClipboardEvent {
    let mut event = std::ptr::null();
    wait(|| {
        event = unsafe { cleat_session_acquire_clipboard_event(session) };
        !event.is_null()
    });
    event
}
fn value(event: *const CleatClipboardEvent) -> String {
    unsafe { String::from_utf8(std::slice::from_raw_parts((*event).text, (*event).text_len).to_vec()).unwrap() }
}
#[test]
fn providers_owned_acquisition_effect_only_wake_and_render_credit_independence() {
    let _isolation = fixture_lock();
    // Run the same real PTY -> VT -> C ABI contract for both hostings. Holding
    // render credit must not prevent effects; each acquisition owns its bytes.
    for backend in [CLEAT_PROVIDER_BACKEND_IN_PROCESS, CLEAT_PROVIDER_BACKEND_DAEMON] {
        let mut f = Fixture::new(backend);
        let s = f.create(b"clip", COMMAND);
        f.ready(s, "clip");
        wait(|| unsafe { cleat_session_clipboard_supported(s) });
        let before = WAKES.load(Ordering::SeqCst);
        send(s, "copy\n");
        let event = acquire(s);
        assert_eq!(value(event), "🌍 hello");
        assert_eq!(unsafe { (*event).kind }, 1);
        assert!(WAKES.load(Ordering::SeqCst) > before);
        assert!(consume_render(s).is_empty(), "clipboard input must not dirty rendered cells");
        assert!(unsafe { cleat_session_acquire_clipboard_event(s) }.is_null());
        let first_sequence = unsafe { (*event).sequence };
        send(s, "clear\n");
        let clear = acquire(s);
        unsafe {
            assert_eq!((*clear).kind, 2);
            assert_eq!((*clear).destination, 1);
            assert!((*clear).text.is_null());
            assert_eq!((*clear).text_len, 0);
            assert!((*clear).sequence > first_sequence);
            cleat_clipboard_event_release(clear);
        }
        send(s, "screen\n");
        std::thread::sleep(Duration::from_millis(100)); // Deliberately retain the resulting render credit.
        send(s, "copy\n");
        let second = acquire(s);
        assert_eq!(value(second), "🌍 hello");
        unsafe {
            cleat_clipboard_event_release(second);
        }
        drop(f);
        // Event bytes outlive both provider and session; release is independent.
        assert_eq!(value(event), "🌍 hello");
        unsafe {
            cleat_clipboard_event_release(event);
        }
    }
}
#[test]
fn daemon_controllers_watchers_takeover_and_late_subscribers() {
    let _isolation = fixture_lock();
    // Only the lowest live controller identity is eligible; a watcher, late
    // subscriber, and newly promoted controller must never inherit pending writes.
    let mut f = Fixture::new(CLEAT_PROVIDER_BACKEND_DAEMON);
    let a = f.create(b"clip", COMMAND);
    f.ready(a, "clip");
    let b = f.attach(b"clip", CLEAT_ROLE_CONTROLLER);
    let watcher = f.attach(b"clip", CLEAT_ROLE_WATCHER);
    send(a, "copy\n");
    let e = acquire(a);
    unsafe {
        cleat_clipboard_event_release(e);
    }
    assert!(unsafe { cleat_session_acquire_clipboard_event(b) }.is_null());
    assert!(unsafe { cleat_session_acquire_clipboard_event(watcher) }.is_null());
    send(a, "copy\n");
    std::thread::sleep(Duration::from_millis(100));
    unsafe {
        assert!(cleat_session_take_control(b));
    }
    wait(|| unsafe { cleat_session_role(a) == CLEAT_ROLE_WATCHER });
    assert!(unsafe { cleat_session_acquire_clipboard_event(a) }.is_null());
    assert!(unsafe { cleat_session_acquire_clipboard_event(b) }.is_null());
    send(b, "clear\n");
    let e = acquire(b);
    unsafe {
        assert_eq!((*e).kind, 2);
        cleat_clipboard_event_release(e);
    }
    let late = f.attach(b"clip", CLEAT_ROLE_WATCHER);
    assert!(unsafe { cleat_session_acquire_clipboard_event(late) }.is_null());
    unsafe {
        assert!(cleat_session_set_role(b, CLEAT_ROLE_WATCHER, false));
    }
    wait(|| unsafe { cleat_session_role(b) == CLEAT_ROLE_WATCHER });
    unsafe {
        assert!(cleat_session_take_control(b));
    }
    wait(|| unsafe { cleat_session_role(b) == CLEAT_ROLE_CONTROLLER });
    assert!(unsafe { cleat_session_acquire_clipboard_event(b) }.is_null());
}
#[test]
fn actual_attach_output_reparses_in_real_ghostty() {
    let _isolation = fixture_lock();
    // A real daemon and CLI attachment serialize live writes. Reparse CLI bytes
    // with the real enclosing VT; no proposed relay is replaced by a fake.
    let mut f = Fixture::new(CLEAT_PROVIDER_BACKEND_DAEMON);
    let s = f.create(b"clip", COMMAND);
    f.ready(s, "clip");
    unsafe {
        assert!(cleat_session_set_role(s, CLEAT_ROLE_WATCHER, false));
    }
    wait(|| unsafe { cleat_session_role(s) == CLEAT_ROLE_WATCHER });
    let mut output = tempfile::tempfile().unwrap();
    let mut cli = Command::new(env!("CARGO_BIN_EXE_cleat"))
        .args(["--runtime-root", f.temp.path().to_str().unwrap(), "attach", "clip", "--no-create", "--no-record"])
        .env_remove("CLEAT_DAEMON")
        .env_remove("CLEAT_SESSION")
        .stdin(Stdio::piped())
        .stdout(output.try_clone().unwrap())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    wait(|| output.metadata().unwrap().len() > 0);
    cli.stdin.as_mut().unwrap().write_all(b"copy\nclear\n").unwrap();
    let mut enclosing = GhosttyVtEngine::new(80, 24);
    let mut events = Vec::new();
    let mut offset = 0;
    wait(|| {
        output.seek(std::io::SeekFrom::Start(offset)).unwrap();
        let mut bytes = Vec::new();
        output.read_to_end(&mut bytes).unwrap();
        offset += bytes.len() as u64;
        enclosing.feed(&bytes).unwrap();
        events.extend(enclosing.drain_clipboard().0);
        events.len() >= 2
    });
    f.service().kill("clip").unwrap();
    let _ = cli.wait();
    output.seek(std::io::SeekFrom::Start(offset)).unwrap();
    let mut bytes = Vec::new();
    output.read_to_end(&mut bytes).unwrap();
    enclosing.feed(&bytes).unwrap();
    events.extend(enclosing.drain_clipboard().0);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].text.as_deref(), Some("🌍 hello"));
    assert_eq!(events[1].text, None);
    assert_eq!(events[1].destination, 1);
}
#[test]
fn nested_local_attach_delivers_to_native_outer_provider() {
    let _isolation = fixture_lock();
    // The inner CLI's enclosing terminal is another real Cleat VT. The outer
    // provider must expose the original text to a native sink exactly once.
    let mut f = Fixture::new(CLEAT_PROVIDER_BACKEND_DAEMON);
    let inner = f.create(b"inner", COMMAND);
    f.ready(inner, "inner");
    unsafe {
        assert!(cleat_session_set_role(inner, CLEAT_ROLE_WATCHER, false));
    }
    wait(|| unsafe { cleat_session_role(inner) == CLEAT_ROLE_WATCHER });
    let command =
        format!("{} --runtime-root {} attach inner --no-create --no-record", env!("CARGO_BIN_EXE_cleat"), f.temp.path().display());
    let outer = f.create(b"outer", command.as_bytes());
    f.ready(outer, "outer");
    send(outer, "copy\n");
    let e = acquire(outer);
    assert_eq!(value(e), "🌍 hello");
    unsafe {
        cleat_clipboard_event_release(e);
    }
    send(outer, "clear\n");
    let e = acquire(outer);
    unsafe {
        assert_eq!((*e).kind, 2);
        cleat_clipboard_event_release(e);
    }
    assert!(unsafe { cleat_session_acquire_clipboard_event(outer) }.is_null());
}

#[test]
fn slow_and_disconnected_consumers_stay_bounded_and_do_not_stall_output() {
    let _isolation = fixture_lock();
    // A consumer that never drains effects is bounded in both hostings. A
    // disconnected daemon attachment loses effects instead of replaying them.
    for backend in [CLEAT_PROVIDER_BACKEND_IN_PROCESS, CLEAT_PROVIDER_BACKEND_DAEMON] {
        let mut f = Fixture::new(backend);
        let s = f.create(b"clip", COMMAND);
        f.ready(s, "clip");
        send(s, &("copy\n".repeat(100) + "screen\n"));
        wait(|| consume_render(s).contains("done"));
        wait(|| unsafe { cleat_session_clipboard_dropped(s) > 0 });
        let mut count = 0;
        loop {
            let e = unsafe { cleat_session_acquire_clipboard_event(s) };
            if e.is_null() {
                break;
            }
            count += 1;
            unsafe {
                cleat_clipboard_event_release(e);
            }
        }
        assert!(count <= cleat::clipboard::MAX_CLIPBOARD_EVENTS);
        assert!(count > 0);
        if backend == CLEAT_PROVIDER_BACKEND_DAEMON {
            unsafe {
                cleat_session_destroy(s);
            }
            f.sessions[0] = std::ptr::null_mut();
            wait(|| f.service().inspect("clip").is_ok_and(|i| i.attachments.is_empty()));
            f.service().send_keys("clip", b"copy\nscreen\n").unwrap();
            wait(|| f.service().capture("clip").is_ok_and(|text| text.contains("donedone")));
            let reconnected = f.attach(b"clip", CLEAT_ROLE_CONTROLLER);
            assert!(unsafe { cleat_session_acquire_clipboard_event(reconnected) }.is_null());
            send(reconnected, "copy\n");
            let e = acquire(reconnected);
            assert_eq!(value(e), "🌍 hello");
            unsafe {
                cleat_clipboard_event_release(e);
            }
        }
    }
}
#[test]
fn history_resize_refresh_and_hosting_transfer_never_reemit_effects() {
    let _isolation = fixture_lock();
    // Independent views and full snapshots carry retained state only. Transfer
    // drops queued writes and starts a new actor identity for subsequent effects.
    let mut f = Fixture::new(CLEAT_PROVIDER_BACKEND_DAEMON);
    let s = f.create(b"clip", COMMAND);
    f.ready(s, "clip");
    send(s, "copy\n");
    let e = acquire(s);
    let epoch = unsafe { (*e).session_epoch };
    unsafe {
        cleat_clipboard_event_release(e);
    }
    for kind in [CLEAT_VIEWPORT_COMMAND_TOP, CLEAT_VIEWPORT_COMMAND_BOTTOM] {
        unsafe {
            assert!(cleat_session_scroll_viewport(s, &CleatViewportCommand { kind, delta_rows: 0 }, std::ptr::null_mut()));
        }
        let _ = consume_render(s);
        assert!(unsafe { cleat_session_acquire_clipboard_event(s) }.is_null());
    }
    unsafe {
        assert!(cleat_session_resize(s, 81, 25));
    }
    std::thread::sleep(Duration::from_millis(100));
    let _ = consume_render(s);
    unsafe {
        let mut snapshot = CleatSnapshot::default();
        if cleat_session_snapshot(s, &mut snapshot) {
            cleat_session_release_snapshot(s, &mut snapshot);
        }
    }
    assert!(unsafe { cleat_session_acquire_clipboard_event(s) }.is_null());
    send(s, "copy\n");
    std::thread::sleep(Duration::from_millis(100));
    unsafe {
        assert!(cleat_session_adopt(s));
    }
    assert!(unsafe { cleat_session_acquire_clipboard_event(s) }.is_null());
    send(s, "copy\n");
    let e = acquire(s);
    assert_ne!(unsafe { (*e).session_epoch }, epoch);
    unsafe {
        cleat_clipboard_event_release(e);
    }
    send(s, "copy\n");
    std::thread::sleep(Duration::from_millis(100));
    unsafe {
        assert!(cleat_session_transfer(s, b"default".as_ptr(), 7));
    }
    wait(|| unsafe { cleat_session_role(s) == CLEAT_ROLE_CONTROLLER });
    assert!(unsafe { cleat_session_acquire_clipboard_event(s) }.is_null());
    send(s, "copy\n");
    let e = acquire(s);
    assert_eq!(value(e), "🌍 hello");
    unsafe {
        cleat_clipboard_event_release(e);
    }
}
