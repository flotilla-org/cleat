use std::path::PathBuf;

use clap::{CommandFactory, Parser, Subcommand};

use crate::{
    keys::encode_send_keys,
    protocol::{WaitCondition, WaitStatus},
    runtime::SessionMetadata,
    server::SessionService,
    vt::VtEngineKind,
};

#[derive(Debug, Parser)]
#[command(name = "cleat", version, about = "Session daemon with a structured control plane for agents and terminal persistence")]
pub struct Cli {
    #[arg(long, hide = true)]
    pub runtime_root: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand, PartialEq)]
pub enum Command {
    /// Attach to a session interactively
    Attach {
        #[arg(value_name = "ID")]
        id: Option<String>,
        #[arg(long, help = "Fail if the session does not exist")]
        no_create: bool,
        #[arg(long, value_enum, help = "Virtual terminal engine")]
        vt: Option<VtEngineKind>,
        #[arg(long, help = "Working directory for the session")]
        cwd: Option<PathBuf>,
        #[arg(long, help = "Command to run (default: user's shell)")]
        cmd: Option<String>,
        #[arg(long, env = "CLEAT_RECORD", help = "Enable output recording")]
        record: bool,
    },
    /// Create a new session
    #[command(
        alias = "create",
        after_long_help = "Tip: launch a shell (e.g. zsh) and use `send` to run commands.\nSessions exit when the launched process exits."
    )]
    Launch {
        #[arg(value_name = "ID")]
        id: Option<String>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(long, value_enum, help = "Virtual terminal engine")]
        vt: Option<VtEngineKind>,
        #[arg(long, help = "Working directory for the session")]
        cwd: Option<PathBuf>,
        #[arg(long, help = "Command to run (default: user's shell)")]
        cmd: Option<String>,
        #[arg(long, env = "CLEAT_RECORD", help = "Enable output recording")]
        record: bool,
    },
    /// List all sessions
    List {
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    /// Capture terminal screen content
    Capture {
        id: String,
        /// Byte offset in .cast file; return output events after this position
        #[arg(long, conflicts_with = "since_marker")]
        since: Option<u64>,
        /// Named marker to use as the start offset
        #[arg(long, conflicts_with = "since")]
        since_marker: Option<String>,
        /// Return raw event data instead of VT-rendered text (requires --since or --since-marker)
        #[arg(long)]
        raw: bool,
    },
    /// Detach from a session
    Detach { id: String },
    /// Terminate a session
    Kill { id: String },
    /// Send key sequences using tmux-style names
    #[command(
        after_long_help = "Key names: Enter, Escape (Esc), Tab, BSpace, Space,\n           Up, Down, Left, Right, Home, End,\n           PgUp (PageUp), PgDn (PageDown),\n           IC (Insert), DC (Delete),\n           F1-F12, BTab (Shift-Tab)\n\nModifiers:  C-x (Ctrl), M-x (Meta/Alt), S-x (Shift)\n            ^x  (Ctrl, alternative syntax)\n\nExamples:   cleat send-keys myapp Enter\n            cleat send-keys myapp C-c\n            cleat send-keys myapp -l 'literal text'\n            cleat send-keys myapp -H 1b5b41"
    )]
    SendKeys {
        #[arg(value_name = "ID")]
        id: String,
        #[arg(short = 'l', conflicts_with = "hex", help = "Send keys as literal characters")]
        literal: bool,
        #[arg(short = 'H', conflicts_with = "literal", help = "Send keys as hex-encoded bytes")]
        hex: bool,
        #[arg(short = 'N', default_value_t = 1, value_parser = parse_repeat, help = "Repeat the key sequence N times")]
        repeat: usize,
        #[arg(value_name = "KEY", required = true, num_args = 1..)]
        keys: Vec<String>,
    },
    /// Show session state and process info
    Inspect {
        id: String,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    /// Send an OS signal to the session process
    Signal {
        id: String,
        signal: String,
        #[arg(long, default_value = "foreground", help = "Signal target: foreground (default) or leader")]
        target: String,
    },
    /// Enable output recording
    Record { id: String },
    /// Set a named marker in the recording
    Mark {
        id: String,
        /// Optional marker name — stores the current offset with this label
        #[arg(value_name = "NAME")]
        name: Option<String>,
    },
    /// Send text to a session
    Send {
        id: String,
        #[arg(value_name = "TEXT", help = "Text to send")]
        text: String,
        #[arg(long, help = "Do not append Enter after the text")]
        no_enter: bool,
    },
    /// Send Ctrl-C to a session
    Interrupt { id: String },
    /// Send Escape to a session
    Escape { id: String },
    /// Wait for a condition before continuing
    Wait {
        id: String,
        #[arg(long, help = "Wait until output settles for this many seconds")]
        idle_time: Option<f64>,
        #[arg(long, help = "Wait until this text appears on screen")]
        text: Option<String>,
        #[arg(long, default_value_t = 30.0, help = "Maximum seconds to wait (default: 30)")]
        timeout: f64,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    #[command(hide = true)]
    Serve {
        #[arg(long)]
        id: String,
        #[arg(long, value_enum, default_value_t = crate::vt::default_vt_engine_kind())]
        vt: VtEngineKind,
        #[arg(long)]
        cmd: Option<String>,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long, env = "CLEAT_RECORD")]
        record: bool,
    },
}

pub fn parse() -> Cli {
    Cli::parse()
}

pub fn command() -> clap::Command {
    Cli::command()
}

#[derive(Debug)]
pub enum ExecResult {
    Ok(Option<String>),
    Err(String),
    Exit { code: i32, message: Option<String>, output: Option<String> },
}

impl ExecResult {
    /// Test helper — panics on `Err` or `Exit`. Not intended for production use.
    #[doc(hidden)]
    pub fn expect(self, msg: &str) -> Option<String> {
        match self {
            ExecResult::Ok(v) => v,
            ExecResult::Err(e) => panic!("{msg}: {e}"),
            ExecResult::Exit { code, message, .. } => {
                panic!("{msg}: exit code {code}{}", message.map(|m| format!(": {m}")).unwrap_or_default())
            }
        }
    }

    /// Test helper — panics on `Ok` or `Exit`. Not intended for production use.
    #[doc(hidden)]
    pub fn expect_err(self, msg: &str) -> String {
        match self {
            ExecResult::Err(e) => e,
            ExecResult::Ok(v) => panic!("{msg}: got Ok({v:?})"),
            ExecResult::Exit { code, message, .. } => {
                panic!("{msg}: got Exit {{ code: {code}, message: {message:?} }}")
            }
        }
    }
}

pub fn execute(cli: Cli, service: &SessionService) -> ExecResult {
    match cli.command {
        Command::Attach { id, no_create, vt, cwd, cmd, record } => {
            let (attached, guard) = match service.attach(id, vt, cwd, cmd, no_create) {
                Ok(v) => v,
                Err(e) => return ExecResult::Err(e),
            };
            if record {
                if let Err(e) = service.record(&attached.id, true) {
                    return ExecResult::Err(e);
                }
            }
            match guard.relay_stdio() {
                Ok(()) => ExecResult::Ok(None),
                Err(e) => ExecResult::Err(e),
            }
        }
        Command::Launch { id, json, vt, cwd, cmd, record } => {
            let created = match service.create(id, vt, cwd, cmd, record) {
                Ok(v) => v,
                Err(e) => return ExecResult::Err(e),
            };
            if json {
                match serde_json::to_string(&created) {
                    Ok(s) => ExecResult::Ok(Some(s)),
                    Err(err) => ExecResult::Err(format!("serialize create result: {err}")),
                }
            } else {
                ExecResult::Ok(Some(created.id))
            }
        }
        Command::List { json } => {
            let sessions = match service.list() {
                Ok(v) => v,
                Err(e) => return ExecResult::Err(e),
            };
            if json {
                match serde_json::to_string(&sessions) {
                    Ok(s) => ExecResult::Ok(Some(s)),
                    Err(err) => ExecResult::Err(format!("serialize list result: {err}")),
                }
            } else if sessions.is_empty() {
                ExecResult::Ok(None)
            } else {
                ExecResult::Ok(Some(sessions.iter().map(format_session_human).collect::<Vec<_>>().join("\n")))
            }
        }
        Command::Capture { id, since, since_marker, raw } => {
            if raw && since.is_none() && since_marker.is_none() {
                return ExecResult::Err("--raw requires --since or --since-marker".to_string());
            }
            // --since and --since-marker mutual exclusion enforced by clap conflicts_with
            let offset = match (since, &since_marker) {
                (Some(o), _) => Some(o),
                (_, Some(name)) => match service.resolve_marker(&id, name) {
                    Ok(o) => Some(o),
                    Err(e) => return ExecResult::Err(e),
                },
                _ => None,
            };
            let result = match offset {
                Some(o) => {
                    if raw {
                        service.capture_since_raw(&id, o)
                    } else {
                        service.capture_since_text(&id, o)
                    }
                }
                None => service.capture(&id),
            };
            match result {
                Ok(s) => ExecResult::Ok(Some(s)),
                Err(e) => ExecResult::Err(e),
            }
        }
        Command::Detach { id } => match service.detach(&id) {
            Ok(()) => ExecResult::Ok(None),
            Err(e) => ExecResult::Err(e),
        },
        Command::Kill { id } => match service.kill(&id) {
            Ok(()) => ExecResult::Ok(None),
            Err(e) => ExecResult::Err(e),
        },
        Command::SendKeys { id, literal, hex, repeat, keys } => {
            let bytes = match encode_send_keys(&keys, literal, hex, repeat) {
                Ok(v) => v,
                Err(e) => return ExecResult::Err(e),
            };
            match service.send_keys(&id, &bytes) {
                Ok(()) => ExecResult::Ok(None),
                Err(e) => ExecResult::Err(e),
            }
        }
        Command::Inspect { id, json } => {
            let result = match service.inspect(&id) {
                Ok(v) => v,
                Err(e) => return ExecResult::Err(e),
            };
            if json {
                match serde_json::to_string_pretty(&result) {
                    Ok(s) => ExecResult::Ok(Some(s)),
                    Err(err) => ExecResult::Err(format!("serialize inspect result: {err}")),
                }
            } else {
                ExecResult::Ok(Some(format_inspect_human(&result)))
            }
        }
        Command::Signal { id, signal, target } => {
            let sig = match parse_signal_name(&signal) {
                Ok(v) => v,
                Err(e) => return ExecResult::Err(e),
            };
            let tgt = match parse_signal_target(&target) {
                Ok(v) => v,
                Err(e) => return ExecResult::Err(e),
            };
            match service.signal(&id, sig, tgt) {
                Ok(()) => ExecResult::Ok(None),
                Err(e) => ExecResult::Err(e),
            }
        }
        Command::Record { id } => match service.record(&id, true) {
            Ok(()) => ExecResult::Ok(None),
            Err(e) => ExecResult::Err(e),
        },
        Command::Mark { id, name } => {
            let offset = match name {
                Some(ref n) => service.named_mark(&id, n),
                None => service.mark(&id),
            };
            match offset {
                Ok(v) => ExecResult::Ok(Some(v.to_string())),
                Err(e) => ExecResult::Err(e),
            }
        }
        Command::Send { id, text, no_enter } => {
            let mut bytes = text.into_bytes();
            if !no_enter {
                bytes.push(b'\r');
            }
            match service.send_keys(&id, &bytes) {
                Ok(()) => ExecResult::Ok(None),
                Err(e) => ExecResult::Err(e),
            }
        }
        Command::Interrupt { id } => match service.send_keys(&id, &[0x03]) {
            Ok(()) => ExecResult::Ok(None),
            Err(e) => ExecResult::Err(e),
        },
        Command::Escape { id } => match service.send_keys(&id, &[0x1b]) {
            Ok(()) => ExecResult::Ok(None),
            Err(e) => ExecResult::Err(e),
        },
        Command::Wait { id, idle_time, text, timeout, json } => execute_wait(service, id, idle_time, text, timeout, json),
        Command::Serve { id, vt, cmd, cwd, record } => {
            let session = SessionMetadata { id, vt_engine: vt, cwd, cmd, record };
            match service.serve(&session) {
                Ok(()) => ExecResult::Ok(None),
                Err(e) => ExecResult::Err(e),
            }
        }
    }
}

fn execute_wait(
    service: &SessionService,
    id: String,
    idle_time: Option<f64>,
    text: Option<String>,
    timeout: f64,
    json: bool,
) -> ExecResult {
    if idle_time.is_none() && text.is_none() {
        return ExecResult::Exit {
            code: 2,
            message: Some("wait requires at least one of --idle-time or --text".to_string()),
            output: None,
        };
    }

    if !timeout.is_finite() || !(0.0..=86_400.0).contains(&timeout) {
        return ExecResult::Exit { code: 2, message: Some(format!("invalid timeout: {timeout} (max 86400)")), output: None };
    }

    let mut conditions = Vec::new();
    if let Some(secs) = idle_time {
        if !secs.is_finite() || !(0.0..=86_400.0).contains(&secs) {
            return ExecResult::Exit { code: 2, message: Some(format!("invalid idle-time: {secs} (max 86400)")), output: None };
        }
        conditions.push(WaitCondition::OutputIdle { quiet_ms: (secs * 1000.0) as u64 });
    }
    if let Some(pattern) = text {
        conditions.push(WaitCondition::TextMatch { text: pattern });
    }
    let timeout_ms = (timeout * 1000.0) as u64;

    let (status, elapsed_ms) = match service.wait(&id, conditions, timeout_ms) {
        Ok(v) => v,
        Err(e) => {
            return ExecResult::Exit { code: 2, message: Some(e), output: None };
        }
    };

    match status {
        WaitStatus::Ready => {
            if json {
                ExecResult::Ok(Some(format!(r#"{{"status":"ready","elapsed_ms":{elapsed_ms}}}"#)))
            } else {
                ExecResult::Ok(None)
            }
        }
        WaitStatus::Timeout => {
            if json {
                ExecResult::Exit { code: 1, message: None, output: Some(format!(r#"{{"status":"timeout","elapsed_ms":{elapsed_ms}}}"#)) }
            } else {
                ExecResult::Exit { code: 1, message: Some("wait timed out".to_string()), output: None }
            }
        }
        WaitStatus::SessionGone => {
            if json {
                ExecResult::Exit {
                    code: 2,
                    message: None,
                    output: Some(format!(r#"{{"status":"session_gone","elapsed_ms":{elapsed_ms}}}"#)),
                }
            } else {
                ExecResult::Exit { code: 2, message: Some("session exited while waiting".to_string()), output: None }
            }
        }
    }
}

fn format_session_human(session: &crate::protocol::SessionInfo) -> String {
    let mut fields = vec![session.id.clone(), format_session_status(&session.status).to_string(), session.vt_engine.as_str().to_string()];
    if let Some(cwd) = &session.cwd {
        fields.push(cwd.display().to_string());
    } else if let Some(cmd) = &session.cmd {
        fields.push(cmd.clone());
    }
    fields.join("\t")
}

fn format_session_status(status: &crate::protocol::SessionStatus) -> &'static str {
    match status {
        crate::protocol::SessionStatus::Attached => "attached",
        crate::protocol::SessionStatus::Detached => "detached",
    }
}

fn format_inspect_human(result: &crate::protocol::InspectResult) -> String {
    use comfy_table::{presets::NOTHING, Table};

    let mut table = Table::new();
    table.load_preset(NOTHING);

    table.add_row(vec!["session", &result.session.id]);
    table.add_row(vec!["state", &result.session.state]);
    table.add_row(vec!["terminal", &format!("{}x{}", result.terminal.cols, result.terminal.rows)]);
    table.add_row(vec!["leader_pid", &result.process.leader_pid.to_string()]);
    if let Some(fg) = result.process.foreground_pgid {
        table.add_row(vec!["fg_pgid", &fg.to_string()]);
    }
    table.add_row(vec!["recording", if result.recording.active { "active" } else { "off" }]);
    if !result.recording.markers.is_empty() {
        let markers_str = result.recording.markers.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", ");
        table.add_row(vec!["markers", &markers_str]);
    }

    table.to_string()
}

fn parse_signal_name(name: &str) -> Result<i32, String> {
    use nix::sys::signal::Signal;

    let normalized = name.to_uppercase();
    let normalized = normalized.trim_start_matches("SIG");
    let signal = match normalized {
        "HUP" => Signal::SIGHUP,
        "INT" => Signal::SIGINT,
        "QUIT" => Signal::SIGQUIT,
        "KILL" => Signal::SIGKILL,
        "TERM" => Signal::SIGTERM,
        "STOP" => Signal::SIGSTOP,
        "TSTP" => Signal::SIGTSTP,
        "CONT" => Signal::SIGCONT,
        "USR1" => Signal::SIGUSR1,
        "USR2" => Signal::SIGUSR2,
        other => return Err(format!("unknown signal: {other}")),
    };
    Ok(signal as i32)
}

fn parse_signal_target(target: &str) -> Result<crate::protocol::SignalTarget, String> {
    match target {
        "foreground" => Ok(crate::protocol::SignalTarget::Foreground),
        "leader" => Ok(crate::protocol::SignalTarget::Leader),
        "tree" => Err("tree signal target is not yet implemented".to_string()),
        other => Err(format!("unknown signal target: {other}")),
    }
}

fn parse_repeat(value: &str) -> Result<usize, String> {
    let repeat = value.parse::<usize>().map_err(|err| err.to_string())?;
    if repeat == 0 {
        Err("repeat count must be at least 1".to_string())
    } else {
        Ok(repeat)
    }
}
