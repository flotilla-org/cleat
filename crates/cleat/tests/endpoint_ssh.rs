#![cfg(all(unix, feature = "ghostty-vt"))]
//! `cleat_provider_open_endpoint` and `cleat_provider_transport_state` against
//! a real daemon and a real `cleat proxy` (cleat#383). One test in its own
//! binary: it puts a stand-in `ssh` first on PATH, which must not race other
//! tests spawning processes.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    process::Command,
    thread,
    time::{Duration, Instant},
};

use cleat::{provider::ProviderFeatures, provider_ffi::*, runtime::RuntimeLayout, server::SessionService, vt::VtEngineKind};

fn text(s: &str) -> CleatStr {
    CleatStr { ptr: s.as_ptr(), len: s.len() }
}

fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

struct State {
    connected: bool,
    retrying: bool,
    error: String,
    warning: String,
}

fn state(provider: *mut CleatProvider, capacity: usize) -> State {
    let (mut error, mut warning) = (vec![0u8; capacity], vec![0u8; capacity]);
    let mut state = CleatTransportState {
        error: error.as_mut_ptr(),
        error_capacity: capacity,
        warning: warning.as_mut_ptr(),
        warning_capacity: capacity,
        ..CleatTransportState::default()
    };
    assert!(unsafe { cleat_provider_transport_state(provider, &mut state) });
    State {
        connected: state.connected,
        retrying: state.retrying,
        error: String::from_utf8(error[..state.error_len].to_vec()).unwrap(),
        warning: String::from_utf8(warning[..state.warning_len].to_vec()).unwrap(),
    }
}

fn attach_streaming(provider: *mut CleatProvider) -> *mut CleatSession {
    let id = "remote";
    let session = unsafe {
        cleat_session_attach(provider, &CleatSessionDesc {
            cols: 80,
            rows: 24,
            id: id.as_ptr(),
            id_len: id.len(),
            ..CleatSessionDesc::default()
        })
    };
    assert!(!session.is_null());
    eventually("streaming", || {
        let mut update = CleatRenderUpdate::default();
        if unsafe { cleat_session_render_update(session, &mut update) } {
            unsafe { cleat_session_release_render_update(session, &mut update) };
        }
        let state = unsafe { cleat_session_connection_state(session) };
        state == CLEAT_SESSION_STREAMING
    });
    session
}

fn hosting(session: *mut CleatSession) -> String {
    let mut hosting = CleatStr::default();
    assert!(unsafe { cleat_session_hosting(session, &mut hosting) });
    unsafe { std::str::from_utf8(std::slice::from_raw_parts(hosting.ptr, hosting.len)).unwrap().to_string() }
}

// Behaviour: an ssh endpoint attaches through `ssh … cleat proxy` with
// remote hosting; its state reports the admission warning, and once the
// bridge dies it reads disconnected, given up, with a reason, its texts
// cut to the caller's buffers. A local endpoint attaches through the
// daemon provider for its runtime root, and an invalid one opens nothing.
#[test]
fn endpoints_open_providers_and_report_state() {
    let root = tempfile::Builder::new().prefix("cleat-endpoint-").tempdir_in("/tmp").unwrap();
    let layout = RuntimeLayout::new(root.path().to_owned());
    let service = SessionService::new(layout.clone());
    service.create(Some("remote".into()), Some(VtEngineKind::Ghostty), None, Some("cat".into()), false).unwrap();

    // The stand-in for ssh at the process boundary: it runs the remote
    // command here, as sshd would hand it to the login shell there.
    let bin = root.path().join("bin");
    fs::create_dir(&bin).unwrap();
    fs::write(bin.join("ssh"), "#!/bin/sh\nwhile [ \"$#\" -gt 0 ] && [ \"$1\" != -- ]; do shift; done\nshift 2\nexec sh -c \"$1\"\n")
        .unwrap();
    fs::set_permissions(bin.join("ssh"), fs::Permissions::from_mode(0o755)).unwrap();
    // SAFETY: this binary's only test, before it spawns anything.
    unsafe { std::env::set_var("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap())) };

    let desc = CleatProviderDesc {
        abi_version: CLEAT_PROVIDER_ABI_VERSION,
        requested_features: ProviderFeatures::CELL_SNAPSHOTS.bits(),
        backend: CLEAT_PROVIDER_BACKEND_DAEMON,
        ..CleatProviderDesc::default()
    };
    let (ssh, local, host, cleat) = ("ssh", "local", "fleet", env!("CARGO_BIN_EXE_cleat"));
    let runtime_root = root.path().display().to_string();
    let endpoint = CleatEndpoint {
        transport: text(ssh),
        host: text(host),
        runtime_root: text(&runtime_root),
        cleat_command: text(cleat),
        ..CleatEndpoint::default()
    };

    let provider = unsafe { cleat_provider_open_endpoint(&desc, &endpoint) };
    assert!(!provider.is_null());
    let session = attach_streaming(provider);
    assert_eq!(hosting(session), "remote:command");
    let up = state(provider, 256);
    assert!(up.connected && up.error.is_empty(), "connected without an error");
    assert!(up.warning.contains("cycle protection does not cover remote relationships"), "{}", up.warning);
    assert_eq!(state(provider, 5).warning.len(), 5, "the warning is cut to the caller's buffer");

    Command::new("pkill").args(["-f", &format!("proxy --runtime-root={runtime_root}")]).status().unwrap();
    eventually("the bridge gives up", || !state(provider, 256).retrying);
    let down = state(provider, 256);
    assert!(!down.connected && !down.error.is_empty(), "disconnected with a reason");
    assert_eq!(unsafe { cleat_session_connection_state(session) }, CLEAT_SESSION_DISCONNECTED);
    unsafe {
        cleat_session_destroy(session);
        cleat_provider_close(provider);
    }

    let local_endpoint = CleatEndpoint { transport: text(local), runtime_root: text(&runtime_root), ..CleatEndpoint::default() };
    let provider = unsafe { cleat_provider_open_endpoint(&desc, &local_endpoint) };
    assert!(!provider.is_null());
    let session = attach_streaming(provider);
    assert!(hosting(session).starts_with("daemon:"), "{}", hosting(session));
    unsafe {
        cleat_session_destroy(session);
        cleat_provider_close(provider);
    }

    let typo = "ssh ";
    assert!(unsafe { cleat_provider_open_endpoint(&desc, &CleatEndpoint { transport: text(typo), ..endpoint }) }.is_null());
    let mock = CleatProviderDesc { backend: CLEAT_PROVIDER_BACKEND_MOCK, ..desc };
    assert!(unsafe { cleat_provider_open_endpoint(&mock, &endpoint) }.is_null());

    let _ = service.kill("remote");
    if let Ok(pid) = fs::read_to_string(layout.daemon_pid_path()) {
        unsafe { libc::kill(pid.trim().parse().unwrap(), libc::SIGKILL) };
    }
}
