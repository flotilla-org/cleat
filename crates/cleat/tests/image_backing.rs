#![cfg(all(unix, feature = "ghostty-vt"))]
use std::{
    ffi::CString,
    os::fd::FromRawFd,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use cleat::provider_ffi::*;

struct Fixture {
    daemon: Child,
    provider: *mut CleatProvider,
    session: *mut CleatSession,
    _root: tempfile::TempDir,
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
fn wait(mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(start.elapsed() < Duration::from_secs(15));
        std::thread::sleep(Duration::from_millis(10));
    }
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let daemon = Command::new(env!("CARGO_BIN_EXE_cleat"))
            .arg("--runtime-root")
            .arg(root.path())
            .args(["serve"])
            .env_remove("CLEAT_DAEMON")
            .env_remove("CLEAT_SESSION")
            .env_remove("CLEAT_TEST_IMAGE_LINK_REFUSE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        wait(|| root.path().join("default@1/socket").exists());
        let path = root.path().to_str().unwrap();
        let provider = unsafe {
            cleat_provider_open(&CleatProviderDesc {
                abi_version: CLEAT_PROVIDER_ABI_VERSION,
                backend: CLEAT_PROVIDER_BACKEND_DAEMON,
                runtime_root: path.as_ptr(),
                runtime_root_len: path.len(),
                ..Default::default()
            })
        };
        assert!(!provider.is_null());
        let session = unsafe {
            cleat_session_create(provider, &CleatSessionDesc {
                cols: 80,
                rows: 24,
                id: b"image".as_ptr(),
                id_len: 5,
                command: b"cat".as_ptr(),
                command_len: 3,
                ..Default::default()
            })
        };
        assert!(!session.is_null());
        wait(|| unsafe { cleat_session_role(session) == CLEAT_ROLE_CONTROLLER });
        Self { daemon, provider, session, _root: root }
    }
    fn write(&self, bytes: &[u8]) {
        assert!(unsafe { cleat_session_write_bytes(self.session, bytes.as_ptr(), bytes.len()) });
    }
    fn view(&self, present: bool) -> Option<CleatImageResource> {
        let mut resource = None;
        wait(|| unsafe {
            let mut update = CleatRenderUpdate::default();
            assert!(cleat_session_render_update(self.session, &mut update));
            let committed = update.dirty != CleatDirtyState::Clean;
            resource = if update.image_resource_count > 0 { Some(*update.image_resources) } else { None };
            cleat_session_mark_observed(self.session, update.render_generation);
            cleat_session_release_render_update(self.session, &mut update);
            committed && resource.is_some() == present
        });
        resource
    }
    fn acquire(&self, r: CleatImageResource, kind: u32) -> CleatImageBacking {
        let mut out = CleatImageBacking::default();
        assert!(unsafe { cleat_session_image_resource_backing(self.session, r.image_id, r.generation, kind, &mut out) });
        out
    }
    fn release(&self, out: &mut CleatImageBacking) {
        unsafe {
            cleat_session_release_image_resource_backing(self.session, out);
        }
        assert!(out.name.is_null());
    }
}
fn name(out: &CleatImageBacking) -> Vec<u8> {
    unsafe { std::slice::from_raw_parts(out.name, out.name_len).to_vec() }
}
fn shm_bytes(out: &CleatImageBacking) -> Vec<u8> {
    use std::io::Read;
    let name = CString::new(name(out)).unwrap();
    let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDONLY, 0) };
    assert!(fd >= 0);
    let mut bytes: Vec<u8> = Vec::new();
    unsafe { std::fs::File::from_raw_fd(fd) }.read_to_end(&mut bytes).unwrap();
    bytes
}
unsafe extern "C" fn copy(user: *mut std::ffi::c_void, data: *const u8, len: usize) -> bool {
    unsafe {
        *(user as *mut Vec<u8>) = std::slice::from_raw_parts(data, len).to_vec();
    }
    true
}
fn exercise(refused: bool) {
    let f = Fixture::new();
    f.write(b"\x1b[H\x1b_Ga=T,C=1,q=2,i=77,p=1,f=24,s=1,v=1,c=1,r=1;AQID\x1b\\\n");
    let r = f.view(true).unwrap();
    let mut bytes: Vec<u8> = Vec::new();
    assert!(unsafe {
        cleat_session_with_image_resource_data(f.session, r.image_id, r.generation, Some(copy), (&mut bytes as *mut Vec<u8>).cast())
    });
    assert_eq!(bytes, [1u8, 2, 3]);
    let mut file = CleatImageBacking::default();
    let success = unsafe { cleat_session_image_resource_backing(f.session, r.image_id, r.generation, CLEAT_IMAGE_BACKING_FILE, &mut file) };
    assert_eq!(success, !refused);
    let path = success.then(|| std::path::PathBuf::from(String::from_utf8(name(&file)).unwrap()));
    if let Some(path) = &path {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
    let mut shm = f.acquire(r, CLEAT_IMAGE_BACKING_SHM);
    let mut second = f.acquire(r, CLEAT_IMAGE_BACKING_SHM);
    assert_ne!(name(&shm), name(&second));
    assert_eq!(shm_bytes(&shm), bytes);
    assert_eq!(
        (shm.width_px, shm.height_px, shm.format, shm.compression, shm.data_len),
        (r.width_px, r.height_px, r.format, r.compression, r.data_len)
    );
    // Release frees only library name storage: caller ownership survives it.
    let shm_name = CString::new(name(&shm)).unwrap();
    f.release(&mut shm);
    let fd = unsafe { libc::shm_open(shm_name.as_ptr(), libc::O_RDONLY, 0) };
    assert!(fd >= 0);
    drop(unsafe { std::fs::File::from_raw_fd(fd) });
    assert_eq!(unsafe { libc::shm_unlink(shm_name.as_ptr()) }, 0);
    assert_eq!(unsafe { libc::shm_unlink(CString::new(name(&second)).unwrap().as_ptr()) }, 0);
    f.release(&mut second);
    if let Some(path) = &path {
        f.write(b"later render\n");
        f.view(true);
        f.release(&mut file);
        assert_eq!(std::fs::read(path).unwrap(), bytes);
        file = f.acquire(r, CLEAT_IMAGE_BACKING_FILE);
        f.write(b"\x1b_Ga=d,d=A,q=2\x1b\\\n");
        f.view(false);
        assert_eq!(std::fs::read(path).unwrap(), bytes);
        f.release(&mut file);
        wait(|| !path.exists());
    }
    // Invalid and absent generations fail without altering the caller output.
    for (generation, kind) in [(u64::MAX, CLEAT_IMAGE_BACKING_SHM), (r.generation, 99)] {
        let mut out = CleatImageBacking { data_len: 123, ..Default::default() };
        assert!(!unsafe { cleat_session_image_resource_backing(f.session, r.image_id, generation, kind, &mut out) });
        assert_eq!(out.data_len, 123);
        let mut error = CleatStr::default();
        assert!(unsafe { cleat_session_image_resource_backing_error(f.session, &mut error) });
        assert!(error.len > 0);
    }
}
// Issue #294: daemon C ABI preserves FILE lifetimes and transfers independent SHM ownership.
#[test]
fn daemon_backing_lifecycle() {
    exercise(false);
}
// A process boundary isolates forced filesystem refusal from parallel tests.
#[test]
fn hard_link_refusal_falls_back_to_shm() {
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "refused_child", "--nocapture"])
        .env("CLEAT_TEST_IMAGE_LINK_REFUSE", "1")
        .status()
        .unwrap();
    assert!(status.success());
}
// Issue #294: byte-delivered generations refuse FILE but still support SHM.
#[test]
fn refused_child() {
    if std::env::var_os("CLEAT_TEST_IMAGE_LINK_REFUSE").is_some() {
        exercise(true);
    }
}
