#![cfg(all(unix, feature = "ghostty-vt"))]

use std::{
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use cleat::{provider_ffi::*, runtime::RuntimeLayout, server::SessionService};

struct Fixture {
    temp: tempfile::TempDir,
    daemon: Option<Child>,
    provider: *mut CleatProvider,
    controller: *mut CleatSession,
    watcher: *mut CleatSession,
}

impl Fixture {
    fn new(backend: u32) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_str().unwrap();
        let daemon = if backend == CLEAT_PROVIDER_BACKEND_DAEMON {
            let daemon = Command::new(env!("CARGO_BIN_EXE_cleat"))
                .args(["--runtime-root", root, "--server", "default", "serve"])
                .env_remove("CLEAT_DAEMON")
                .env_remove("CLEAT_SESSION")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap();
            wait(|| temp.path().join("default@1/socket").exists());
            Some(daemon)
        } else {
            None
        };
        unsafe {
            let provider = cleat_provider_open(&CleatProviderDesc {
                abi_version: CLEAT_PROVIDER_ABI_VERSION,
                backend,
                runtime_root: root.as_ptr(),
                runtime_root_len: root.len(),
                ..Default::default()
            });
            assert!(!provider.is_null());
            // The readiness marker proves the TERM trap is installed before ending.
            let command = b"sh -c 'trap \"\" TERM; echo ready; sleep 600'";
            let controller = cleat_session_create(provider, &CleatSessionDesc {
                cols: 80,
                rows: 24,
                id: b"ending".as_ptr(),
                id_len: 6,
                command: command.as_ptr(),
                command_len: command.len(),
                record: true,
                ..Default::default()
            });
            assert!(!controller.is_null());
            Self { temp, daemon, provider, controller, watcher: std::ptr::null_mut() }
        }
    }
    fn error(&self, session: *const CleatSession) -> String {
        unsafe {
            let mut out = CleatStr::default();
            assert!(cleat_session_end_error(session, &mut out));
            String::from_utf8_lossy(std::slice::from_raw_parts(out.ptr, out.len)).into_owned()
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        unsafe {
            cleat_session_destroy(self.watcher);
            cleat_session_destroy(self.controller);
            cleat_provider_close(self.provider);
        }
        if let Some(daemon) = &mut self.daemon {
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
    }
}
#[track_caller]
fn wait(ready: impl FnMut() -> bool) {
    wait_for(Duration::from_secs(10), ready);
}
#[track_caller]
fn wait_for(timeout: Duration, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !ready() {
        assert!(Instant::now() < deadline, "timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}

// The C ABI must reject watchers without ending the session; controllers request
// whole-tree escalation, retain their handles, and preserve the recording.
#[test]
fn controller_ends_term_resistant_session_watcher_cannot() {
    let mut fixture = Fixture::new(CLEAT_PROVIDER_BACKEND_DAEMON);
    let service = SessionService::new(RuntimeLayout::new(fixture.temp.path().into()));
    wait(|| service.capture("ending").is_ok_and(|text| text.contains("ready")));
    let cast = fixture.temp.path().join("default@1/sessions/ending/session.cast");
    wait(|| cast.exists());
    unsafe {
        wait(|| cleat_session_role(fixture.controller) == CLEAT_ROLE_CONTROLLER);
        fixture.watcher = cleat_session_attach(fixture.provider, &CleatSessionDesc {
            cols: 80,
            rows: 24,
            id: b"ending".as_ptr(),
            id_len: 6,
            role: CLEAT_ROLE_WATCHER,
            ..Default::default()
        });
        assert!(!fixture.watcher.is_null());
        wait(|| cleat_session_role(fixture.watcher) == CLEAT_ROLE_WATCHER);
        assert!(!cleat_session_end(fixture.watcher));
        assert!(fixture.error(fixture.watcher).contains("watcher"));
        assert!(service.inspect("ending").is_ok());
        assert_ne!(cleat_session_connection_state(fixture.controller), CLEAT_SESSION_CLOSED);
        assert!(cleat_session_end(fixture.controller), "{}", fixture.error(fixture.controller));
        assert!(fixture.error(fixture.controller).is_empty());
        // A role change takes effect on the same handle, and success clears its
        // earlier watcher error. Repeated end requests share daemon escalation.
        assert!(cleat_session_set_role(fixture.watcher, CLEAT_ROLE_CONTROLLER, false));
        wait(|| cleat_session_role(fixture.watcher) == CLEAT_ROLE_CONTROLLER);
        assert!(cleat_session_end(fixture.watcher), "{}", fixture.error(fixture.watcher));
        assert!(fixture.error(fixture.watcher).is_empty());
        wait_for(Duration::from_secs(30), || cleat_session_connection_state(fixture.controller) == CLEAT_SESSION_CLOSED);
        wait_for(Duration::from_secs(30), || cleat_session_connection_state(fixture.watcher) == CLEAT_SESSION_CLOSED);
        assert!(cast.exists());
        // A failed retry keeps the handle usable and reports the HTTP error.
        assert!(!cleat_session_end(fixture.controller));
        assert!(!fixture.error(fixture.controller).is_empty());
    }
}

// In-process ending is explicitly unsupported; refusal leaves input usable.
#[test]
fn in_process_end_is_unsupported_and_preserves_handle() {
    let fixture = Fixture::new(CLEAT_PROVIDER_BACKEND_IN_PROCESS);
    unsafe {
        assert_eq!(cleat_session_role(fixture.controller), CLEAT_ROLE_CONTROLLER);
        assert!(!cleat_session_end(fixture.controller));
        let error = fixture.error(fixture.controller);
        assert!(error.contains("in-process") && error.contains("unsupported"), "{error}");
        assert!(cleat_session_write_bytes(fixture.controller, b"x".as_ptr(), 1));
        assert_eq!(cleat_session_connection_state(fixture.controller), CLEAT_SESSION_STREAMING);
    }
}

// Invalid pointers are refused by the additive exports, like transfer_error.
#[test]
fn end_null_arguments_are_rejected() {
    unsafe {
        assert!(!cleat_session_end(std::ptr::null_mut()));
        assert!(!cleat_session_end_error(std::ptr::null(), &mut CleatStr::default()));
        // A mock handle is enough to exercise pointer validation; no processes.
        let provider = cleat_provider_open(&CleatProviderDesc {
            abi_version: CLEAT_PROVIDER_ABI_VERSION,
            backend: CLEAT_PROVIDER_BACKEND_MOCK,
            ..Default::default()
        });
        assert!(!provider.is_null());
        let session = cleat_session_create(provider, &CleatSessionDesc::default());
        assert!(!session.is_null());
        assert!(!cleat_session_end_error(session, std::ptr::null_mut()));
        cleat_session_destroy(session);
        cleat_provider_close(provider);
    }
}
