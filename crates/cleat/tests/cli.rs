use clap::{CommandFactory, Parser};
use cleat::{
    cli::{execute, Cli, Command},
    runtime::RuntimeLayout,
    server::SessionService,
    vt::VtEngineKind,
};

#[test]
fn help_lists_expected_subcommands() {
    let command = Cli::command();
    let subcommands: Vec<_> = command.get_subcommands().filter(|sub| !sub.is_hide_set()).map(|sub| sub.get_name().to_string()).collect();
    assert_eq!(subcommands, vec![
        "attach",
        "create",
        "list",
        "capture",
        "detach",
        "kill",
        "send-keys",
        "inspect",
        "signal",
        "record",
        "mark"
    ]);
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
fn create_command_parses() {
    let cli = Cli::try_parse_from(["cleat", "create", "--cmd", "bash"]).expect("create parses");
    assert_eq!(cli.command, Command::Create { id: None, json: false, vt: None, cwd: None, cmd: Some("bash".into()), record: false });
}

#[test]
fn create_command_parses_positional_name() {
    let cli = Cli::try_parse_from(["cleat", "create", "demo", "--cmd", "bash"]).expect("create positional parses");
    assert_eq!(cli.command, Command::Create {
        id: Some("demo".into()),
        json: false,
        vt: None,
        cwd: None,
        cmd: Some("bash".into()),
        record: false
    });
}

#[test]
fn create_command_parses_json() {
    let cli = Cli::try_parse_from(["cleat", "create", "--json", "demo"]).expect("create --json parses");
    assert_eq!(cli.command, Command::Create { id: Some("demo".into()), json: true, vt: None, cwd: None, cmd: None, record: false });
}

#[test]
fn create_command_parses_vt() {
    let cli = Cli::try_parse_from(["cleat", "create", "--vt", "ghostty", "demo"]).expect("create --vt parses");
    assert_eq!(cli.command, Command::Create {
        id: Some("demo".into()),
        json: false,
        vt: Some(VtEngineKind::Ghostty),
        cwd: None,
        cmd: None,
        record: false
    });
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
    assert_eq!(cli.command, Command::Capture { id: "session-1".into(), since: None, raw: false });
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
fn create_record_flag() {
    let cli = Cli::try_parse_from(["cleat", "create", "alpha", "--record"]).expect("parse create --record");
    assert!(matches!(cli.command, Command::Create { record: true, .. }));
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
    assert_eq!(cli.command, Command::Mark { id: "my-session".into() });
}

#[test]
fn send_keys_execute_reports_missing_session() {
    let cli = Cli {
        runtime_root: None,
        command: Command::SendKeys { id: "demo".into(), literal: false, hex: false, repeat: 1, keys: vec!["Enter".into()] },
    };
    let service = SessionService::new(RuntimeLayout::new(tempfile::tempdir().expect("tempdir").path().to_path_buf()));

    let err = execute(cli, &service).expect_err("missing session should fail");
    assert!(err.contains("missing"));
}

#[test]
fn capture_with_since_flag_parses() {
    let cli = Cli::try_parse_from(["cleat", "capture", "sess", "--since", "12345"]).expect("parse");
    assert_eq!(cli.command, Command::Capture { id: "sess".into(), since: Some(12345), raw: false });
}

#[test]
fn capture_with_raw_flag_parses() {
    let cli = Cli::try_parse_from(["cleat", "capture", "sess", "--since", "0", "--raw"]).expect("parse");
    assert_eq!(cli.command, Command::Capture { id: "sess".into(), since: Some(0), raw: true });
}

#[test]
fn capture_without_since_still_works() {
    let cli = Cli::try_parse_from(["cleat", "capture", "sess"]).expect("parse");
    assert_eq!(cli.command, Command::Capture { id: "sess".into(), since: None, raw: false });
}

#[test]
fn capture_raw_without_since_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));
    let cli = Cli::try_parse_from(["cleat", "capture", "sess", "--raw"]).expect("parse");
    let err = execute(cli, &service).unwrap_err();
    assert!(err.contains("--raw requires --since"));
}
