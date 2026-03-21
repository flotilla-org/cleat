use std::path::PathBuf;

use clap::{CommandFactory, Parser, Subcommand};

use crate::{keys::encode_send_keys, server::SessionService, vt::VtEngineKind};

#[derive(Debug, Parser)]
#[command(name = "cleat", version)]
pub struct Cli {
    #[arg(long, hide = true)]
    pub runtime_root: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum Command {
    Attach {
        #[arg(value_name = "ID")]
        id: Option<String>,
        #[arg(long)]
        no_create: bool,
        #[arg(long, value_enum)]
        vt: Option<VtEngineKind>,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        cmd: Option<String>,
        #[arg(long)]
        record: bool,
    },
    Create {
        #[arg(value_name = "ID")]
        id: Option<String>,
        #[arg(long)]
        json: bool,
        #[arg(long, value_enum)]
        vt: Option<VtEngineKind>,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        cmd: Option<String>,
        #[arg(long)]
        record: bool,
    },
    List {
        #[arg(long)]
        json: bool,
    },
    Capture {
        id: String,
    },
    Detach {
        id: String,
    },
    Kill {
        id: String,
    },
    SendKeys {
        #[arg(value_name = "ID")]
        id: String,
        #[arg(short = 'l', conflicts_with = "hex")]
        literal: bool,
        #[arg(short = 'H', conflicts_with = "literal")]
        hex: bool,
        #[arg(short = 'N', default_value_t = 1, value_parser = parse_repeat)]
        repeat: usize,
        #[arg(value_name = "KEY", required = true, num_args = 1..)]
        keys: Vec<String>,
    },
    Inspect {
        id: String,
        #[arg(long)]
        json: bool,
    },
    Signal {
        id: String,
        signal: String,
        #[arg(long, default_value = "foreground")]
        target: String,
    },
    Record {
        id: String,
    },
    #[command(hide = true)]
    Serve {
        #[arg(long)]
        id: String,
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
            let (_attached, guard) = service.attach(id, vt, cwd, cmd, no_create)?;
            if record {
                service.record(&_attached.id, true)?;
            }
            guard.relay_stdio()?;
            Ok(None)
        }
        Command::Create { id, json, vt, cwd, cmd, record } => {
            let created = service.create(id, vt, cwd, cmd)?;
            if record {
                let meta_path = service.layout_root().join(&created.id).join("meta.json");
                if let Ok(contents) = std::fs::read_to_string(&meta_path) {
                    if let Ok(mut meta) = serde_json::from_str::<crate::runtime::SessionMetadata>(&contents) {
                        meta.record = true;
                        let _ = std::fs::write(&meta_path, serde_json::to_string_pretty(&meta).expect("serialize"));
                    }
                }
                // The daemon is already running by the time create() returns, so also
                // send a RecordControl frame to activate recording immediately.
                // The daemon may still be initializing, so retry briefly.
                let record_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                loop {
                    match service.record(&created.id, true) {
                        Ok(()) => break,
                        Err(_) if std::time::Instant::now() < record_deadline => {
                            std::thread::sleep(std::time::Duration::from_millis(50));
                        }
                        Err(err) => return Err(format!("activate recording: {err}")),
                    }
                }
            }
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
        Command::Capture { id } => service.capture(&id).map(Some),
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
        Command::Serve { id } => {
            service.serve(&id)?;
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

    table.to_string()
}

fn parse_signal_name(name: &str) -> Result<i32, String> {
    let normalized = name.to_uppercase();
    let normalized = normalized.trim_start_matches("SIG");
    match normalized {
        "INT" => Ok(libc::SIGINT),
        "TERM" => Ok(libc::SIGTERM),
        "HUP" => Ok(libc::SIGHUP),
        "KILL" => Ok(libc::SIGKILL),
        "USR1" => Ok(libc::SIGUSR1),
        "USR2" => Ok(libc::SIGUSR2),
        "STOP" => Ok(libc::SIGSTOP),
        "CONT" => Ok(libc::SIGCONT),
        other => Err(format!("unknown signal: {other}")),
    }
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
