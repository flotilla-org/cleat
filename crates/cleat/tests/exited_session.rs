#![cfg(all(unix, feature = "ghostty-vt"))]

//! An exited program cannot redraw, so its session keeps the last frame
//! through resizes, and the C API reports the exit with its status.

use std::{
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use cleat::{provider_ffi::*, runtime::RuntimeLayout, server::SessionService};

/// Enters the alternate screen and draws a marker at column 50 of row 0 and
/// another on row 20, redrawing after every resize while it runs (recording
/// the size it saw). Exits with status 3 once the test creates `go`.
const PROGRAM: &str = r#"
draw() {
    printf '\033[2J\033[1;51HCOLMARK\033[21;1HROWMARK'
    stty size > size.tmp && mv size.tmp size
}
trap draw WINCH
printf '\033[?1049h'
draw
while [ ! -e go ]; do sleep 0.02; done
exit 3
"#;

struct DaemonProcess(Child);

impl Drop for DaemonProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The cells a client holds, applied from render updates the way a UI shell
/// stores them: a smaller frame keeps the cells that still fit.
#[derive(Clone, Debug, Default, PartialEq)]
struct Frame {
    cols: usize,
    rows: usize,
    cells: Vec<Vec<String>>,
}

impl Frame {
    fn reshape(&mut self, cols: usize, rows: usize) {
        self.cells.resize(rows, Vec::new());
        for row in &mut self.cells {
            row.resize(cols, String::new());
        }
        (self.cols, self.rows) = (cols, rows);
    }

    unsafe fn replace_rows(&mut self, rows: &[CleatRenderRow]) {
        for row in rows {
            let Some(target) = self.cells.get_mut(row.row as usize) else {
                continue;
            };
            target.iter_mut().for_each(String::clear);
            for (col, cell) in unsafe { slice(row.cells, row.cell_count) }.iter().enumerate().take(self.cols) {
                target[col] = unsafe { slice(cell.graphemes, cell.grapheme_count) }.iter().filter_map(|ch| char::from_u32(*ch)).collect();
            }
        }
    }

    unsafe fn apply(&mut self, update: &CleatRenderUpdate) {
        self.reshape(update.cols as usize, update.rows as usize);
        for op in unsafe { slice(update.ops, update.op_count) } {
            let rows = unsafe { slice(op.rows, op.row_desc_count) };
            match op.kind {
                CLEAT_RENDER_OP_FULL_VISIBLE_REPLACE => {
                    self.cells.iter_mut().flatten().for_each(String::clear);
                    unsafe { self.replace_rows(rows) };
                }
                CLEAT_RENDER_OP_ROW_REPLACE => unsafe { self.replace_rows(rows) },
                CLEAT_RENDER_OP_SCROLL_COPY => {
                    let (src, dst, count) = (op.src_row as usize, op.dst_row as usize, op.row_count as usize);
                    let moved: Vec<_> = self.cells.iter().skip(src).take(count).cloned().collect();
                    for (offset, row) in moved.into_iter().enumerate() {
                        if let Some(target) = self.cells.get_mut(dst + offset) {
                            *target = row;
                        }
                    }
                }
                kind => panic!("unknown render op {kind}"),
            }
        }
    }

    fn text_at(&self, row: usize, col: usize, len: usize) -> String {
        self.cells.get(row).map(|cells| cells.iter().skip(col).take(len).map(String::as_str).collect()).unwrap_or_default()
    }

    fn has_markers(&self) -> bool {
        self.text_at(0, 50, 7) == "COLMARK" && self.text_at(20, 0, 7) == "ROWMARK"
    }
}

unsafe fn slice<'a, T>(ptr: *const T, len: usize) -> &'a [T] {
    if len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(ptr, len) }
    }
}

#[track_caller]
fn wait(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

struct Client {
    session: *mut CleatSession,
    frame: Frame,
    generation: u64,
}

impl Client {
    /// Apply every pending render update to the stored frame.
    fn pump(&mut self) {
        unsafe {
            let mut update = CleatRenderUpdate::default();
            if cleat_session_render_update(self.session, &mut update) {
                self.frame.apply(&update);
                self.generation = update.render_generation;
                cleat_session_mark_observed(self.session, update.render_generation);
                cleat_session_release_render_update(self.session, &mut update);
            }
        }
    }

    /// Resize, then take in whatever the session renders for the new size.
    fn resize(&mut self, cols: u16, rows: u16) {
        assert!(unsafe { cleat_session_resize(self.session, cols, rows) });
        let settle = Instant::now() + Duration::from_millis(300);
        while Instant::now() < settle {
            self.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn program_state(&self) -> (u32, Option<i32>) {
        let mut code = i32::MIN;
        let state = unsafe { cleat_session_program_state(self.session, &mut code) };
        (state, (code != i32::MIN).then_some(code))
    }
}

fn run_exited_session_scenario(backend: u32) {
    let temp = tempfile::Builder::new().prefix("cleat-exited-").tempdir_in("/tmp").unwrap();
    let dir = temp.path();
    let root = dir.to_str().unwrap();
    let _daemon = (backend == CLEAT_PROVIDER_BACKEND_DAEMON).then(|| {
        let daemon = DaemonProcess(
            Command::new(env!("CARGO_BIN_EXE_cleat"))
                .args(["--runtime-root", root, "--server", "default", "serve"])
                .env_remove("CLEAT_DAEMON")
                .env_remove("CLEAT_SESSION")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        wait("daemon socket", || RuntimeLayout::new(dir.into()).socket_path().exists());
        daemon
    });
    std::fs::write(dir.join("program.sh"), PROGRAM).unwrap();
    let command = format!("sh -c 'cd {root} && . ./program.sh'");
    let size = || std::fs::read_to_string(dir.join("size")).unwrap_or_default();
    unsafe {
        let provider = cleat_provider_open(&CleatProviderDesc {
            abi_version: CLEAT_PROVIDER_ABI_VERSION,
            backend,
            runtime_root: root.as_ptr(),
            runtime_root_len: root.len(),
            ..Default::default()
        });
        assert!(!provider.is_null());
        let session = cleat_session_create(provider, &CleatSessionDesc {
            cols: 109,
            rows: 25,
            vt_engine: CLEAT_PROVIDER_VT_GHOSTTY,
            id: b"exited".as_ptr(),
            id_len: 6,
            command: command.as_ptr(),
            command_len: command.len(),
            ..Default::default()
        });
        assert!(!session.is_null());
        let mut client = Client { session, frame: Frame::default(), generation: 0 };
        if backend == CLEAT_PROVIDER_BACKEND_DAEMON {
            wait("controller grant", || cleat_session_role(session) == CLEAT_ROLE_CONTROLLER);
        }
        wait("markers drawn", || {
            client.pump();
            client.frame.has_markers() && size().trim() == "25 109"
        });
        assert_eq!(client.program_state(), (CLEAT_PROGRAM_RUNNING, None));

        // A running program still gets its resizes and redraws for them.
        client.resize(43, 10);
        wait("running program sees the shrink", || size().trim() == "10 43");
        client.resize(109, 25);
        wait("running program redraws after the grow", || {
            client.pump();
            client.frame.has_markers() && size().trim() == "25 109"
        });

        std::fs::write(dir.join("go"), "").unwrap();
        wait("exit reported", || {
            client.pump();
            client.program_state() == (CLEAT_PROGRAM_EXITED, Some(3))
        });
        if backend == CLEAT_PROVIDER_BACKEND_DAEMON {
            wait("channel closed", || cleat_session_connection_state(session) == CLEAT_SESSION_CLOSED);
        } else {
            assert_eq!(cleat_session_connection_state(session), CLEAT_SESSION_STREAMING);
        }
        client.pump();
        let (last, generation) = (client.frame.clone(), client.generation);
        assert_eq!((last.cols, last.rows), (109, 25));
        assert!(last.has_markers(), "{last:?}");

        // Nothing can redraw an exited program: resizes leave its frame alone.
        client.resize(43, 10);
        client.resize(109, 25);
        assert_eq!(client.frame, last, "backend={backend}");
        // No new frame was rendered, so none can still be on its way.
        client.pump();
        assert_eq!(client.generation, generation, "backend={backend}");
        assert_eq!(client.program_state(), (CLEAT_PROGRAM_EXITED, Some(3)));

        cleat_session_destroy(session);
        cleat_provider_close(provider);
    }
    if backend == CLEAT_PROVIDER_BACKEND_DAEMON {
        let _ = SessionService::new(RuntimeLayout::new(dir.into())).kill("exited");
    }
}

#[test]
fn in_process_exited_session_keeps_its_frame_and_reports_exit() {
    run_exited_session_scenario(CLEAT_PROVIDER_BACKEND_IN_PROCESS);
}

#[test]
fn daemon_exited_session_keeps_its_frame_and_reports_exit() {
    run_exited_session_scenario(CLEAT_PROVIDER_BACKEND_DAEMON);
}

#[test]
fn program_state_of_null_is_unknown_and_of_mock_is_running() {
    unsafe {
        assert_eq!(cleat_session_program_state(std::ptr::null(), std::ptr::null_mut()), CLEAT_PROGRAM_UNKNOWN);
        let provider = cleat_provider_open(&CleatProviderDesc {
            abi_version: CLEAT_PROVIDER_ABI_VERSION,
            backend: CLEAT_PROVIDER_BACKEND_MOCK,
            ..Default::default()
        });
        let session = cleat_session_create(provider, &CleatSessionDesc::default());
        assert!(!session.is_null());
        assert_eq!(cleat_session_program_state(session, std::ptr::null_mut()), CLEAT_PROGRAM_RUNNING);
        cleat_session_destroy(session);
        cleat_provider_close(provider);
    }
}
