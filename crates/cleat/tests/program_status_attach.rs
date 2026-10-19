#![cfg(all(unix, feature = "ghostty-vt"))]
//! Scripted outer terminals exercise the real CLI, daemon, PTY and VT engine.
use std::{
    fs::File,
    io::{Read, Write},
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use cleat::{runtime::RuntimeLayout, server::SessionService, vt::VtEngineKind};

const QUERY: &[u8] = b"\x1b]7501;?\x1b\\";
const WORKING: &[u8] = b"\x1b]7501;state=working:id=task\x1b\\";
const DONE: &[u8] = b"\x1b]7501;state=done:id=task\x1b\\";
const CLEAR: &[u8] = b"\x1b]7501;state=clear:id=task\x1b\\";
struct Outer {
    master: File,
    child: Child,
    bytes: Vec<u8>,
    screen: cleat::vt::ghostty::GhosttyVtEngine,
}
impl Outer {
    fn attach(root: &Path, verb: &str, mode: &str) -> Self {
        // A real PTY stands in for the user's outer terminal process boundary.
        let pty = nix::pty::openpty(Some(&nix::pty::Winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 }), None).unwrap();
        let slave = File::from(pty.slave);
        let master = File::from(pty.master);
        let mut command = Command::new(env!("CARGO_BIN_EXE_cleat"));
        command
            .arg("--runtime-root")
            .arg(root)
            .args([verb, "status", &format!("--program-status={mode}")])
            .env_remove("CLEAT_SESSION")
            .env_remove("CLEAT_DAEMON")
            .env_remove("CLEAT_OUTPUT_DAEMON")
            .env_remove("CLEAT_OUTPUT_SESSION")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        if verb == "attach" {
            command.args(["--no-create", "--no-record"]);
        }
        // SAFETY: child-only async-signal-safe syscalls establish its controlling terminal.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        // SAFETY: the owned master descriptor is live; preserve its existing flags.
        unsafe {
            let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
            assert!(flags >= 0);
            assert_eq!(libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK), 0);
        }
        Self { master, child, bytes: Vec::new(), screen: cleat::vt::ghostty::GhosttyVtEngine::new(80, 24) }
    }
    fn drain(&mut self) {
        let mut buf = [0; 65536];
        loop {
            match self.master.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    use cleat::vt::VtEngine;
                    self.screen.feed(&buf[..n]).unwrap();
                    self.bytes.extend_from_slice(&buf[..n]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.raw_os_error() == Some(libc::EIO) => break,
                Err(e) => panic!("outer read: {e}"),
            }
        }
    }
    fn wait_for(&mut self, expected: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.drain();
            if self.bytes.windows(expected.len()).any(|b| b == expected) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "missing {:?}; received {:?}; child {:?}",
                String::from_utf8_lossy(expected),
                String::from_utf8_lossy(&self.bytes[self.bytes.len().saturating_sub(1000)..]),
                self.child.try_wait()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn wait_screen(&mut self, expected: &str) {
        use cleat::vt::VtEngine;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.drain();
            let text = self.screen.screen_text().unwrap();
            if text.contains(expected) {
                return;
            }
            assert!(Instant::now() < deadline, "missing {expected:?} in {text:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn input(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).unwrap();
    }
    fn detach(&mut self) {
        self.input(b"\x1dd");
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.child.try_wait().unwrap().is_none() {
            self.drain();
            assert!(Instant::now() < deadline, "attach did not detach");
            std::thread::sleep(Duration::from_millis(10));
        }
        self.drain();
    }
}
impl Drop for Outer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
struct Session {
    _temp: tempfile::TempDir,
    service: SessionService,
}
impl Session {
    fn new(engine: VtEngineKind, script: &str, record: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let service = SessionService::new(RuntimeLayout::new(temp.path().to_owned()));
        service.create(Some("status".into()), Some(engine), None, Some(script.into()), record).unwrap();
        Self { _temp: temp, service }
    }
    fn root(&self) -> &Path {
        self._temp.path()
    }
    fn wait_status(&self, state: cleat::program_status::ProgramState) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if self.service.inspect("status").unwrap().program_status.iter().any(|r| r.state == state) {
                return;
            }
            assert!(Instant::now() < deadline, "status not observed");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.service.kill("status");
    }
}
const SCRIPT: &str =
    r#"sh -c 'printf "\033]7501;state=working:id=task\007"; read line; printf "\033]7501;state=done:id=task\007"; sleep 30'"#;

// Answered probes replay existing records, emit replacement reports and clear on detach.
// Each live attachment, including watchers, emits independently.
#[test]
fn answered_probe_replay_change_watchers_and_detach() {
    let script = r#"sh -c 'i=0; while [ "$i" -lt 60 ]; do printf "line %s\n" "$i"; i=$((i+1)); done; printf "\033]7501;state=working:id=task\007"; read line; printf "\033]7501;state=done:id=task\007"; sleep 30'"#;
    let session = Session::new(VtEngineKind::Ghostty, script, false);
    session.wait_status(cleat::program_status::ProgramState::Working);
    let mut driver = Outer::attach(session.root(), "attach", "auto");
    driver.wait_for(QUERY);
    driver.input(QUERY);
    driver.wait_for(WORKING);
    let mut watcher = Outer::attach(session.root(), "watch", "auto");
    watcher.wait_for(QUERY);
    watcher.input(QUERY);
    watcher.wait_for(WORKING);
    let mut history = Outer::attach(session.root(), "watch", "on");
    history.wait_for(WORKING);
    history.input(b"\x1d[");
    history.wait_for(CLEAR);
    let history_start = history.bytes.len();
    session.service.send_keys("status", b"\n").unwrap();
    driver.wait_for(DONE);
    watcher.wait_for(DONE);
    history.drain();
    assert!(!history.bytes[history_start..].windows(DONE.len()).any(|bytes| bytes == DONE));
    watcher.input(b"x");
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(session.service.inspect("status").unwrap().program_status.len(), 1, "watcher typing cannot acknowledge");
    driver.input(b"x");
    driver.wait_for(CLEAR);
    watcher.wait_for(CLEAR);
    assert!(session.service.inspect("status").unwrap().program_status.is_empty());
    driver.detach();
    watcher.detach();
    history.detach();
}

// A silent probe and off mode emit no status. A late reply is consumed without
// enabling status or reaching the session. On mode replays without a probe.
#[test]
fn silent_late_reply_and_overrides() {
    for mode in ["auto", "off", "on"] {
        let session = Session::new(VtEngineKind::Ghostty, SCRIPT, true);
        session.wait_status(cleat::program_status::ProgramState::Working);
        let mut outer = Outer::attach(session.root(), "attach", mode);
        if mode == "auto" {
            outer.wait_for(QUERY);
            std::thread::sleep(Duration::from_millis(300));
            outer.input(QUERY);
        } else if mode == "on" {
            outer.wait_for(WORKING);
        }
        // A read-visible marker proves normal input still reaches the child after the late reply.
        std::thread::sleep(Duration::from_millis(100));
        outer.detach();
        assert_eq!(outer.bytes.windows(WORKING.len()).any(|b| b == WORKING), mode == "on");
        assert_eq!(outer.bytes.windows(CLEAR.len()).any(|b| b == CLEAR), mode == "on");
        if mode != "auto" {
            assert!(!outer.bytes.windows(QUERY.len()).any(|b| b == QUERY));
        }
        let transcript = session
            .service
            .capture_slice_raw("status", cleat::server::StartBound::Offset(0), cleat::server::EndBound::EndOfRecording)
            .unwrap()
            .0;
        assert!(!transcript.contains("7501;?"), "probe reply leaked into child echo: {transcript}");
    }
}

// Explicit API acknowledgement and send --ack dismiss retained results. Plain
// send preserves them, including while no interactive attachment is present.
#[test]
fn explicit_acknowledgement_and_send_opt_in() {
    for method in ["api", "send"] {
        let session = Session::new(VtEngineKind::Ghostty, SCRIPT, false);
        session.wait_status(cleat::program_status::ProgramState::Working);
        session.service.send_keys("status", b"\n").unwrap();
        session.wait_status(cleat::program_status::ProgramState::Done);
        let mut driver = Outer::attach(session.root(), "attach", "on");
        let mut watcher = Outer::attach(session.root(), "watch", "on");
        driver.wait_for(DONE);
        watcher.wait_for(DONE);
        let plain = Command::new(env!("CARGO_BIN_EXE_cleat"))
            .arg("--runtime-root")
            .arg(session.root())
            .args(["send", "status", "plain", "--no-enter"])
            .output()
            .unwrap();
        assert!(plain.status.success(), "{:?}", plain);
        assert_eq!(session.service.inspect("status").unwrap().program_status.len(), 1);
        if method == "api" {
            session.service.acknowledge_program_status("status").unwrap();
        } else {
            let ack = Command::new(env!("CARGO_BIN_EXE_cleat"))
                .arg("--runtime-root")
                .arg(session.root())
                .args(["send", "status", "user", "--no-enter", "--ack"])
                .output()
                .unwrap();
            assert!(ack.status.success(), "{:?}", ack);
        }
        assert!(session.service.inspect("status").unwrap().program_status.is_empty());
        driver.wait_for(CLEAR);
        watcher.wait_for(CLEAR);
        driver.detach();
        watcher.detach();
    }
}

// Two real cleat layers each own their support query. The program receives
// exactly one reply, and re-emitted ids survive both layers without prefixes.
#[test]
fn cleat_inside_cleat_has_exactly_one_query_reply() {
    let temp = tempfile::tempdir().unwrap();
    let service = SessionService::new(RuntimeLayout::new(temp.path().to_owned()));
    service.create(Some("inner".into()), Some(VtEngineKind::Ghostty), None,
        Some("sh -c 'printf READY; read line; stty -echo -icanon min 0 time 2; printf \"\\033]7501;?\\033\\\\\"; reply=$(dd bs=1 count=64 2>/dev/null | od -An -tx1 | tr -d \" \\n\"); printf \"REPLY=%s_END\" \"$reply\"; printf \"\\033]7501;state=working:id=task\\007\"; sleep 30'".into()), false).unwrap();
    let command = format!(
        "'{}' --runtime-root '{}' attach inner --no-create --no-record --program-status=auto",
        env!("CARGO_BIN_EXE_cleat"),
        temp.path().display()
    );
    service.create(Some("status".into()), Some(VtEngineKind::Ghostty), None, Some(command), false).unwrap();
    let session = Session { _temp: temp, service };
    let mut outer = Outer::attach(session.root(), "attach", "auto");
    outer.wait_for(QUERY);
    outer.input(QUERY);
    outer.wait_screen("READY");
    outer.input(b"\r");
    outer.wait_screen("REPLY=1b5d373530313b3f1b5c_END");
    outer.wait_for(WORKING);
    assert_eq!(outer.bytes.windows(QUERY.len()).filter(|bytes| *bytes == QUERY).count(), 1);
    outer.detach();
    session.service.kill("inner").unwrap();
}

// Session termination clears the outer terminal even when the attachment has
// emitted a working record that the user has not detached from or acknowledged.
#[test]
fn session_end_clears_emitted_status() {
    let session = Session::new(VtEngineKind::Ghostty, SCRIPT, false);
    session.wait_status(cleat::program_status::ProgramState::Working);
    let mut outer = Outer::attach(session.root(), "attach", "on");
    outer.wait_for(WORKING);
    session.service.kill("status").unwrap();
    outer.wait_for(CLEAR);
}

// A user pressing Esc immediately after a silent probe must not wait for the
// three-second reply-discard window before the child receives the key.
#[test]
fn escape_during_probe_reaches_the_child_promptly() {
    let script = r#"sh -c 'stty -echo -icanon min 1 time 0; printf READY; byte=$(dd bs=1 count=1 2>/dev/null | od -An -tx1 | tr -d " \n"); printf "BYTE=%s_END" "$byte"; sleep 30'"#;
    let session = Session::new(VtEngineKind::Ghostty, script, false);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !session.service.capture("status").unwrap().contains("READY") {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut outer = Outer::attach(session.root(), "attach", "auto");
    outer.wait_for(QUERY);
    let started = Instant::now();
    outer.input(b"\x1b");
    outer.wait_screen("BYTE=1b_END");
    assert!(started.elapsed() < Duration::from_secs(2), "Esc waited for the probe discard window");
    outer.detach();
}
