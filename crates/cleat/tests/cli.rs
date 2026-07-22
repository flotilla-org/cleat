use clap::CommandFactory;
use cleat::{
    cli::{self, execute, Cli, Command, ExecResult, RecordFlags},
    runtime::{RuntimeLayout, TerminalSize},
    server::SessionService,
    session::session_socket_path,
    vt::{self, VtEngineKind},
};

#[test]
fn help_lists_expected_subcommands() {
    let command = Cli::command();
    let subcommands: Vec<_> = command.get_subcommands().filter(|sub| !sub.is_hide_set()).map(|sub| sub.get_name().to_string()).collect();
    assert_eq!(subcommands, vec![
        "attach",
        "watch",
        "packets",
        "launch",
        "list",
        "tag",
        "capture",
        "transcript",
        "replay",
        "detach",
        "kill",
        "send-keys",
        "inspect",
        "signal",
        "record",
        "mark",
        "send",
        "interrupt",
        "escape",
        "wait",
        "expect"
    ]);
    assert!(!subcommands.contains(&"create".to_string()), "create should not be visible in help");
}

#[test]
fn packets_command_parses() {
    let cli = Cli::try_parse_from(["cleat", "packets", "demo", "--count", "3"]).expect("packets parses");
    assert_eq!(cli.command, Command::Packets { id: "demo".into(), count: 3 });
}

#[test]
fn help_surfaces_vt_support_policy() {
    let mut command = cli::command();
    let mut buffer = Vec::new();
    command.write_long_help(&mut buffer).expect("write help");
    let help = String::from_utf8(buffer).expect("help utf8");

    assert!(help.contains("Ghostty is currently the only functional VT engine"));
    assert!(help.contains(vt::BUILD_SUPPORT_MESSAGE));
    assert!(help.contains("Typical agent workflow"));

    let mut launch = cli::command().find_subcommand_mut("launch").expect("launch command").clone();
    let mut launch_buffer = Vec::new();
    launch.write_long_help(&mut launch_buffer).expect("write launch help");
    let launch_help = String::from_utf8(launch_buffer).expect("launch help utf8");
    assert!(launch_help.contains("placeholder engines are for testing/development only"));
}

#[test]
fn attach_command_parses() {
    let cli = Cli::try_parse_from(["cleat", "attach", "demo"]).expect("attach positional parses");
    assert_eq!(cli.command, Command::Attach {
        id: Some("demo".into()),
        no_create: false,
        vt: None,
        cwd: None,
        cmd: None,
        record: RecordFlags::default()
    });
}

#[test]
fn attach_command_parses_no_create() {
    let cli = Cli::try_parse_from(["cleat", "attach", "--no-create", "demo"]).expect("attach --no-create parses");
    assert_eq!(cli.command, Command::Attach {
        id: Some("demo".into()),
        no_create: true,
        vt: None,
        cwd: None,
        cmd: None,
        record: RecordFlags::default()
    });
}

#[test]
fn attach_command_parses_vt() {
    let cli = Cli::try_parse_from(["cleat", "attach", "--vt", "passthrough", "demo"]).expect("attach --vt parses");
    assert_eq!(cli.command, Command::Attach {
        id: Some("demo".into()),
        no_create: false,
        vt: Some(VtEngineKind::Passthrough),
        cwd: None,
        cmd: None,
        record: RecordFlags::default()
    });
}

#[test]
fn watch_command_parses() {
    let cli = Cli::try_parse_from(["cleat", "watch", "demo"]).expect("watch parses");
    assert_eq!(cli.command, Command::Watch { id: "demo".into() });
}

#[test]
fn launch_command_parses() {
    let cli = Cli::try_parse_from(["cleat", "launch", "--cmd", "bash"]).expect("launch parses");
    assert_eq!(cli.command, Command::Launch {
        id: None,
        from: None,
        json: false,
        size: None,
        vt: None,
        cwd: None,
        cmd: Some("bash".into()),
        tags: Vec::new(),
        record: RecordFlags::default()
    });
}

#[test]
fn launch_from_parses_and_rejects_an_explicit_server() {
    let cli = Cli::try_parse_from(["cleat", "launch", "sibling", "--from", "source"]).expect("launch --from parses");
    assert!(matches!(cli.command, Command::Launch { from: Some(ref source), .. } if source == "source"));

    assert!(Cli::try_parse_from(["cleat", "--server", "other", "launch", "sibling", "--from", "source"]).is_err());

    let raw_cli = <Cli as clap::Parser>::try_parse_from(["cleat", "--server", "other", "launch", "sibling", "--from", "source"])
        .expect("raw clap parser accepts cross-scope globals");
    let service = SessionService::new(RuntimeLayout::new(tempfile::tempdir().expect("tempdir").path().to_path_buf()));
    assert!(execute(raw_cli, &service)
        .expect_err("execute must reject an invalid parsed CLI")
        .contains("--server cannot be used with --from"));
}

#[test]
fn launch_from_rejects_a_missing_source_session() {
    let temp = tempfile::tempdir().expect("tempdir");
    let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));
    let cli = Cli::try_parse_from(["cleat", "launch", "sibling", "--from", "missing", "--vt", "passthrough"]).expect("parse launch --from");

    let err = execute(cli, &service).expect_err("missing source should fail");

    assert!(err.contains("missing session missing"), "{err}");
}

#[test]
fn launch_command_parses_size() {
    let cli = Cli::try_parse_from(["cleat", "launch", "demo", "--size", "120x40"]).expect("launch --size parses");
    assert!(matches!(
        cli.command,
        Command::Launch {
            id: Some(ref id),
            size: Some(TerminalSize { cols: 120, rows: 40 }),
            ..
        } if id == "demo"
    ));
}

#[test]
fn launch_command_rejects_invalid_size() {
    assert!(Cli::try_parse_from(["cleat", "launch", "demo", "--size", "120"]).is_err());
    assert!(Cli::try_parse_from(["cleat", "launch", "demo", "--size", "0x40"]).is_err());
}

#[test]
fn launch_command_parses_positional_name() {
    let cli = Cli::try_parse_from(["cleat", "launch", "demo", "--cmd", "bash"]).expect("launch positional parses");
    assert_eq!(cli.command, Command::Launch {
        id: Some("demo".into()),
        from: None,
        json: false,
        size: None,
        vt: None,
        cwd: None,
        cmd: Some("bash".into()),
        tags: Vec::new(),
        record: RecordFlags::default()
    });
}

#[test]
fn launch_command_parses_repeatable_tags() {
    let cli = Cli::try_parse_from(["cleat", "launch", "demo", "--tag", "role=impl", "--tag", "task=99"]).expect("launch --tag parses");
    assert!(matches!(
        cli.command,
        Command::Launch {
            id: Some(ref id),
            tags: ref parsed_tags,
            ..
        } if id == "demo" && parsed_tags == &vec!["role=impl".to_string(), "task=99".to_string()]
    ));
}

#[test]
fn launch_command_parses_json() {
    let cli = Cli::try_parse_from(["cleat", "launch", "--json", "demo"]).expect("launch --json parses");
    assert_eq!(cli.command, Command::Launch {
        id: Some("demo".into()),
        from: None,
        json: true,
        size: None,
        vt: None,
        cwd: None,
        cmd: None,
        tags: Vec::new(),
        record: RecordFlags::default()
    });
}

#[test]
fn launch_command_parses_vt() {
    let cli = Cli::try_parse_from(["cleat", "launch", "--vt", "ghostty", "demo"]).expect("launch --vt parses");
    assert_eq!(cli.command, Command::Launch {
        id: Some("demo".into()),
        from: None,
        json: false,
        size: None,
        vt: Some(VtEngineKind::Ghostty),
        cwd: None,
        cmd: None,
        tags: Vec::new(),
        record: RecordFlags::default()
    });
}

#[test]
fn create_alias_still_parses_as_launch() {
    let cli = Cli::try_parse_from(["cleat", "create", "--cmd", "bash"]).expect("create alias parses");
    assert_eq!(cli.command, Command::Launch {
        id: None,
        from: None,
        json: false,
        size: None,
        vt: None,
        cwd: None,
        cmd: Some("bash".into()),
        tags: Vec::new(),
        record: RecordFlags::default()
    });
}

#[test]
fn list_command_parses() {
    let cli = Cli::try_parse_from(["cleat", "list"]).expect("list parses");
    assert_eq!(cli.command, Command::List { json: false, watch: false, all: false, selectors: Vec::new() });
}

#[test]
fn list_command_parses_json() {
    let cli = Cli::try_parse_from(["cleat", "list", "--json"]).expect("list --json parses");
    assert_eq!(cli.command, Command::List { json: true, watch: false, all: false, selectors: Vec::new() });
}

#[test]
fn list_command_parses_watch_and_selectors() {
    let cli =
        Cli::try_parse_from(["cleat", "list", "--watch", "--selector", "role=impl", "--selector", "task=99"]).expect("list watch parses");
    assert_eq!(cli.command, Command::List {
        json: false,
        watch: true,
        all: false,
        selectors: vec!["role=impl".to_string(), "task=99".to_string()]
    });
}

#[test]
fn list_command_parses_all() {
    let cli = Cli::try_parse_from(["cleat", "list", "--all"]).expect("list --all parses");
    assert_eq!(cli.command, Command::List { json: false, watch: false, all: true, selectors: Vec::new() });
}

#[test]
fn list_command_rejects_all_with_watch() {
    assert!(Cli::try_parse_from(["cleat", "list", "--all", "--watch"]).is_err());
}

#[test]
fn capture_command_parses() {
    let cli = Cli::try_parse_from(["cleat", "capture", "session-1"]).expect("capture parses");
    assert_eq!(cli.command, Command::Capture { id: "session-1".into() });
}

#[test]
fn detach_command_parses() {
    let cli = Cli::try_parse_from(["cleat", "detach", "session-1"]).expect("detach parses");
    assert_eq!(cli.command, Command::Detach { id: "session-1".into() });
}

#[test]
fn kill_command_parses() {
    let cli = Cli::try_parse_from(["cleat", "kill", "session-1"]).expect("kill parses");
    assert_eq!(cli.command, Command::Kill { id: "session-1".into(), purge: false });
}

#[test]
fn kill_purge_command_parses() {
    let cli = Cli::try_parse_from(["cleat", "kill", "session-1", "--purge"]).expect("kill --purge parses");
    assert_eq!(cli.command, Command::Kill { id: "session-1".into(), purge: true });
}

#[test]
fn send_keys_command_parses() {
    let cli = Cli::try_parse_from(["cleat", "send-keys", "demo", "Enter"]).expect("send-keys parses");
    assert_eq!(cli.command, Command::SendKeys {
        id: "demo".into(),
        literal: false,
        hex: false,
        repeat: 1,
        keys: vec!["Enter".into()],
        mark_before: None
    });
}

#[test]
fn send_keys_command_parses_literal_mode() {
    let cli = Cli::try_parse_from(["cleat", "send-keys", "-l", "demo", "hello", "world"]).expect("send-keys -l parses");
    assert_eq!(cli.command, Command::SendKeys {
        id: "demo".into(),
        literal: true,
        hex: false,
        repeat: 1,
        keys: vec!["hello".into(), "world".into()],
        mark_before: None,
    });
}

#[test]
fn send_keys_command_parses_hex_mode() {
    let cli = Cli::try_parse_from(["cleat", "send-keys", "-H", "demo", "41", "0a"]).expect("send-keys -H parses");
    assert_eq!(cli.command, Command::SendKeys {
        id: "demo".into(),
        literal: false,
        hex: true,
        repeat: 1,
        keys: vec!["41".into(), "0a".into()],
        mark_before: None,
    });
}

#[test]
fn send_keys_command_parses_repeat() {
    let cli = Cli::try_parse_from(["cleat", "send-keys", "-N", "3", "demo", "C-l"]).expect("send-keys -N parses");
    assert_eq!(cli.command, Command::SendKeys {
        id: "demo".into(),
        literal: false,
        hex: false,
        repeat: 3,
        keys: vec!["C-l".into()],
        mark_before: None
    });
}

#[test]
fn send_keys_command_rejects_missing_keys() {
    assert!(Cli::try_parse_from(["cleat", "send-keys", "demo"]).is_err());
}

#[test]
fn send_keys_command_rejects_literal_and_hex_together() {
    assert!(Cli::try_parse_from(["cleat", "send-keys", "-l", "-H", "demo", "Enter"]).is_err());
}

#[test]
fn send_keys_command_rejects_zero_repeat() {
    assert!(Cli::try_parse_from(["cleat", "send-keys", "-N", "0", "demo", "Enter"]).is_err());
}

#[test]
fn inspect_parses_session_id() {
    let cli = Cli::try_parse_from(["cleat", "inspect", "alpha"]).expect("parse inspect");
    assert!(matches!(cli.command, Command::Inspect { ref id, json: false } if id == "alpha"));
}

#[test]
fn inspect_json_flag() {
    let cli = Cli::try_parse_from(["cleat", "inspect", "alpha", "--json"]).expect("parse inspect --json");
    assert!(matches!(cli.command, Command::Inspect { json: true, .. }));
}

#[test]
fn signal_parses_session_and_signal_name() {
    let cli = Cli::try_parse_from(["cleat", "signal", "alpha", "INT"]).expect("parse signal");
    assert!(
        matches!(cli.command, Command::Signal { ref id, ref signal, ref target } if id == "alpha" && signal == "INT" && target == "foreground")
    );
}

#[test]
fn signal_with_target() {
    let cli = Cli::try_parse_from(["cleat", "signal", "alpha", "TERM", "--target", "leader"]).expect("parse signal --target");
    assert!(matches!(cli.command, Command::Signal { ref target, .. } if target == "leader"));
}

#[test]
fn record_parses_session_id() {
    let cli = Cli::try_parse_from(["cleat", "record", "alpha"]).expect("parse record");
    assert!(matches!(cli.command, Command::Record { ref id } if id == "alpha"));
}

#[test]
fn launch_record_flag() {
    let cli = Cli::try_parse_from(["cleat", "launch", "alpha", "--record"]).expect("parse launch --record");
    assert!(matches!(cli.command, Command::Launch { record: RecordFlags { record: true, .. }, .. }));
}

#[test]
fn launch_no_record_flag_parses() {
    let cli = Cli::try_parse_from(["cleat", "launch", "alpha", "--no-record"]).expect("parse launch --no-record");
    assert!(matches!(cli.command, Command::Launch { record: RecordFlags { no_record: true, .. }, .. }));
}

#[test]
fn record_flags_default_to_on() {
    // No flag set: recording is on by default (CLEAT_RECORD unset in normal runs).
    let flags = RecordFlags::default();
    assert!(flags.enabled());
}

#[test]
fn record_flags_no_record_disables() {
    let flags = RecordFlags { record: false, no_record: true };
    assert!(!flags.enabled());
}

#[test]
fn record_flags_explicit_record_enables() {
    let flags = RecordFlags { record: true, no_record: false };
    assert!(flags.enabled());
}

#[test]
fn serve_parses_as_daemon_scoped_command() {
    let cli = Cli::try_parse_from(["cleat", "--server", "alternate", "serve"]).expect("parse serve");
    assert_eq!(cli.server.as_deref(), Some("alternate"));
    assert!(matches!(cli.command, Command::Serve));
}

#[test]
fn mark_command_parses_session_id() {
    let cli = Cli::try_parse_from(["cleat", "mark", "my-session"]).expect("mark parses");
    assert_eq!(cli.command, Command::Mark { id: "my-session".into(), name: None });
}

#[test]
fn send_keys_execute_reports_missing_session() {
    let cli = Cli {
        runtime_root: None,
        server: Some(cleat::runtime::DEFAULT_DAEMON_NAME.to_string()),
        command: Command::SendKeys {
            id: "demo".into(),
            literal: false,
            hex: false,
            repeat: 1,
            keys: vec!["Enter".into()],
            mark_before: None,
        },
    };
    let service = SessionService::new(RuntimeLayout::new(tempfile::tempdir().expect("tempdir").path().to_path_buf()));

    let result = execute(cli, &service);
    let err = match result {
        ExecResult::Err(e) => e,
        _ => panic!("missing session should fail"),
    };
    assert!(err.contains("missing"));
}

#[test]
fn mark_with_name_parses() {
    let cli = Cli::try_parse_from(["cleat", "mark", "sess", "checkpoint"]).expect("parse");
    assert_eq!(cli.command, Command::Mark { id: "sess".into(), name: Some("checkpoint".into()) });
}

#[test]
fn mark_without_name_still_works() {
    let cli = Cli::try_parse_from(["cleat", "mark", "sess"]).expect("parse");
    assert_eq!(cli.command, Command::Mark { id: "sess".into(), name: None });
}

#[test]
fn transcript_with_since_marker_parses() {
    let cli = Cli::try_parse_from(["cleat", "transcript", "sess", "--since-marker", "m1"]).expect("parse");
    assert_eq!(cli.command, Command::Transcript {
        id: "sess".into(),
        since: None,
        since_marker: Some("m1".into()),
        until: None,
        until_marker: None,
        until_next_marker: false,
        until_idle: None,
        raw: false,
    });
}

#[test]
fn transcript_with_since_offset_parses() {
    let cli = Cli::try_parse_from(["cleat", "transcript", "sess", "--since", "500"]).expect("parse");
    assert_eq!(cli.command, Command::Transcript {
        id: "sess".into(),
        since: Some(500),
        since_marker: None,
        until: None,
        until_marker: None,
        until_next_marker: false,
        until_idle: None,
        raw: false,
    });
}

#[test]
fn transcript_with_raw_parses() {
    let cli = Cli::try_parse_from(["cleat", "transcript", "sess", "--since-marker", "m1", "--raw"]).expect("parse");
    assert_eq!(cli.command, Command::Transcript {
        id: "sess".into(),
        since: None,
        since_marker: Some("m1".into()),
        until: None,
        until_marker: None,
        until_next_marker: false,
        until_idle: None,
        raw: true,
    });
}

#[test]
fn transcript_requires_since_or_since_marker() {
    let temp = tempfile::tempdir().unwrap();
    let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));
    let cli = Cli::try_parse_from(["cleat", "transcript", "sess"]).expect("parse");
    let result = execute(cli, &service);
    let err = match result {
        ExecResult::Err(e) => e,
        _ => panic!("transcript without --since should fail"),
    };
    assert!(err.contains("--since or --since-marker"));
}

#[test]
fn transcript_since_and_since_marker_are_mutually_exclusive() {
    let result = Cli::try_parse_from(["cleat", "transcript", "sess", "--since", "100", "--since-marker", "m1"]);
    assert!(result.is_err(), "--since and --since-marker should be mutually exclusive");
}

#[test]
fn transcript_with_until_offset_parses() {
    let cli = Cli::try_parse_from(["cleat", "transcript", "sess", "--since", "0", "--until", "1000"]).expect("parse");
    assert_eq!(cli.command, Command::Transcript {
        id: "sess".into(),
        since: Some(0),
        since_marker: None,
        until: Some(1000),
        until_marker: None,
        until_next_marker: false,
        until_idle: None,
        raw: false,
    });
}

#[test]
fn transcript_with_until_marker_parses() {
    let cli = Cli::try_parse_from(["cleat", "transcript", "sess", "--since-marker", "a", "--until-marker", "b"]).expect("parse");
    assert_eq!(cli.command, Command::Transcript {
        id: "sess".into(),
        since: None,
        since_marker: Some("a".into()),
        until: None,
        until_marker: Some("b".into()),
        until_next_marker: false,
        until_idle: None,
        raw: false,
    });
}

#[test]
fn transcript_with_until_next_marker_parses() {
    let cli = Cli::try_parse_from(["cleat", "transcript", "sess", "--since-marker", "a", "--until-next-marker"]).expect("parse");
    assert_eq!(cli.command, Command::Transcript {
        id: "sess".into(),
        since: None,
        since_marker: Some("a".into()),
        until: None,
        until_marker: None,
        until_next_marker: true,
        until_idle: None,
        raw: false,
    });
}

#[test]
fn transcript_with_until_idle_parses_humantime() {
    let cli = Cli::try_parse_from(["cleat", "transcript", "sess", "--since", "0", "--until-idle", "500ms"]).expect("parse");
    assert_eq!(cli.command, Command::Transcript {
        id: "sess".into(),
        since: Some(0),
        since_marker: None,
        until: None,
        until_marker: None,
        until_next_marker: false,
        until_idle: Some(std::time::Duration::from_millis(500)),
        raw: false,
    });
}

#[test]
fn transcript_end_bounds_are_mutually_exclusive() {
    for args in [
        &["cleat", "transcript", "sess", "--since", "0", "--until", "100", "--until-marker", "m1"][..],
        &["cleat", "transcript", "sess", "--since", "0", "--until", "100", "--until-next-marker"][..],
        &["cleat", "transcript", "sess", "--since", "0", "--until-marker", "m1", "--until-idle", "1s"][..],
        &["cleat", "transcript", "sess", "--since", "0", "--until-next-marker", "--until-idle", "1s"][..],
    ] {
        let result = Cli::try_parse_from(args.iter().copied());
        assert!(result.is_err(), "end bounds should be mutually exclusive: {args:?}");
    }
}

#[test]
fn send_command_parses() {
    let cli = Cli::try_parse_from(["cleat", "send", "demo", "echo hello"]).expect("send parses");
    assert_eq!(cli.command, Command::Send {
        id: "demo".into(),
        text: "echo hello".into(),
        no_enter: false,
        submit: false,
        mark_before: None
    });
}

#[test]
fn send_command_parses_no_enter() {
    let cli = Cli::try_parse_from(["cleat", "send", "--no-enter", "demo", "partial"]).expect("send --no-enter parses");
    assert_eq!(cli.command, Command::Send { id: "demo".into(), text: "partial".into(), no_enter: true, submit: false, mark_before: None });
}

#[test]
fn send_command_parses_submit() {
    let cli = Cli::try_parse_from(["cleat", "send", "--submit", "demo", "prompt"]).expect("send --submit parses");
    assert_eq!(cli.command, Command::Send { id: "demo".into(), text: "prompt".into(), no_enter: false, submit: true, mark_before: None });
}

#[test]
fn send_command_rejects_submit_with_no_enter() {
    assert!(Cli::try_parse_from(["cleat", "send", "--submit", "--no-enter", "demo", "prompt"]).is_err());
}

#[cfg(unix)]
#[test]
fn send_submit_posts_paste_then_enter_input_requests() {
    let temp = tempfile::tempdir().expect("tempdir");
    let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));
    std::fs::create_dir_all(service.session_dir("alpha")).expect("create session dir");

    let socket_path = session_socket_path(temp.path(), "alpha");
    let listener = std::os::unix::net::UnixListener::bind(&socket_path).expect("bind socket");
    let reader = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..2 {
            use std::io::Write;

            let (mut stream, _) = listener.accept().expect("accept connection");
            requests.push(read_http_request_for_cli_test(&mut stream));
            stream.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").expect("write response");
        }
        requests
    });

    let cli = Cli::try_parse_from(["cleat", "send", "--submit", "alpha", "hello"]).expect("parse send --submit");
    assert_eq!(execute(cli, &service).expect("execute send --submit"), None);

    let requests = reader.join().expect("join reader");
    assert_eq!(requests.len(), 2);
    assert!(requests[0].starts_with("POST /sessions/alpha/input HTTP/1.1\r\n"), "{}", requests[0]);
    assert!(requests[0].ends_with(r#"{"kind":"paste","text":"hello"}"#), "{}", requests[0]);
    assert!(requests[1].starts_with("POST /sessions/alpha/input HTTP/1.1\r\n"), "{}", requests[1]);
    assert!(requests[1].ends_with(r#"{"kind":"key","key":{"kind":"named","key":"enter"}}"#), "{}", requests[1]);
}

#[cfg(unix)]
#[test]
fn send_submit_with_mark_marks_before_paste_and_returns_offset() {
    let temp = tempfile::tempdir().expect("tempdir");
    let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));
    std::fs::create_dir_all(service.session_dir("alpha")).expect("create session dir");

    let socket_path = session_socket_path(temp.path(), "alpha");
    let listener = std::os::unix::net::UnixListener::bind(&socket_path).expect("bind socket");
    let reader = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for index in 0..2 {
            use std::io::Write;

            let (mut stream, _) = listener.accept().expect("accept connection");
            requests.push(read_http_request_for_cli_test(&mut stream));
            if index == 0 {
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 13\r\nConnection: close\r\n\r\n{\"offset\":42}")
                    .expect("write mark response");
            } else {
                stream.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").expect("write response");
            }
        }
        requests
    });

    let cli = Cli::try_parse_from(["cleat", "send", "--submit", "--mark-before", "m1", "alpha", "hello"]).expect("parse send --submit");
    assert_eq!(execute(cli, &service).expect("execute send --submit"), Some("42".to_string()));

    let requests = reader.join().expect("join reader");
    assert_eq!(requests.len(), 2);
    assert!(requests[0].starts_with("POST /sessions/alpha/paste-with-mark HTTP/1.1\r\n"), "{}", requests[0]);
    assert!(requests[0].ends_with(r#"{"text":"hello","marker_name":"m1"}"#), "{}", requests[0]);
    assert!(requests[1].starts_with("POST /sessions/alpha/input HTTP/1.1\r\n"), "{}", requests[1]);
    assert!(requests[1].ends_with(r#"{"kind":"key","key":{"kind":"named","key":"enter"}}"#), "{}", requests[1]);
}

#[test]
fn interrupt_command_parses() {
    let cli = Cli::try_parse_from(["cleat", "interrupt", "demo"]).expect("interrupt parses");
    assert_eq!(cli.command, Command::Interrupt { id: "demo".into() });
}

#[test]
fn escape_command_parses() {
    let cli = Cli::try_parse_from(["cleat", "escape", "demo"]).expect("escape parses");
    assert_eq!(cli.command, Command::Escape { id: "demo".into() });
}

#[test]
fn wait_requires_at_least_one_condition() {
    let cli = Cli::try_parse_from(["cleat", "wait", "sess"]).expect("parse succeeds");
    // The validation happens at execute time, not parse time
    // But we can test that it parses and has the right defaults
    assert!(matches!(cli.command, Command::Wait { idle_time: None, text: None, timeout, json: false, .. } if timeout == 30.0));
}

#[test]
fn wait_idle_time_parses() {
    let cli = Cli::try_parse_from(["cleat", "wait", "sess", "--idle-time", "2.0"]).expect("parse");
    assert!(matches!(cli.command, Command::Wait { idle_time: Some(t), text: None, .. } if t == std::time::Duration::from_secs_f64(2.0)));
}

#[test]
fn wait_text_parses() {
    let cli = Cli::try_parse_from(["cleat", "wait", "sess", "--text", "DONE"]).expect("parse");
    assert!(matches!(cli.command, Command::Wait { text: Some(ref t), idle_time: None, .. } if t == "DONE"));
}

#[test]
fn wait_screen_stable_parses() {
    let cli = Cli::try_parse_from(["cleat", "wait", "sess", "--screen-stable", "750ms"]).expect("parse");
    assert!(matches!(
        cli.command,
        Command::Wait { screen_stable: Some(t), idle_time: None, text: None, .. } if t == std::time::Duration::from_millis(750)
    ));
}

#[test]
fn wait_combined_parses() {
    let cli =
        Cli::try_parse_from(["cleat", "wait", "sess", "--idle-time", "1.0", "--text", "ready", "--screen-stable", "2s", "--timeout", "10"])
            .expect("parse");
    assert!(
        matches!(cli.command, Command::Wait { idle_time: Some(_), text: Some(_), screen_stable: Some(_), timeout, .. } if (timeout - 10.0).abs() < f64::EPSILON)
    );
}

#[test]
fn wait_json_flag() {
    let cli = Cli::try_parse_from(["cleat", "wait", "sess", "--idle-time", "1", "--json"]).expect("parse");
    assert!(matches!(cli.command, Command::Wait { json: true, .. }));
}

#[test]
fn wait_execute_rejects_no_conditions() {
    let temp = tempfile::tempdir().unwrap();
    let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));
    let cli = Cli::try_parse_from(["cleat", "wait", "sess"]).expect("parse");
    let result = execute(cli, &service);
    match result {
        ExecResult::Exit { code: 2, message: Some(msg), .. } => {
            assert!(msg.contains("at least one of --idle-time, --text, or --screen-stable"));
        }
        other => panic!("wait without conditions should exit 2, got: {other:?}"),
    }
}

#[test]
fn wait_execute_rejects_screen_stable_above_maximum() {
    let temp = tempfile::tempdir().unwrap();
    let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));
    let cli = Cli::try_parse_from(["cleat", "wait", "sess", "--screen-stable", "86401s"]).expect("parse");
    let result = execute(cli, &service);
    match result {
        ExecResult::Exit { code: 2, message: Some(msg), .. } => {
            assert!(msg.contains("invalid screen-stable"));
            assert!(msg.contains("max 86400"));
        }
        other => panic!("screen-stable above maximum should exit 2, got: {other:?}"),
    }
}

#[test]
fn wait_idle_time_accepts_humantime_and_seconds() {
    // Both forms parse to the same Duration.
    let humantime_form = Cli::try_parse_from(["cleat", "wait", "x", "--idle-time", "500ms"]).expect("humantime parse");
    let seconds_form = Cli::try_parse_from(["cleat", "wait", "x", "--idle-time", "0.5"]).expect("seconds parse");

    match (&humantime_form.command, &seconds_form.command) {
        (Command::Wait { idle_time: Some(a), .. }, Command::Wait { idle_time: Some(b), .. }) => {
            assert_eq!(*a, std::time::Duration::from_millis(500));
            assert_eq!(*b, std::time::Duration::from_millis(500));
        }
        _ => panic!("expected both forms to parse as Wait with idle_time set"),
    }
}

#[test]
fn expect_with_since_marker_parses() {
    let cli = Cli::try_parse_from(["cleat", "expect", "sess", "--text", "PASS", "--since-marker", "m1", "--timeout", "10"]).expect("parse");
    assert_eq!(cli.command, Command::Expect {
        id: "sess".into(),
        text: "PASS".into(),
        since: None,
        since_marker: Some("m1".into()),
        timeout: 10.0,
        json: false,
    });
}

#[test]
fn expect_with_since_offset_parses() {
    let cli = Cli::try_parse_from(["cleat", "expect", "sess", "--text", "DONE", "--since", "100"]).expect("parse");
    assert_eq!(cli.command, Command::Expect {
        id: "sess".into(),
        text: "DONE".into(),
        since: Some(100),
        since_marker: None,
        timeout: 30.0,
        json: false
    });
}

#[test]
fn expect_requires_since_or_since_marker() {
    let temp = tempfile::tempdir().unwrap();
    let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));
    let cli = Cli::try_parse_from(["cleat", "expect", "sess", "--text", "PASS"]).expect("parse");
    let result = execute(cli, &service);
    match result {
        ExecResult::Exit { code: 2, message: Some(msg), .. } => {
            assert!(msg.contains("--since or --since-marker"));
        }
        other => panic!("expect without checkpoint should exit 2, got: {other:?}"),
    }
}

#[test]
fn expect_json_flag_parses() {
    let cli = Cli::try_parse_from(["cleat", "expect", "sess", "--text", "OK", "--since-marker", "m1", "--json"]).expect("parse");
    assert!(matches!(cli.command, Command::Expect { json: true, .. }));
}

#[test]
fn send_mark_before_parses() {
    let cli = Cli::try_parse_from(["cleat", "send", "--mark-before", "m1", "sess", "echo hi"]).expect("parse");
    assert_eq!(cli.command, Command::Send {
        id: "sess".into(),
        text: "echo hi".into(),
        no_enter: false,
        submit: false,
        mark_before: Some("m1".into())
    });
}

#[test]
fn send_keys_mark_before_parses() {
    let cli = Cli::try_parse_from(["cleat", "send-keys", "--mark-before", "m1", "sess", "Enter"]).expect("parse");
    assert_eq!(cli.command, Command::SendKeys {
        id: "sess".into(),
        literal: false,
        hex: false,
        repeat: 1,
        keys: vec!["Enter".into()],
        mark_before: Some("m1".into()),
    });
}

#[test]
fn replay_with_positional_path_parses() {
    let cli = Cli::try_parse_from(["cleat", "replay", "/tmp/demo.cast"]).expect("parse");
    match cli.command {
        Command::Replay { path, session, since, speed, max_idle, .. } => {
            assert_eq!(path.as_deref().and_then(std::path::Path::to_str), Some("/tmp/demo.cast"));
            assert_eq!(session, None);
            assert_eq!(since, None);
            assert_eq!(speed, 1.0);
            assert_eq!(max_idle, None);
        }
        other => panic!("expected Replay, got {other:?}"),
    }
}

#[test]
fn replay_with_session_parses() {
    let cli = Cli::try_parse_from(["cleat", "replay", "--session", "alpha"]).expect("parse");
    match cli.command {
        Command::Replay { path, session, .. } => {
            assert_eq!(path, None);
            assert_eq!(session.as_deref(), Some("alpha"));
        }
        other => panic!("expected Replay, got {other:?}"),
    }
}

#[test]
fn replay_path_and_session_are_mutually_exclusive() {
    let result = Cli::try_parse_from(["cleat", "replay", "/tmp/x.cast", "--session", "alpha"]);
    assert!(result.is_err(), "path and --session should be mutually exclusive");
}

#[test]
fn replay_requires_path_or_session() {
    let result = Cli::try_parse_from(["cleat", "replay"]);
    assert!(result.is_err(), "replay with no path or --session should error");
}

#[test]
fn replay_since_marker_requires_session() {
    let result = Cli::try_parse_from(["cleat", "replay", "/tmp/x.cast", "--since-marker", "a"]);
    assert!(result.is_err(), "--since-marker without --session should error");
}

#[test]
fn replay_speed_validates() {
    let bad_speeds = ["0", "-1", "NaN", "inf"];
    for s in bad_speeds {
        let result = Cli::try_parse_from(["cleat", "replay", "/tmp/x.cast", "--speed", s]);
        assert!(result.is_err(), "--speed {s} should be rejected");
    }
}

#[test]
fn replay_humantime_max_idle_parses() {
    let cli = Cli::try_parse_from(["cleat", "replay", "/tmp/x.cast", "--max-idle", "500ms"]).expect("parse");
    match cli.command {
        Command::Replay { max_idle, .. } => {
            assert_eq!(max_idle, Some(std::time::Duration::from_millis(500)));
        }
        other => panic!("expected Replay, got {other:?}"),
    }
}

#[cfg(unix)]
fn read_http_request_for_cli_test(stream: &mut impl std::io::Read) -> String {
    let mut bytes = Vec::new();
    loop {
        let mut buf = [0; 1024];
        let n = stream.read(&mut buf).expect("read request");
        assert_ne!(n, 0, "connection closed before request completed");
        bytes.extend_from_slice(&buf[..n]);
        if http_request_complete_for_cli_test(&bytes) {
            return String::from_utf8(bytes).expect("request utf8");
        }
    }
}

#[cfg(unix)]
fn http_request_complete_for_cli_test(bytes: &[u8]) -> bool {
    let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
        return false;
    };
    let header = String::from_utf8_lossy(&bytes[..header_end + 4]);
    let content_length = header
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().expect("content length"))
        })
        .unwrap_or(0);
    bytes.len() >= header_end + 4 + content_length
}
