use clap::{CommandFactory, Parser};
use cleat::{
    cli::{execute, Cli, Command, ExecResult},
    runtime::RuntimeLayout,
    server::SessionService,
    vt::{self, VtEngineKind},
};

#[test]
fn help_lists_expected_subcommands() {
    let command = Cli::command();
    let subcommands: Vec<_> = command.get_subcommands().filter(|sub| !sub.is_hide_set()).map(|sub| sub.get_name().to_string()).collect();
    assert_eq!(subcommands, vec![
        "attach",
        "launch",
        "list",
        "capture",
        "transcript",
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
fn help_surfaces_vt_support_policy() {
    let mut command = Cli::command();
    let mut buffer = Vec::new();
    command.write_long_help(&mut buffer).expect("write help");
    let help = String::from_utf8(buffer).expect("help utf8");

    assert!(help.contains("Ghostty is currently the only functional VT engine"));
    assert!(help.contains(vt::BUILD_SUPPORT_MESSAGE));

    let mut launch = Cli::command().find_subcommand_mut("launch").expect("launch command").clone();
    let mut launch_buffer = Vec::new();
    launch.write_long_help(&mut launch_buffer).expect("write launch help");
    let launch_help = String::from_utf8(launch_buffer).expect("launch help utf8");
    assert!(launch_help.contains("placeholder engines are for testing/development only"));
}

#[test]
fn attach_command_parses() {
    let cli = Cli::try_parse_from(["cleat", "attach", "demo"]).expect("attach positional parses");
    assert_eq!(cli.command, Command::Attach { id: Some("demo".into()), no_create: false, vt: None, cwd: None, cmd: None, record: false });
}

#[test]
fn attach_command_parses_no_create() {
    let cli = Cli::try_parse_from(["cleat", "attach", "--no-create", "demo"]).expect("attach --no-create parses");
    assert_eq!(cli.command, Command::Attach { id: Some("demo".into()), no_create: true, vt: None, cwd: None, cmd: None, record: false });
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
        record: false
    });
}

#[test]
fn launch_command_parses() {
    let cli = Cli::try_parse_from(["cleat", "launch", "--cmd", "bash"]).expect("launch parses");
    assert_eq!(cli.command, Command::Launch { id: None, json: false, vt: None, cwd: None, cmd: Some("bash".into()), record: false });
}

#[test]
fn launch_command_parses_positional_name() {
    let cli = Cli::try_parse_from(["cleat", "launch", "demo", "--cmd", "bash"]).expect("launch positional parses");
    assert_eq!(cli.command, Command::Launch {
        id: Some("demo".into()),
        json: false,
        vt: None,
        cwd: None,
        cmd: Some("bash".into()),
        record: false
    });
}

#[test]
fn launch_command_parses_json() {
    let cli = Cli::try_parse_from(["cleat", "launch", "--json", "demo"]).expect("launch --json parses");
    assert_eq!(cli.command, Command::Launch { id: Some("demo".into()), json: true, vt: None, cwd: None, cmd: None, record: false });
}

#[test]
fn launch_command_parses_vt() {
    let cli = Cli::try_parse_from(["cleat", "launch", "--vt", "ghostty", "demo"]).expect("launch --vt parses");
    assert_eq!(cli.command, Command::Launch {
        id: Some("demo".into()),
        json: false,
        vt: Some(VtEngineKind::Ghostty),
        cwd: None,
        cmd: None,
        record: false
    });
}

#[test]
fn create_alias_still_parses_as_launch() {
    let cli = Cli::try_parse_from(["cleat", "create", "--cmd", "bash"]).expect("create alias parses");
    assert_eq!(cli.command, Command::Launch { id: None, json: false, vt: None, cwd: None, cmd: Some("bash".into()), record: false });
}

#[test]
fn list_command_parses() {
    let cli = Cli::try_parse_from(["cleat", "list"]).expect("list parses");
    assert_eq!(cli.command, Command::List { json: false });
}

#[test]
fn list_command_parses_json() {
    let cli = Cli::try_parse_from(["cleat", "list", "--json"]).expect("list --json parses");
    assert_eq!(cli.command, Command::List { json: true });
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
    assert_eq!(cli.command, Command::Kill { id: "session-1".into() });
}

#[test]
fn send_keys_command_parses() {
    let cli = Cli::try_parse_from(["cleat", "send-keys", "demo", "Enter"]).expect("send-keys parses");
    assert_eq!(cli.command, Command::SendKeys { id: "demo".into(), literal: false, hex: false, repeat: 1, keys: vec!["Enter".into()] });
}

#[test]
fn send_keys_command_parses_literal_mode() {
    let cli = Cli::try_parse_from(["cleat", "send-keys", "-l", "demo", "hello", "world"]).expect("send-keys -l parses");
    assert_eq!(cli.command, Command::SendKeys {
        id: "demo".into(),
        literal: true,
        hex: false,
        repeat: 1,
        keys: vec!["hello".into(), "world".into()]
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
        keys: vec!["41".into(), "0a".into()]
    });
}

#[test]
fn send_keys_command_parses_repeat() {
    let cli = Cli::try_parse_from(["cleat", "send-keys", "-N", "3", "demo", "C-l"]).expect("send-keys -N parses");
    assert_eq!(cli.command, Command::SendKeys { id: "demo".into(), literal: false, hex: false, repeat: 3, keys: vec!["C-l".into()] });
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
    assert!(matches!(cli.command, Command::Launch { record: true, .. }));
}

#[test]
fn serve_parses_all_flags() {
    let cli = Cli::try_parse_from(["cleat", "serve", "--id", "alpha", "--vt", "passthrough", "--cmd", "bash", "--cwd", "/tmp", "--record"])
        .expect("parse serve");
    assert!(matches!(cli.command, Command::Serve { ref id, record: true, .. } if id == "alpha"));
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
        command: Command::SendKeys { id: "demo".into(), literal: false, hex: false, repeat: 1, keys: vec!["Enter".into()] },
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
    assert_eq!(cli.command, Command::Transcript { id: "sess".into(), since: None, since_marker: Some("m1".into()), raw: false });
}

#[test]
fn transcript_with_since_offset_parses() {
    let cli = Cli::try_parse_from(["cleat", "transcript", "sess", "--since", "500"]).expect("parse");
    assert_eq!(cli.command, Command::Transcript { id: "sess".into(), since: Some(500), since_marker: None, raw: false });
}

#[test]
fn transcript_with_raw_parses() {
    let cli = Cli::try_parse_from(["cleat", "transcript", "sess", "--since-marker", "m1", "--raw"]).expect("parse");
    assert_eq!(cli.command, Command::Transcript { id: "sess".into(), since: None, since_marker: Some("m1".into()), raw: true });
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
fn send_command_parses() {
    let cli = Cli::try_parse_from(["cleat", "send", "demo", "echo hello"]).expect("send parses");
    assert_eq!(cli.command, Command::Send { id: "demo".into(), text: "echo hello".into(), no_enter: false });
}

#[test]
fn send_command_parses_no_enter() {
    let cli = Cli::try_parse_from(["cleat", "send", "--no-enter", "demo", "partial"]).expect("send --no-enter parses");
    assert_eq!(cli.command, Command::Send { id: "demo".into(), text: "partial".into(), no_enter: true });
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
    assert!(matches!(cli.command, Command::Wait { idle_time: Some(t), text: None, .. } if (t - 2.0).abs() < f64::EPSILON));
}

#[test]
fn wait_text_parses() {
    let cli = Cli::try_parse_from(["cleat", "wait", "sess", "--text", "DONE"]).expect("parse");
    assert!(matches!(cli.command, Command::Wait { text: Some(ref t), idle_time: None, .. } if t == "DONE"));
}

#[test]
fn wait_combined_parses() {
    let cli = Cli::try_parse_from(["cleat", "wait", "sess", "--idle-time", "1.0", "--text", "ready", "--timeout", "10"]).expect("parse");
    assert!(
        matches!(cli.command, Command::Wait { idle_time: Some(_), text: Some(_), timeout, .. } if (timeout - 10.0).abs() < f64::EPSILON)
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
            assert!(msg.contains("at least one of --idle-time or --text"));
        }
        other => panic!("wait without conditions should exit 2, got: {other:?}"),
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
        json: false,
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
