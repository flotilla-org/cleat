use std::path::PathBuf;

use clap::{CommandFactory, Parser, Subcommand};

use crate::{keys::encode_send_keys, runtime::SessionMetadata, server::SessionService, vt::VtEngineKind};

#[derive(Debug, Parser)]
#[command(name = "cleat", version, about = "Session daemon with a structured control plane for agents and terminal persistence")]
pub struct Cli {
    #[arg(long, hide = true)]
    pub runtime_root: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
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
    #[command(alias = "create", after_long_help = "Tip: launch a shell (e.g. zsh) and use `send` to run commands.\nSessions exit when the launched process exits.")]
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
    Detach {
        id: String,
    },
    /// Terminate a session
    Kill {
        id: String,
    },
    /// Send key sequences using tmux-style names
    #[command(after_long_help = "Key names: Enter, Escape (Esc), Tab, BSpace, Space,\n           Up, Down, Left, Right, Home, End,\n           PgUp (PageUp), PgDn (PageDown),\n           IC (Insert), DC (Delete),\n           F1-F12, BTab (Shift-Tab)\n\nModifiers:  C-x (Ctrl), M-x (Meta/Alt), S-x (Shift)\n            ^x  (Ctrl, alternative syntax)\n\nExamples:   cleat send-keys myapp Enter\n            cleat send-keys myapp C-c\n            cleat send-keys myapp -l 'literal text'\n            cleat send-keys myapp -H 1b5b41")]
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
    Record {
        id: String,
    },
    /// Set a named marker in the recording
    Mark {
        id: String,
        /// Optional marker name — stores the current offset with this label
        #[arg(value_name = "NAME")]
        name: Option<String>,
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

pub fn execute(cli: Cli, service: &SessionService) -> Result<Option<String>, String> {
    match cli.command {
        Command::Attach { id, no_create, vt, cwd, cmd, record } => {
            let (attached, guard) = service.attach(id, vt, cwd, cmd, no_create)?;
            if record {
                service.record(&attached.id, true)?;
            }
            guard.relay_stdio()?;
            Ok(None)
        }
        Command::Launch { id, json, vt, cwd, cmd, record } => {
            let created = service.create(id, vt, cwd, cmd, record)?;
            if json {
                serde_json::to_string(&created).map(Some).map_err(|err| format!("serialize create result: {err}"))
            } else {
                Ok(Some(created.id))
            }
        }
        Command::List { json } => {
            let sessions = service.list()?;
            if json {
                serde_json::to_string(&sessions).map(Some).map_err(|err| format!("serialize list result: {err}"))
            } else if sessions.is_empty() {
                Ok(None)
            } else {
                Ok(Some(sessions.iter().map(format_session_human).collect::<Vec<_>>().join("\n")))
            }
        }
        Command::Capture { id, since, since_marker, raw } => {
            if raw && since.is_none() && since_marker.is_none() {
                return Err("--raw requires --since or --since-marker".to_string());
            }
            // --since and --since-marker mutual exclusion enforced by clap conflicts_with
            let offset = match (since, &since_marker) {
                (Some(o), _) => Some(o),
                (_, Some(name)) => Some(service.resolve_marker(&id, name)?),
                _ => None,
            };
            match offset {
                Some(o) => {
                    if raw {
                        service.capture_since_raw(&id, o).map(Some)
                    } else {
                        service.capture_since_text(&id, o).map(Some)
                    }
                }
                None => service.capture(&id).map(Some),
            }
        }
        Command::Detach { id } => {
            service.detach(&id)?;
            Ok(None)
        }
        Command::Kill { id } => {
            service.kill(&id)?;
            Ok(None)
        }
        Command::SendKeys { id, literal, hex, repeat, keys } => {
            let bytes = encode_send_keys(&keys, literal, hex, repeat)?;
            service.send_keys(&id, &bytes)?;
            Ok(None)
        }
        Command::Inspect { id, json } => {
            let result = service.inspect(&id)?;
            if json {
                serde_json::to_string_pretty(&result).map(Some).map_err(|err| format!("serialize inspect result: {err}"))
            } else {
                Ok(Some(format_inspect_human(&result)))
            }
        }
        Command::Signal { id, signal, target } => {
            let sig = parse_signal_name(&signal)?;
            let tgt = parse_signal_target(&target)?;
            service.signal(&id, sig, tgt)?;
            Ok(None)
        }
        Command::Record { id } => {
            service.record(&id, true)?;
            Ok(None)
        }
        Command::Mark { id, name } => {
            let offset = match name {
                Some(ref n) => service.named_mark(&id, n)?,
                None => service.mark(&id)?,
            };
            Ok(Some(offset.to_string()))
        }
        Command::Serve { id, vt, cmd, cwd, record } => {
            let session = SessionMetadata { id, vt_engine: vt, cwd, cmd, record };
            service.serve(&session)?;
            Ok(None)
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
