#![cfg(all(unix, feature = "ghostty-vt"))]
use std::{
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use cleat::{provider_ffi::*, runtime::RuntimeLayout, server::SessionService};

struct Fixture {
    temp: tempfile::TempDir,
    daemon: Child,
    provider: *mut CleatProvider,
    session: *mut CleatSession,
}
impl Fixture {
    fn new(refuse: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_str().unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_cleat"));
        command
            .args(["--runtime-root", root, "--server", "default", "serve"])
            .env_remove("CLEAT_DAEMON")
            .env_remove("CLEAT_SESSION")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        if refuse {
            command.env("CLEAT_TEST_TRANSFER_REFUSE_ADOPTION", "provider NACK");
        }
        let daemon = command.spawn().unwrap();
        wait(|| temp.path().join("default@1/socket").exists());
        let (provider, session) = unsafe {
            let provider = cleat_provider_open(&CleatProviderDesc {
                abi_version: CLEAT_PROVIDER_ABI_VERSION,
                requested_features: cleat::provider::ProviderFeatures::CELL_SNAPSHOTS.bits(),
                backend: CLEAT_PROVIDER_BACKEND_IN_PROCESS,
                runtime_root: root.as_ptr(),
                runtime_root_len: root.len(),
                ..Default::default()
            });
            let session = cleat_session_create(provider, &CleatSessionDesc {
                cols: 80,
                rows: 24,
                id: b"moving".as_ptr(),
                id_len: 6,
                command: b"cat".as_ptr(),
                command_len: 3,
                ..Default::default()
            });
            assert!(!session.is_null());
            (provider, session)
        };
        Self { temp, daemon, provider, session }
    }
    fn service(&self) -> SessionService {
        SessionService::new(RuntimeLayout::new(self.temp.path().into()))
    }
    fn text(&self, expected: &str) {
        unsafe {
            assert!(cleat_session_write_bytes(self.session, format!("{expected}\n").as_ptr(), expected.len() + 1));
        }
        wait(|| unsafe {
            let mut update = CleatRenderUpdate::default();
            if !cleat_session_render_update(self.session, &mut update) {
                return false;
            }
            let mut text = String::new();
            if update.op_count > 0 {
                for op in std::slice::from_raw_parts(update.ops, update.op_count) {
                    if op.cell_count > 0 {
                        for cell in std::slice::from_raw_parts(op.cells, op.cell_count) {
                            if cell.grapheme_count > 0 {
                                for ch in std::slice::from_raw_parts(cell.graphemes, cell.grapheme_count) {
                                    text.push(char::from_u32(*ch).unwrap_or(' '));
                                }
                            }
                        }
                    }
                }
            }
            cleat_session_mark_observed(self.session, update.render_generation);
            cleat_session_release_render_update(self.session, &mut update);
            text.contains(expected)
        });
    }
    fn error(&self) -> String {
        unsafe {
            let mut s = CleatStr::default();
            cleat_session_transfer_error(self.session, &mut s);
            String::from_utf8_lossy(std::slice::from_raw_parts(s.ptr, s.len)).into_owned()
        }
    }
    fn hosting(&self) -> String {
        unsafe {
            let mut s = CleatStr::default();
            assert!(cleat_session_hosting(self.session, &mut s));
            String::from_utf8_lossy(std::slice::from_raw_parts(s.ptr, s.len)).into_owned()
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        unsafe {
            cleat_session_destroy(self.session);
            cleat_provider_close(self.provider);
        }
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}
fn wait(mut ready: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < end, "timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn same_handle_round_trips_and_preserves_input_rendering_recording_and_epoch() {
    let fixture = Fixture::new(false);
    fixture.text("before_transfer");
    assert_eq!(fixture.hosting(), "in_process");
    unsafe {
        assert!(cleat_session_transfer(fixture.session, b"default".as_ptr(), 7), "{}", fixture.error());
    }
    assert_eq!(fixture.hosting(), "daemon:default@1");
    wait(|| unsafe { cleat_session_role(fixture.session) == CLEAT_ROLE_CONTROLLER });
    fixture.text("after_transfer");
    assert_eq!(fixture.service().inspect("moving").unwrap().hosting_epoch, 2);
    let cast = fixture.temp.path().join("default@1/sessions/moving/session.cast");
    assert!(std::fs::read_to_string(&cast).unwrap().contains("before_transfer"));
    unsafe {
        assert!(cleat_session_adopt(fixture.session), "{}", fixture.error());
    }
    assert_eq!(fixture.hosting(), "in_process");
    fixture.text("after_adopt");
    wait(|| fixture.service().inspect("moving").is_ok_and(|inspect| inspect.session.state == "hosted-elsewhere"));
    assert_eq!(fixture.service().inspect("moving").unwrap().hosting_epoch, 3);
    assert!(!cleat::recreate::session_is_recreatable(cast.parent().unwrap()));
    assert!(fixture.service().kill_with_purge("moving", true).is_err());
    assert!(cast.exists());
    unsafe {
        assert!(cleat_session_transfer(fixture.session, b"default".as_ptr(), 7), "{}", fixture.error());
    }
    wait(|| unsafe { cleat_session_role(fixture.session) == CLEAT_ROLE_CONTROLLER });
    fixture.text("returned_again");
    assert_eq!(fixture.service().inspect("moving").unwrap().hosting_epoch, 4);
}

#[test]
fn unreachable_daemon_leaves_handle_usable() {
    let fixture = Fixture::new(false);
    unsafe {
        assert!(!cleat_session_transfer(fixture.session, b"missing".as_ptr(), 7));
    }
    assert!(fixture.error().contains("unreachable"));
    assert_eq!(fixture.hosting(), "in_process");
    fixture.text("still_here");
}

#[test]
fn manifest_nack_leaves_original_handle_and_input_usable() {
    let fixture = Fixture::new(true);
    fixture.text("before_nack");
    unsafe {
        assert!(!cleat_session_transfer(fixture.session, b"default".as_ptr(), 7));
    }
    assert!(fixture.error().contains("provider NACK"), "{}", fixture.error());
    assert_eq!(fixture.hosting(), "in_process");
    fixture.text("after_nack");
    assert!(!fixture.temp.path().join("default@1/sessions/moving").exists());
}

#[test]
fn destroying_adopted_handle_leaves_a_recreatable_husk() {
    let mut fixture = Fixture::new(false);
    unsafe {
        assert!(cleat_session_transfer(fixture.session, b"default".as_ptr(), 7), "{}", fixture.error());
        assert!(cleat_session_adopt(fixture.session), "{}", fixture.error());
    }
    fixture.text("recorded_embedded_output");
    unsafe {
        cleat_session_destroy(fixture.session);
    }
    fixture.session = std::ptr::null_mut();
    let dir = fixture.temp.path().join("default@1/sessions/moving");
    wait(|| cleat::recreate::session_is_recreatable(&dir));
    wait(|| fixture.service().inspect("moving").is_err());
    assert!(std::fs::read_to_string(dir.join("session.cast")).unwrap().contains("recorded_embedded_output"));
}

#[test]
fn embedded_holder_child() {
    let Ok(root) = std::env::var("CLEAT_TRANSFER_TEST_HOLDER_ROOT") else {
        return;
    };
    unsafe {
        let provider = cleat_provider_open(&CleatProviderDesc {
            abi_version: CLEAT_PROVIDER_ABI_VERSION,
            requested_features: cleat::provider::ProviderFeatures::CELL_SNAPSHOTS.bits(),
            backend: CLEAT_PROVIDER_BACKEND_DAEMON,
            runtime_root: root.as_ptr(),
            runtime_root_len: root.len(),
            ..Default::default()
        });
        let session = cleat_session_attach(provider, &CleatSessionDesc {
            cols: 80,
            rows: 24,
            id: b"moving".as_ptr(),
            id_len: 6,
            ..Default::default()
        });
        assert!(!session.is_null());
        assert!(cleat_session_adopt(session));
        std::fs::write(std::path::Path::new(&root).join("holder-ready"), "ready").unwrap();
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

#[test]
fn killed_embedded_process_releases_lease_and_leaves_recreatable_recording() {
    let fixture = Fixture::new(false);
    fixture.text("survives_holder_death");
    unsafe {
        assert!(cleat_session_transfer(fixture.session, b"default".as_ptr(), 7), "{}", fixture.error());
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "embedded_holder_child", "--nocapture"])
        .env("CLEAT_TRANSFER_TEST_HOLDER_ROOT", fixture.temp.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    wait(|| fixture.temp.path().join("holder-ready").exists());
    let dir = fixture.temp.path().join("default@1/sessions/moving");
    assert!(!cleat::recreate::session_is_recreatable(&dir));
    // Observe publication before killing the adopter: COMMITTED reaches the
    // process before the source daemon publishes its hosted-elsewhere entry.
    wait(|| fixture.service().inspect("moving").is_ok_and(|inspect| inspect.session.state == "hosted-elsewhere"));
    child.kill().unwrap();
    child.wait().unwrap();
    wait(|| cleat::recreate::session_is_recreatable(&dir));
    wait(|| fixture.service().inspect("moving").is_err());
    // Recreation uses the same directory and seeds the replacement VT from the
    // recording, exactly as attach's create-if-missing path does.
    fixture.service().create(Some("moving".into()), Some(cleat::vt::VtEngineKind::Ghostty), None, Some("cat".into()), true).unwrap();
    fixture.service().inspect("moving").expect("inspect recreated session");
    assert!(std::fs::read_to_string(dir.join("session.cast")).unwrap().contains("survives_holder_death"));
}

struct DaemonGuard(Child);
impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn adopted_holder_can_return_to_another_daemon_with_one_recording() {
    let fixture = Fixture::new(false);
    fixture.text("original_history");
    unsafe {
        assert!(cleat_session_transfer(fixture.session, b"default".as_ptr(), 7), "{}", fixture.error());
        assert!(cleat_session_adopt(fixture.session), "{}", fixture.error());
    }
    fixture.text("embedded_history");
    wait(|| fixture.service().inspect("moving").is_ok_and(|inspect| inspect.session.state == "hosted-elsewhere"));
    let _other = DaemonGuard(
        Command::new(env!("CARGO_BIN_EXE_cleat"))
            .arg("--runtime-root")
            .arg(fixture.temp.path())
            .args(["--server", "other", "serve"])
            .env_remove("CLEAT_SESSION")
            .env_remove("CLEAT_DAEMON")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    wait(|| fixture.temp.path().join("other@1/socket").exists());
    unsafe {
        assert!(cleat_session_transfer(fixture.session, b"other".as_ptr(), 5), "{}", fixture.error());
    }
    wait(|| unsafe { cleat_session_role(fixture.session) == CLEAT_ROLE_CONTROLLER });
    fixture.text("new_daemon");
    assert_eq!(fixture.hosting(), "daemon:other@1");
    assert!(!fixture.temp.path().join("default@1/sessions/moving").exists());
    let cast = std::fs::read_to_string(fixture.temp.path().join("other@1/sessions/moving/session.cast")).unwrap();
    assert!(cast.contains("original_history"));
    assert!(cast.contains("embedded_history"));
    assert_eq!(cast.lines().filter(|line| line.contains("\"version\":3")).count(), 1);
}

#[test]
fn image_bytes_and_placement_survive_both_hosting_changes() {
    let fixture = Fixture::new(false);
    let image = b"\x1b[H\x1b_Ga=T,C=1,q=2,i=77,p=1,f=24,s=1,v=1,c=1,r=1;AQID\x1b\\\n";
    unsafe {
        assert!(cleat_session_write_bytes(fixture.session, image.as_ptr(), image.len()));
    }
    fn image_present(session: *mut CleatSession) -> bool {
        unsafe extern "C" fn check(_: *mut std::ffi::c_void, bytes: *const u8, len: usize) -> bool {
            unsafe { std::slice::from_raw_parts(bytes, len) == [1, 2, 3] }
        }
        unsafe {
            let mut update = CleatRenderUpdate::default();
            if !cleat_session_render_update(session, &mut update) {
                return false;
            }
            let mut found = false;
            if update.image_resource_count > 0 && update.image_placement_count > 0 {
                for resource in std::slice::from_raw_parts(update.image_resources, update.image_resource_count) {
                    if resource.image_id == 77 {
                        found = cleat_session_with_image_resource_data(
                            session,
                            resource.image_id,
                            resource.generation,
                            Some(check),
                            std::ptr::null_mut(),
                        );
                    }
                }
            }
            cleat_session_mark_observed(session, update.render_generation);
            cleat_session_release_render_update(session, &mut update);
            found
        }
    }
    wait(|| image_present(fixture.session));
    unsafe {
        assert!(cleat_session_transfer(fixture.session, b"default".as_ptr(), 7), "{}", fixture.error());
    }
    wait(|| image_present(fixture.session));
    unsafe {
        assert!(cleat_session_adopt(fixture.session), "{}", fixture.error());
    }
    wait(|| image_present(fixture.session));
}
