#![cfg(all(unix, feature = "ghostty-vt"))]
//! A provider reaching a daemon over an explicit transport: a command whose
//! stdio carries the packet stream (`cleat proxy`, as `ssh host cleat proxy`
//! runs it), or a Unix socket. Both attach the same packet-protocol view, and
//! neither starts, discovers or manages a local daemon.

use std::{
    fs,
    path::Path,
    process::Command,
    thread,
    time::{Duration, Instant},
};

use cleat::{
    provider::ProviderFeatures, provider_ffi::*, proxy_command::shell_word, runtime::RuntimeLayout, server::SessionService,
    vt::VtEngineKind,
};

struct Daemon {
    root: tempfile::TempDir,
    layout: RuntimeLayout,
    service: SessionService,
}

impl Daemon {
    fn new() -> Self {
        let root = tempfile::Builder::new().prefix("cleat-remote-").tempdir_in("/tmp").unwrap();
        let layout = RuntimeLayout::new(root.path().to_owned());
        let service = SessionService::new(layout.clone());
        service.create(Some("remote".into()), Some(VtEngineKind::Ghostty), None, Some("cat".into()), false).unwrap();
        Self { root, layout, service }
    }

    fn proxy_argv(&self) -> Vec<String> {
        proxy_argv(self.root.path())
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.service.kill("remote");
        if let Ok(pid) = fs::read_to_string(self.layout.daemon_pid_path()) {
            if let Ok(pid) = pid.trim().parse::<i32>() {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
    }
}

fn proxy_argv(root: &Path) -> Vec<String> {
    vec![env!("CARGO_BIN_EXE_cleat").into(), "proxy".into(), "--runtime-root".into(), root.display().to_string()]
}

fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

enum Kind<'a> {
    Command(&'a [String]),
    Socket(&'a Path),
}

struct Provider(*mut CleatProvider);

impl Provider {
    fn open(kind: Kind<'_>) -> Self {
        let argv: Vec<CleatStr>;
        let path: String;
        let mut transport = CleatProviderTransport::default();
        match kind {
            Kind::Command(command) => {
                argv = command.iter().map(|arg| CleatStr { ptr: arg.as_ptr(), len: arg.len() }).collect();
                transport.kind = CLEAT_TRANSPORT_COMMAND;
                transport.argv = argv.as_ptr();
                transport.argc = argv.len();
            }
            Kind::Socket(socket) => {
                path = socket.display().to_string();
                transport.kind = CLEAT_TRANSPORT_UNIX_SOCKET;
                transport.socket_path = path.as_ptr();
                transport.socket_path_len = path.len();
            }
        }
        let desc = CleatProviderDesc {
            abi_version: CLEAT_PROVIDER_ABI_VERSION,
            requested_features: ProviderFeatures::CELL_SNAPSHOTS.bits(),
            backend: CLEAT_PROVIDER_BACKEND_DAEMON,
            ..CleatProviderDesc::default()
        };
        let provider = unsafe { cleat_provider_open_transport(&desc, &transport) };
        assert!(!provider.is_null());
        Self(provider)
    }

    fn state(&self) -> serde_json::Value {
        let mut len = 0;
        assert!(unsafe { cleat_provider_transport_state_json(self.0, std::ptr::null_mut(), 0, &mut len) });
        let mut buffer = vec![0u8; len];
        assert!(unsafe { cleat_provider_transport_state_json(self.0, buffer.as_mut_ptr(), buffer.len(), &mut len) });
        serde_json::from_slice(&buffer[..len]).unwrap()
    }

    /// The state as fields, with `capacity`-byte text buffers.
    fn fields(&self, capacity: usize) -> Fields {
        let (mut error, mut warning) = (vec![0u8; capacity], vec![0u8; capacity]);
        let mut state = CleatTransportState {
            error: error.as_mut_ptr(),
            error_capacity: capacity,
            warning: warning.as_mut_ptr(),
            warning_capacity: capacity,
            ..CleatTransportState::default()
        };
        assert!(unsafe { cleat_provider_transport_state(self.0, &mut state) });
        let read = |buffer: &[u8], len: usize| String::from_utf8(buffer[..len].to_vec()).expect("cut at a character boundary");
        Fields {
            connected: state.connected,
            retrying: state.retrying,
            error: read(&error, state.error_len),
            warning: read(&warning, state.warning_len),
        }
    }

    fn attach(&self, id: &str) -> Session {
        let session = unsafe {
            cleat_session_attach(self.0, &CleatSessionDesc {
                cols: 80,
                rows: 24,
                id: id.as_ptr(),
                id_len: id.len(),
                ..CleatSessionDesc::default()
            })
        };
        assert!(!session.is_null());
        Session(session)
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        unsafe { cleat_provider_close(self.0) };
    }
}

#[derive(Debug, PartialEq)]
struct Fields {
    connected: bool,
    retrying: bool,
    error: String,
    warning: String,
}

impl Fields {
    /// The same state as the JSON form reports, absent texts as empty.
    fn of_json(state: &serde_json::Value) -> Self {
        let text = |key: &str| state[key].as_str().unwrap_or_default().to_string();
        Self {
            connected: state["connected"].as_bool().unwrap(),
            retrying: state["retrying"].as_bool().unwrap(),
            error: text("error"),
            warning: text("warning"),
        }
    }
}

struct Session(*mut CleatSession);

impl Session {
    fn state(&self) -> u32 {
        unsafe { cleat_session_connection_state(self.0) }
    }

    /// Consume pending render updates, acknowledging them so more can come.
    fn drain(&self) {
        let mut update = CleatRenderUpdate::default();
        if unsafe { cleat_session_render_update(self.0, &mut update) } {
            unsafe { cleat_session_release_render_update(self.0, &mut update) };
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe { cleat_session_destroy(self.0) };
    }
}

/// The contract both transports meet: attach as controller, stream renders,
/// round-trip input and resize, report remote hosting, keep the admission
/// warning out of the content, and refuse what needs the daemon's host.
fn attaches_and_round_trips(daemon: &Daemon, provider: &Provider) {
    let session = provider.attach("remote");
    eventually("controller grant", || unsafe { cleat_session_role(session.0) } == CLEAT_ROLE_CONTROLLER);
    eventually("streaming", || {
        session.drain();
        session.state() == CLEAT_SESSION_STREAMING
    });

    assert!(unsafe { cleat_session_write_bytes(session.0, b"over-the-transport\n".as_ptr(), 19) });
    eventually("input reaches the PTY", || {
        session.drain();
        daemon.service.capture("remote").unwrap().contains("over-the-transport")
    });
    assert!(unsafe { cleat_session_resize(session.0, 100, 30) });
    eventually("resize reaches the PTY", || {
        session.drain();
        let terminal = daemon.service.inspect("remote").unwrap().terminal;
        (terminal.cols, terminal.rows) == (100, 30)
    });

    let mut hosting = CleatStr::default();
    assert!(unsafe { cleat_session_hosting(session.0, &mut hosting) });
    let hosting = unsafe { std::str::from_utf8(std::slice::from_raw_parts(hosting.ptr, hosting.len)).unwrap() };
    assert!(hosting.starts_with("remote:"), "{hosting}");

    let state = provider.state();
    assert_eq!(state["connected"], true, "{state}");
    assert!(state["error"].is_null(), "{state}");
    let warning = state["warning"].as_str().unwrap_or_default();
    assert!(warning.contains("cycle protection does not cover remote relationships"), "{state}");
    assert!(!daemon.service.capture("remote").unwrap().contains("cycle protection"), "warning leaked into content");

    let created = unsafe {
        cleat_session_create(provider.0, &CleatSessionDesc {
            cols: 80,
            rows: 24,
            id: b"made-remotely".as_ptr(),
            id_len: 13,
            ..CleatSessionDesc::default()
        })
    };
    assert!(created.is_null(), "a remote transport must not create sessions");
    assert!(!unsafe { cleat_session_end(session.0) }, "a remote transport must not end sessions");
    assert!(!unsafe { cleat_session_adopt(session.0) }, "a remote transport must not adopt sessions");
    assert!(daemon.service.inspect("remote").is_ok());
}

#[test]
fn command_transport_attaches_through_cleat_proxy() {
    let daemon = Daemon::new();
    let argv = daemon.proxy_argv();
    let provider = Provider::open(Kind::Command(&argv));
    attaches_and_round_trips(&daemon, &provider);
    assert_eq!(provider.state()["kind"], "command");
}

#[test]
fn unix_socket_transport_behaves_identically() {
    let daemon = Daemon::new();
    let provider = Provider::open(Kind::Socket(&daemon.layout.socket_path()));
    attaches_and_round_trips(&daemon, &provider);
    assert_eq!(provider.state()["kind"], "unix_socket");
}

// A transfer moves the session to a daemon the transport cannot reach, so the
// channel closes rather than reconnecting at the target.
#[test]
fn remote_transport_does_not_follow_transfer_redirect() {
    let daemon = Daemon::new();
    let target = daemon.service.with_daemon("target".into()).unwrap();
    let argv = daemon.proxy_argv();
    let provider = Provider::open(Kind::Command(&argv));
    let session = provider.attach("remote");
    eventually("streaming", || {
        session.drain();
        session.state() == CLEAT_SESSION_STREAMING
    });
    daemon.service.transfer("remote", &target, Default::default()).unwrap();
    eventually("channel closed", || {
        session.drain();
        session.state() == CLEAT_SESSION_CLOSED
    });
    let attachments = target.inspect("remote").unwrap().attachments;
    target.kill("remote").unwrap();
    let target_layout = daemon.layout.clone().with_daemon("target".into()).unwrap();
    if let Ok(pid) = fs::read_to_string(target_layout.daemon_pid_path()) {
        unsafe { libc::kill(pid.trim().parse().unwrap(), libc::SIGKILL) };
    }
    assert!(attachments.is_empty(), "a remote transport must not reconnect at the transfer target");
}

#[test]
fn malformed_transports_are_rejected() {
    let desc = CleatProviderDesc {
        abi_version: CLEAT_PROVIDER_ABI_VERSION,
        backend: CLEAT_PROVIDER_BACKEND_DAEMON,
        ..CleatProviderDesc::default()
    };
    let program = "cleat".to_string();
    let argv = [CleatStr { ptr: program.as_ptr(), len: program.len() }];
    let empty = [CleatStr { ptr: program.as_ptr(), len: 0 }];
    let socket = "/nonexistent.sock";
    let valid = CleatProviderTransport { kind: CLEAT_TRANSPORT_COMMAND, argv: argv.as_ptr(), argc: 1, ..Default::default() };
    let cases = [
        ("short struct", CleatProviderTransport { size: 8, ..valid }),
        ("unknown kind", CleatProviderTransport { kind: 99, ..valid }),
        ("no argv", CleatProviderTransport { argc: 0, ..valid }),
        ("empty program", CleatProviderTransport { argv: empty.as_ptr(), ..valid }),
        ("no socket path", CleatProviderTransport { kind: CLEAT_TRANSPORT_UNIX_SOCKET, ..Default::default() }),
    ];
    for (what, transport) in cases {
        assert!(unsafe { cleat_provider_open_transport(&desc, &transport) }.is_null(), "{what}");
    }
    let mock = CleatProviderDesc { backend: CLEAT_PROVIDER_BACKEND_MOCK, ..desc };
    let socket = CleatProviderTransport {
        kind: CLEAT_TRANSPORT_UNIX_SOCKET,
        socket_path: socket.as_ptr(),
        socket_path_len: socket.len(),
        ..Default::default()
    };
    assert!(unsafe { cleat_provider_open_transport(&mock, &socket) }.is_null(), "only the daemon backend has transports");
}

// The command is the transport: when it dies, the view disconnects promptly,
// nothing respawns it, and the session keeps running on its daemon.
#[test]
fn killing_the_proxy_disconnects_without_respawning() {
    let daemon = Daemon::new();
    let pidfile = daemon.root.path().join("proxy.pid");
    let proxy = daemon.proxy_argv().iter().map(|arg| format!("'{arg}'")).collect::<Vec<_>>().join(" ");
    let argv = vec!["sh".into(), "-c".into(), format!("echo $$ > '{}'; exec {proxy}", pidfile.display())];
    let provider = Provider::open(Kind::Command(&argv));
    let session = provider.attach("remote");
    eventually("streaming", || {
        session.drain();
        session.state() == CLEAT_SESSION_STREAMING
    });

    let pid: i32 = fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
    unsafe { libc::kill(pid, libc::SIGKILL) };
    let killed = Instant::now();
    eventually("disconnected", || session.state() == CLEAT_SESSION_DISCONNECTED);
    assert!(killed.elapsed() < Duration::from_secs(1), "disconnect took {:?}", killed.elapsed());
    eventually("the transport gives up", || provider.state()["retrying"] == false);
    let state = provider.state();
    assert_eq!(state["connected"], false, "{state}");
    assert!(state["error"].as_str().is_some_and(|error| !error.is_empty()), "{state}");

    thread::sleep(Duration::from_millis(300));
    assert_eq!(session.state(), CLEAT_SESSION_DISCONNECTED, "the command must not be respawned");
    assert!(daemon.service.inspect("remote").is_ok(), "the daemon session keeps running");
}

// With no daemon at the far end, the host learns the proxy's own reason.
#[test]
fn missing_daemon_reports_the_proxy_reason() {
    let empty = tempfile::Builder::new().prefix("cleat-remote-empty-").tempdir_in("/tmp").unwrap();
    let argv = proxy_argv(empty.path());
    let provider = Provider::open(Kind::Command(&argv));
    eventually("the transport gives up", || provider.state()["retrying"] == false);
    let state = provider.state();
    let error = state["error"].as_str().unwrap_or_default();
    assert!(error.contains("is not reachable"), "{state}");
    assert!(!empty.path().join("default").exists(), "the proxy must not start a daemon");
}

// Closing stdin half-closes the daemon socket; the answer to what was already
// sent still comes back before the proxy exits.
#[test]
fn proxy_delivers_replies_after_stdin_closes() {
    use std::io::Write;
    let daemon = Daemon::new();
    let mut proxy = Command::new(env!("CARGO_BIN_EXE_cleat"))
        .args(["proxy", "--runtime-root"])
        .arg(daemon.root.path())
        .env_remove("CLEAT_DAEMON")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = proxy.stdin.take().unwrap();
    stdin.write_all(b"GET /healthz HTTP/1.1\r\nHost: cleat\r\nConnection: close\r\n\r\n").unwrap();
    drop(stdin);
    let output = proxy.wait_with_output().unwrap();
    let reply = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{:?}", output.status);
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
}

#[test]
fn proxy_cli_reports_an_unreachable_daemon_on_one_line() {
    let empty = tempfile::Builder::new().prefix("cleat-proxy-cli-").tempdir_in("/tmp").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_cleat"))
        .args(["proxy", "--daemon", "work", "--runtime-root"])
        .arg(empty.path())
        .env_remove("CLEAT_DAEMON")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty(), "stdout is the data channel");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr.trim_end().lines().count(), 1, "{stderr}");
    assert!(stderr.contains("daemon work is not reachable"), "{stderr}");
}

/// The far end's command line from `cleat_proxy_command`.
fn proxy_command(target: &CleatProxyTarget) -> String {
    let mut len = 0;
    assert!(unsafe { cleat_proxy_command(target, std::ptr::null_mut(), 0, &mut len) });
    let mut buffer = vec![0u8; len];
    assert!(unsafe { cleat_proxy_command(target, buffer.as_mut_ptr(), len, &mut len) });
    String::from_utf8(buffer).unwrap()
}

// A client-arranged route (here a plain `sh -c`, where ssh or an exec would
// hand the far end its command) works with the command line Cleat builds.
// The fields form of the transport state agrees with the JSON form while
// connected and after the command dies, and cuts its texts to the caller's
// buffers; a state struct smaller than this ABI's is refused.
#[test]
fn transport_state_fields_follow_a_client_arranged_route() {
    let daemon = Daemon::new();
    let pidfile = daemon.root.path().join("proxy.pid");
    let (cleat, root) = (env!("CARGO_BIN_EXE_cleat"), daemon.root.path().display().to_string());
    let target = CleatProxyTarget {
        cleat: CleatStr { ptr: cleat.as_ptr(), len: cleat.len() },
        runtime_root: CleatStr { ptr: root.as_ptr(), len: root.len() },
        ..CleatProxyTarget::default()
    };
    let route = format!("echo $$ > {}; exec {}", shell_word(&pidfile.display().to_string()), proxy_command(&target));
    let argv = vec!["sh".into(), "-c".into(), route];
    let provider = Provider::open(Kind::Command(&argv));
    let session = provider.attach("remote");
    eventually("streaming", || {
        session.drain();
        session.state() == CLEAT_SESSION_STREAMING
    });

    let up = provider.fields(256);
    assert_eq!(up, Fields::of_json(&provider.state()));
    assert!(up.connected && up.error.is_empty(), "{up:?}");
    assert!(up.warning.contains("cycle protection does not cover remote relationships"), "{up:?}");
    assert_eq!(provider.fields(5).warning, up.warning[..5], "the warning is cut to the caller's buffer");
    let mut small = CleatTransportState { size: 8, ..CleatTransportState::default() };
    assert!(!unsafe { cleat_provider_transport_state(provider.0, &mut small) });

    let pid: i32 = fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
    unsafe { libc::kill(pid, libc::SIGKILL) };
    eventually("the transport gives up", || !provider.fields(256).retrying);
    let down = provider.fields(256);
    assert_eq!(down, Fields::of_json(&provider.state()));
    assert!(!down.connected && !down.error.is_empty(), "{down:?}");
}

// A text cut to fit the caller's buffer never splits a character: the
// error names a runtime root holding `é`, and a buffer ending inside its two
// bytes gets the text up to it.
#[test]
fn transport_state_texts_are_cut_at_a_character_boundary() {
    let empty = tempfile::Builder::new().prefix("cleat-remote-é-").tempdir_in("/tmp").unwrap();
    let argv = proxy_argv(empty.path());
    let provider = Provider::open(Kind::Command(&argv));
    eventually("the transport gives up", || !provider.fields(512).retrying);
    let error = provider.fields(512).error;
    let accent = error.find('é').expect("the error names the root");
    assert_eq!(provider.fields(accent + 1).error, error[..accent]);
}
