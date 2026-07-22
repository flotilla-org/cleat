use std::{path::PathBuf, time::Duration};

use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};

use crate::{
    http_uds,
    keys::encode_send_keys,
    protocol::{WaitCondition, WaitStatus},
    runtime::{TerminalSize, DEFAULT_DAEMON_NAME},
    server::{EndBound, FallbackReason, SessionService, StartBound},
    vt::VtEngineKind,
};

#[derive(Debug, Parser)]
#[command(
    name = "cleat",
    version,
    about = "Session daemon with a structured control plane for agents and terminal persistence",
    after_help = "Typical agent workflow:\n\
                  \x20 cleat launch --record my-session --cmd bash\n\
                  \x20 cleat send my-session 'make test' --mark-before m1\n\
                  \x20 cleat wait my-session --idle-time 2\n\
                  \x20 cleat transcript my-session --since-marker m1\n\
                  \x20 cleat kill my-session",
    // Mirrors after_help; BUILD_SUPPORT_MESSAGE appended at runtime via command().
    after_long_help = "Typical agent workflow:\n\
                       \x20 cleat launch --record my-session --cmd bash\n\
                       \x20 cleat send my-session 'make test' --mark-before m1\n\
                       \x20 cleat wait my-session --idle-time 2\n\
                       \x20 cleat transcript my-session --since-marker m1\n\
                       \x20 cleat kill my-session"
)]
pub struct Cli {
    #[arg(long, hide = true)]
    pub runtime_root: Option<PathBuf>,

    #[arg(long, global = true, default_value = DEFAULT_DAEMON_NAME, value_parser = parse_runtime_name)]
    pub server: String,

    #[command(subcommand)]
    pub command: Command,
}

// Bracketed paste marks the paste/submit boundary explicitly; the delay only
// gives the TUI a short turn to consume the completed paste before Enter.
const SUBMIT_ENTER_DELAY: Duration = Duration::from_millis(100);

/// Recording flags shared by the session-creating commands. Recording is on by
/// default; `--no-record` opts out, and `CLEAT_RECORD` sets a boolish baseline
/// (`CLEAT_RECORD=0` disables) that an explicit flag overrides.
#[derive(Debug, Clone, Default, PartialEq, Args)]
pub struct RecordFlags {
    /// Record output to an asciicast file (default)
    #[arg(long, overrides_with = "no_record")]
    pub record: bool,
    /// Disable output recording
    #[arg(long = "no-record", overrides_with = "record")]
    pub no_record: bool,
}

impl RecordFlags {
    /// Resolve the effective recording setting. Default is on; `CLEAT_RECORD`
    /// provides a boolish opt-out baseline that an explicit flag overrides.
    pub fn enabled(&self) -> bool {
        if self.record {
            true
        } else if self.no_record {
            false
        } else {
            record_default_from_env()
        }
    }
}

fn record_default_from_env() -> bool {
    match std::env::var("CLEAT_RECORD") {
        Ok(value) => parse_boolish(&value).unwrap_or(true),
        Err(_) => true,
    }
}

fn parse_boolish(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[derive(Debug, Subcommand, PartialEq)]
pub enum Command {
    /// Attach to a session interactively
    #[command(after_long_help = "By default, creates a new session if the ID does not exist (equivalent\n\
                           to launch + attach). Use --no-create to fail if the session is missing.\n\
                           \n\
                           Unlike launch, attach enters interactive foreground mode — your terminal\n\
                           is connected to the session's PTY until you detach.\n\
                           \n\
                           To detach, run 'cleat detach <ID>' from another terminal.")]
    Attach {
        #[arg(value_name = "ID")]
        id: Option<String>,
        #[arg(long, help = "Fail if the session does not exist")]
        no_create: bool,
        #[arg(long, value_enum, help = crate::vt::VT_ENGINE_HELP)]
        vt: Option<VtEngineKind>,
        #[arg(long, help = "Working directory for the session")]
        cwd: Option<PathBuf>,
        #[arg(long, help = "Command to run (default: user's shell)")]
        cmd: Option<String>,
        #[command(flatten)]
        record: RecordFlags,
    },
    /// Watch a session read-only
    #[command(after_long_help = "Attaches as a read-only watcher. The session keeps its existing\n\
                           controller, if any; watcher input and resize events are ignored.")]
    Watch {
        #[arg(value_name = "ID")]
        id: String,
    },
    /// Subscribe to packet render updates and print debug summaries
    Packets {
        #[arg(value_name = "ID")]
        id: String,
        #[arg(long, default_value_t = 1, value_parser = parse_positive_usize, help = "Number of render packets to print")]
        count: usize,
    },
    /// Create a new session
    #[command(
        alias = "create",
        after_long_help = "Tip: launch a shell (e.g. zsh) and use `send` to run commands.\nSessions exit when the launched process exits."
    )]
    Launch {
        #[arg(value_name = "ID", conflicts_with = "from")]
        id: Option<String>,
        /// Spawn the new session from the selected source session's daemon context
        #[arg(long, value_name = "SESSION", conflicts_with_all = ["id", "size", "vt", "cwd", "tags"])]
        from: Option<String>,
        /// Name the sibling session and its new daemon
        #[arg(long, value_name = "NAME", requires = "from")]
        name: Option<String>,
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(long, value_name = "COLSxROWS", value_parser = parse_terminal_size, help = "Initial terminal size, e.g. 120x40")]
        size: Option<TerminalSize>,
        #[arg(long, value_enum, help = crate::vt::VT_ENGINE_HELP)]
        vt: Option<VtEngineKind>,
        #[arg(long, help = "Working directory for the session")]
        cwd: Option<PathBuf>,
        #[arg(long, help = "Command to run (default: user's shell)")]
        cmd: Option<String>,
        #[arg(long = "tag", value_name = "TAG", allow_hyphen_values = true, help = "Attach an opaque tag to the session; repeatable")]
        tags: Vec<String>,
        #[command(flatten)]
        record: RecordFlags,
    },
    /// List all sessions
    List {
        #[arg(long, help = "Output as JSON")]
        json: bool,
        #[arg(long, help = "Watch directory updates after printing the initial snapshot")]
        watch: bool,
        #[arg(long, conflicts_with = "watch", help = "Enumerate sessions from every daemon directory under the runtime root")]
        all: bool,
        #[arg(long = "selector", value_name = "TAG", allow_hyphen_values = true, help = "Require an exact opaque tag match; repeatable")]
        selectors: Vec<String>,
    },
    /// Add or remove opaque session tags
    Tag {
        #[arg(value_name = "ID")]
        id: String,
        #[arg(value_name = "+TAG|-TAG", required = true, num_args = 1.., allow_hyphen_values = true)]
        mutations: Vec<String>,
    },
    /// Capture terminal screen content
    #[command(after_long_help = "Returns the current rendered screen from the VT engine.\n\
                           Requires a functional VT engine (not passthrough).\n\
                           \n\
                           For recorded output since a checkpoint, use the transcript command.")]
    Capture { id: String },
    /// Read recorded output since a checkpoint
    #[command(after_long_help = "Returns recorded PTY output after the given byte offset or named\n\
                           marker. Requires an active recording.\n\
                           \n\
                           Use mark to set a named checkpoint, then transcript --since-marker\n\
                           to read output produced after that point.\n\
                           \n\
                           --raw is accepted but currently produces the same output as non-raw.\n\
                           VT-rendered replay for the non-raw path is planned.\n\
                           \n\
                           When the chosen end bound cannot be reached (e.g. --until-idle with\n\
                           no matching gap, --until-next-marker with no later marker), the slice\n\
                           falls back to end-of-recording and a line of the form\n\
                           `# bounded by EOF (<reason>)` is written to stderr.")]
    Transcript {
        id: String,
        /// Byte offset in .cast file; return output events after this position
        #[arg(long, conflicts_with = "since_marker")]
        since: Option<u64>,
        /// Named marker to use as the start offset
        #[arg(long, conflicts_with = "since")]
        since_marker: Option<String>,
        /// Byte offset in .cast file; slice ends at this position.
        #[arg(long, conflicts_with_all = ["until_marker", "until_next_marker", "until_idle"])]
        until: Option<u64>,
        /// Named marker to use as the end offset.
        #[arg(long, conflicts_with_all = ["until", "until_next_marker", "until_idle"])]
        until_marker: Option<String>,
        /// Slice until the chronologically-next named marker after the start.
        #[arg(long, conflicts_with_all = ["until", "until_marker", "until_idle"])]
        until_next_marker: bool,
        /// Slice until the recording is idle for this duration (e.g., 500ms, 2s).
        #[arg(long, value_parser = crate::duration_parser::parse_humantime_or_seconds, conflicts_with_all = ["until", "until_marker", "until_next_marker"])]
        until_idle: Option<std::time::Duration>,
        /// Return raw event data instead of VT-rendered text
        #[arg(long)]
        raw: bool,
    },
    /// Play back a recorded cast file (or slice) at controlled speed.
    #[command(long_about = "\
Play a cast file to stdout at controlled speed. The positional argument is a \
path to a .cast file; alternatively use --session <id> to replay a running \
session's recording. \n\
\n\
Slice bounds (--since, --since-marker, --until, --until-marker, \
--until-next-marker, --until-idle) match the `transcript` command's \
semantics. Marker-based flags require --session because markers are \
resolved through the live daemon socket. \n\
")]
    Replay {
        /// Path to the .cast file. Mutually exclusive with --session.
        #[arg(conflicts_with = "session", required_unless_present = "session")]
        path: Option<std::path::PathBuf>,
        /// Session ID whose recording should be replayed.
        #[arg(long, conflicts_with = "path", required_unless_present = "path")]
        session: Option<String>,

        /// Byte offset in the cast file; slice starts at this position.
        #[arg(long, conflicts_with = "since_marker")]
        since: Option<u64>,
        /// Named marker to use as the start offset (requires --session).
        #[arg(long, conflicts_with_all = ["since", "path"])]
        since_marker: Option<String>,

        /// Byte offset in the cast file; slice ends at this position.
        #[arg(long, conflicts_with_all = ["until_marker", "until_next_marker", "until_idle"])]
        until: Option<u64>,
        /// Named marker to use as the end offset (requires --session).
        #[arg(long, conflicts_with_all = ["until", "until_next_marker", "until_idle", "path"])]
        until_marker: Option<String>,
        /// Slice until the chronologically-next named marker after the start (requires --session).
        #[arg(long, conflicts_with_all = ["until", "until_marker", "until_idle", "path"])]
        until_next_marker: bool,
        /// Slice until the recording is idle for this duration (e.g., 500ms, 2s).
        #[arg(long, value_parser = crate::duration_parser::parse_humantime_or_seconds, conflicts_with_all = ["until", "until_marker", "until_next_marker"])]
        until_idle: Option<std::time::Duration>,

        /// Gap multiplier; >1 faster, <1 slower (default: 1.0).
        #[arg(long, value_parser = crate::replay::parse_speed, default_value = "1.0")]
        speed: f64,
        /// Clamp any inter-event gap to this maximum after speed scaling.
        #[arg(long, value_parser = crate::duration_parser::parse_humantime_or_seconds)]
        max_idle: Option<std::time::Duration>,
    },
    /// Detach from a session
    Detach { id: String },
    /// Terminate a session
    Kill {
        #[arg(value_name = "ID")]
        id: String,
        #[arg(long, help = "Delete any preserved recording for the session")]
        purge: bool,
    },
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
        #[arg(long, value_name = "NAME", help = "Set a named marker before sending (requires recording)")]
        mark_before: Option<String>,
    },
    /// Show session state and process info
    #[command(after_long_help = "Returns session metadata: state, terminal dimensions, process info\n\
                           (leader PID, foreground PGID), attachment status, and recording info.\n\
                           \n\
                           NOTE: The cwd field reflects the working directory at launch time.\n\
                           It does not track the shell's current directory after cd commands.")]
    Inspect {
        id: String,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    /// Send an OS signal to the session process
    Signal {
        id: String,
        signal: String,
        #[arg(long, default_value = "foreground", help = "Signal target: foreground (default), leader, or tree")]
        target: String,
    },
    /// Enable output recording
    #[command(after_long_help = "Starts recording PTY output to an asciicast v3 .cast file.\n\
                           Recording can also be enabled at launch time with --record.\n\
                           Markers (via the mark command) require an active recording.")]
    Record { id: String },
    /// Set a named marker in the recording
    #[command(after_long_help = "Returns the byte offset in the .cast file. Named markers can be\n\
                           used with transcript --since-marker to get output recorded after\n\
                           that point. Requires an active recording.")]
    Mark {
        id: String,
        /// Optional marker name — stores the current offset with this label
        #[arg(value_name = "NAME")]
        name: Option<String>,
    },
    /// Send text to a session
    #[command(after_long_help = "Sends text as PTY input — the same as typing on a keyboard.\n\
                           By default, Enter is appended (disable with --no-enter).\n\
                           Use --submit for TUI composers: it paste-encodes the\n\
                           text, waits briefly, then sends Enter as a separate key.\n\
                           \n\
                           In interactive shells, the sent text will be echoed back in the\n\
                           terminal output before any command results appear. This means\n\
                           recorded output (transcript, expect) includes both the echoed\n\
                           command and its output.\n\
                           \n\
                           Recommended pattern for capturing command output:\n\
                           \x20 cleat send my-session 'make test' --mark-before m1\n\
                           \x20 cleat expect my-session --since-marker m1 --text 'pattern'\n\
                           \x20 cleat transcript my-session --since-marker m1\n\
                           \n\
                           Manual TUI composer workaround:\n\
                           \x20 cleat send my-session --no-enter '<prompt>'\n\
                           \x20 sleep 2\n\
                           \x20 cleat send-keys my-session Enter")]
    Send {
        id: String,
        #[arg(value_name = "TEXT", help = "Text to send")]
        text: String,
        #[arg(long, help = "Do not append Enter after the text")]
        no_enter: bool,
        #[arg(long, conflicts_with = "no_enter", help = "Paste-encode text, then send Enter as a separate key after a short delay")]
        submit: bool,
        #[arg(long, value_name = "NAME", help = "Set a named marker before sending (requires recording)")]
        mark_before: Option<String>,
    },
    /// Send Ctrl-C to a session
    Interrupt { id: String },
    /// Send Escape to a session
    Escape { id: String },
    /// Wait for a condition before continuing
    #[command(after_long_help = "Conditions (OR semantics — any match wins):\n\
                           \x20 --idle-time N  Wait until no PTY output for N seconds\n\
                           \x20 --text STR     Wait until STR appears on the VT screen\n\
                           \x20 --screen-stable N  Wait until the rendered screen is stable for N (e.g., 500ms, 2s)\n\
                           \n\
                           At least one condition is required.\n\
                           \n\
                           NOTE: --text matches against the current VT screen state. If the\n\
                           text is already visible when wait is called, it returns immediately.\n\
                           For edge-triggered text matching on new output, use the expect\n\
                           command with --since-marker.\n\
                           \n\
                           Exit codes:\n\
                           \x20 0  Condition met (ready)\n\
                           \x20 1  Timeout reached\n\
                           \x20 2  Error or session exited\n\
                           \n\
                           JSON output (--json): {\"status\": \"ready|timeout|session_gone\", \"elapsed_ms\": N}")]
    Wait {
        id: String,
        /// Wait until output settles for this duration (e.g., 500ms, 2s, or plain seconds).
        #[arg(long, value_parser = crate::duration_parser::parse_humantime_or_seconds)]
        idle_time: Option<std::time::Duration>,
        #[arg(long, help = "Wait until this text appears on screen")]
        text: Option<String>,
        /// Wait until the rendered screen is stable for this duration (e.g., 500ms, 2s, or plain seconds).
        #[arg(long, value_parser = crate::duration_parser::parse_humantime_or_seconds)]
        screen_stable: Option<std::time::Duration>,
        #[arg(long, default_value_t = 30.0, help = "Maximum seconds to wait (default: 30)")]
        timeout: f64,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    /// Wait for text in recorded output since a checkpoint
    #[command(after_long_help = "Edge-triggered text wait: blocks until the given text appears in\n\
                           recorded output after the specified checkpoint. Unlike wait --text,\n\
                           which checks the current VT screen, expect only matches NEW output\n\
                           recorded since the marker.\n\
                           \n\
                           Requires an active recording and --since or --since-marker.\n\
                           \n\
                           WARNING: In interactive shells, recorded output includes the echoed\n\
                           command text. If you send 'make test; echo DONE' and then expect\n\
                           --text DONE, it may match the echoed command line before the actual\n\
                           output appears. To avoid false positives, wait for text that does\n\
                           NOT appear in the command you sent, or use wait --idle-time first\n\
                           to let the command complete.\n\
                           \n\
                           Exit codes:\n\
                           \x20 0  Text found\n\
                           \x20 1  Timeout reached\n\
                           \x20 2  Error or session exited\n\
                           \n\
                           JSON output (--json): {\"status\": \"ready|timeout|session_gone\", \"elapsed_ms\": N}")]
    Expect {
        id: String,
        #[arg(long, required = true, help = "Text pattern to search for in recorded output")]
        text: String,
        /// Byte offset in .cast file to start searching from
        #[arg(long, conflicts_with = "since_marker")]
        since: Option<u64>,
        /// Named marker to use as the start offset
        #[arg(long, conflicts_with = "since")]
        since_marker: Option<String>,
        #[arg(long, default_value_t = 30.0, help = "Maximum seconds to wait (default: 30)")]
        timeout: f64,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    #[command(hide = true)]
    Serve {
        #[arg(long, hide = true)]
        bootstrap_fd: Option<i32>,
    },
}

/// Uses `command()` instead of `Cli::parse()` so --help renders the workflow
/// snippet alongside BUILD_SUPPORT_MESSAGE (which can't be concatenated at
/// compile time since it's a const &str, not a literal).
pub fn parse() -> Cli {
    Cli::from_arg_matches(&command().get_matches()).expect("clap arg parsing should not fail after get_matches succeeds")
}

pub fn command() -> clap::Command {
    let cmd = Cli::command();
    let existing = cmd.get_after_long_help().map(|s| s.to_string()).unwrap_or_default();
    let combined = format!("{existing}\n\n{}", crate::vt::BUILD_SUPPORT_MESSAGE);
    cmd.after_long_help(combined)
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
    let service = match service.with_daemon(cli.server.clone()) {
        Ok(service) => service,
        Err(err) => return ExecResult::Err(err),
    };
    let service = &service;
    match cli.command {
        Command::Attach { id, no_create, vt, cwd, cmd, record } => {
            // Windows can provide basic sessions through ConPTY plus the
            // passthrough engine while Ghostty VT support is still optional.
            #[cfg(not(windows))]
            if !no_create && !crate::vt::functional_vt_available() {
                return ExecResult::Err(crate::vt::nonfunctional_build_error());
            }
            // Install signal handlers before the handshake: the daemon
            // considers us attached (and observers may act on it) the moment
            // the grant lands, which can precede the relay starting.
            let signal_handlers = match crate::platform::terminal::AttachSignalHandlers::install() {
                Ok(handlers) => handlers,
                Err(e) => return ExecResult::Err(e),
            };
            let (attached, guard) = match service.attach(id, vt, cwd, cmd, no_create) {
                Ok(v) => v,
                Err(e) => return ExecResult::Err(e),
            };
            if record.enabled() {
                if let Err(e) = service.record(&attached.id, true) {
                    return ExecResult::Err(e);
                }
            }
            match guard.relay_stdio_with_handlers(signal_handlers) {
                Ok(()) => ExecResult::Ok(None),
                Err(e) => ExecResult::Err(e),
            }
        }
        Command::Watch { id } => {
            let signal_handlers = match crate::platform::terminal::AttachSignalHandlers::install() {
                Ok(handlers) => handlers,
                Err(e) => return ExecResult::Err(e),
            };
            let guard = match service.watch(&id) {
                Ok(v) => v,
                Err(e) => return ExecResult::Err(e),
            };
            match guard.relay_stdio_with_handlers(signal_handlers) {
                Ok(()) => ExecResult::Ok(None),
                Err(e) => ExecResult::Err(e),
            }
        }
        Command::Packets { id, count } => match run_packets_command(service, &id, count) {
            Ok(lines) if lines.is_empty() => ExecResult::Ok(None),
            Ok(lines) => ExecResult::Ok(Some(lines.join("\n"))),
            Err(err) => ExecResult::Err(err),
        },
        Command::Launch { id, from, name, json, size, vt, cwd, cmd, tags, record } => {
            // Windows can provide basic sessions through ConPTY plus the
            // passthrough engine while Ghostty VT support is still optional.
            #[cfg(not(windows))]
            if from.is_none() && !crate::vt::functional_vt_available() {
                return ExecResult::Err(crate::vt::nonfunctional_build_error());
            }
            if let Some(source) = from {
                let created = match service.create_sibling(&source, name, cmd, record.enabled()) {
                    Ok(created) => created,
                    Err(err) => return ExecResult::Err(err),
                };
                return if json {
                    match serde_json::to_string(&created) {
                        Ok(output) => ExecResult::Ok(Some(output)),
                        Err(err) => ExecResult::Err(format!("serialize sibling create result: {err}")),
                    }
                } else {
                    ExecResult::Ok(Some(created.session.id))
                };
            }
            let tags = match normalize_cli_tags(tags) {
                Ok(tags) => tags,
                Err(err) => return ExecResult::Err(err),
            };
            let created = match service.create_with_options(id, vt, cwd, cmd, crate::session::SessionStartOptions {
                record: record.enabled(),
                initial_size: size.unwrap_or_default(),
                colors: crate::vt::TerminalColors::default(),
                tags,
            }) {
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
        Command::List { json, watch, all, selectors } => {
            let selectors = match normalize_cli_tags(selectors) {
                Ok(selectors) => selectors,
                Err(err) => return ExecResult::Err(err),
            };
            if watch {
                return match run_list_watch_command(service, &selectors, json) {
                    Ok(()) => ExecResult::Ok(None),
                    Err(err) => ExecResult::Err(err),
                };
            }
            let sessions = match if all { service.list_all_with_selectors(&selectors) } else { service.list_with_selectors(&selectors) } {
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
        Command::Tag { id, mutations } => {
            let (add, remove) = match parse_tag_mutations(mutations) {
                Ok(value) => value,
                Err(err) => return ExecResult::Err(err),
            };
            match service.update_tags(&id, add, remove) {
                Ok(tags) if tags.is_empty() => ExecResult::Ok(None),
                Ok(tags) => ExecResult::Ok(Some(tags.join(" "))),
                Err(e) => ExecResult::Err(e),
            }
        }
        Command::Capture { id } => match service.capture(&id) {
            Ok(s) => ExecResult::Ok(Some(s)),
            Err(e) => ExecResult::Err(e),
        },
        Command::Transcript { id, since, since_marker, until, until_marker, until_next_marker, until_idle, raw } => {
            let start = match (since, since_marker) {
                (Some(o), None) => StartBound::Offset(o),
                (None, Some(name)) => StartBound::Marker(name),
                (None, None) => {
                    return ExecResult::Err("transcript requires --since or --since-marker".to_string());
                }
                _ => unreachable!("clap conflicts_with prevents this"),
            };

            let end = match (until, until_marker, until_next_marker, until_idle) {
                (Some(o), None, false, None) => EndBound::Offset(o),
                (None, Some(name), false, None) => EndBound::Marker(name),
                (None, None, true, None) => EndBound::NextMarker,
                (None, None, false, Some(d)) => EndBound::IdleGap(d),
                (None, None, false, None) => EndBound::EndOfRecording,
                _ => unreachable!("clap conflicts_with prevents this"),
            };

            let result = if raw { service.capture_slice_raw(&id, start, end) } else { service.capture_slice_text(&id, start, end) };
            match result {
                Ok((s, outcome)) => {
                    if let Some(reason) = &outcome.end_status {
                        let reason_str = match reason {
                            FallbackReason::NoMarkerAfterStart => "no marker after start".to_string(),
                            FallbackReason::NoIdleGap(d) => format!("no {} idle found", humantime::format_duration(*d)),
                        };
                        eprintln!("# bounded by EOF ({reason_str})");
                    }
                    ExecResult::Ok(Some(s))
                }
                Err(e) => ExecResult::Err(e),
            }
        }
        Command::Replay { path, session, since, since_marker, until, until_marker, until_next_marker, until_idle, speed, max_idle } => {
            let start = match (since, since_marker) {
                (Some(o), None) => crate::server::StartBound::Offset(o),
                (None, Some(name)) => crate::server::StartBound::Marker(name),
                (None, None) => crate::server::StartBound::Offset(0),
                _ => unreachable!("clap conflicts_with prevents this"),
            };

            let end = match (until, until_marker, until_next_marker, until_idle) {
                (Some(o), None, false, None) => crate::server::EndBound::Offset(o),
                (None, Some(name), false, None) => crate::server::EndBound::Marker(name),
                (None, None, true, None) => crate::server::EndBound::NextMarker,
                (None, None, false, Some(d)) => crate::server::EndBound::IdleGap(d),
                (None, None, false, None) => crate::server::EndBound::EndOfRecording,
                _ => unreachable!("clap conflicts_with prevents this"),
            };

            let (cast_path, start_offset, end_offset, end_status) = match (path, session) {
                (Some(p), None) => {
                    if !p.exists() {
                        return ExecResult::Err(format!("replay: no such file: {}", p.display()));
                    }
                    match crate::server::resolve_range_for_path(&p, start, end) {
                        Ok((so, eo, status)) => (p, so, eo, status),
                        Err(e) => return ExecResult::Err(e),
                    }
                }
                (None, Some(id)) => {
                    let cast_path = service.session_dir(&id).join(crate::recording::CAST_FILE_NAME);
                    if !cast_path.exists() {
                        return ExecResult::Err(format!("replay: no recording for session {id}"));
                    }
                    match service.resolve_slice_range(&id, start, end, &cast_path) {
                        Ok((so, eo, status)) => (cast_path, so, eo, status),
                        Err(e) => return ExecResult::Err(e),
                    }
                }
                _ => unreachable!("clap enforces exactly one of path or --session"),
            };

            if let Some(reason) = &end_status {
                let reason_str = match reason {
                    crate::server::FallbackReason::NoMarkerAfterStart => "no marker after start".to_string(),
                    crate::server::FallbackReason::NoIdleGap(d) => {
                        format!("no {} idle found", humantime::format_duration(*d))
                    }
                };
                eprintln!("# bounded by EOF ({reason_str})");
            }

            let opts = crate::replay::ReplayOptions { speed, max_idle };
            let mut stdout = std::io::stdout().lock();
            match crate::replay::run_replay(&cast_path, start_offset, end_offset, &opts, &mut stdout, std::thread::sleep) {
                Ok(()) => ExecResult::Ok(None),
                Err(e) => ExecResult::Err(e),
            }
        }
        Command::Detach { id } => match service.detach(&id) {
            Ok(()) => ExecResult::Ok(None),
            Err(e) => ExecResult::Err(e),
        },
        Command::Kill { id, purge } => match service.kill_with_purge(&id, purge) {
            Ok(()) => ExecResult::Ok(None),
            Err(e) => ExecResult::Err(e),
        },
        Command::SendKeys { id, literal, hex, repeat, keys, mark_before } => {
            let bytes = match encode_send_keys(&keys, literal, hex, repeat) {
                Ok(v) => v,
                Err(e) => return ExecResult::Err(e),
            };
            if let Some(marker_name) = mark_before {
                match service.send_keys_with_mark(&id, &bytes, &marker_name) {
                    Ok(offset) => ExecResult::Ok(Some(offset.to_string())),
                    Err(e) => ExecResult::Err(e),
                }
            } else {
                match service.send_keys(&id, &bytes) {
                    Ok(()) => ExecResult::Ok(None),
                    Err(e) => ExecResult::Err(e),
                }
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
        Command::Send { id, text, no_enter, submit, mark_before } => {
            if submit {
                let marker_offset = if let Some(marker_name) = mark_before {
                    match service.send_paste_with_mark(&id, &text, &marker_name) {
                        Ok(offset) => Some(offset),
                        Err(e) => return ExecResult::Err(e),
                    }
                } else {
                    if let Err(e) = service.send_input(&id, &http_uds::InputRequest::Paste { text }) {
                        return ExecResult::Err(e);
                    }
                    None
                };
                std::thread::sleep(SUBMIT_ENTER_DELAY);
                if let Err(e) = service
                    .send_input(&id, &http_uds::InputRequest::Key { key: http_uds::KeyRequest::Named { key: http_uds::NamedKey::Enter } })
                {
                    return ExecResult::Err(e);
                }

                ExecResult::Ok(marker_offset.map(|offset| offset.to_string()))
            } else {
                let mut bytes = text.into_bytes();
                if !no_enter {
                    bytes.push(b'\r');
                }
                if let Some(marker_name) = mark_before {
                    match service.send_keys_with_mark(&id, &bytes, &marker_name) {
                        Ok(offset) => ExecResult::Ok(Some(offset.to_string())),
                        Err(e) => ExecResult::Err(e),
                    }
                } else {
                    match service.send_keys(&id, &bytes) {
                        Ok(()) => ExecResult::Ok(None),
                        Err(e) => ExecResult::Err(e),
                    }
                }
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
        Command::Wait { id, idle_time, text, screen_stable, timeout, json } => {
            execute_wait(service, id, idle_time, text, screen_stable, timeout, json)
        }
        Command::Expect { id, text, since, since_marker, timeout, json } => {
            execute_expect(service, id, text, since, since_marker, timeout, json)
        }
        Command::Serve { bootstrap_fd } => match service.serve_with_bootstrap(bootstrap_fd) {
            Ok(()) => ExecResult::Ok(None),
            Err(e) => ExecResult::Err(e),
        },
    }
}

fn execute_wait(
    service: &SessionService,
    id: String,
    idle_time: Option<std::time::Duration>,
    text: Option<String>,
    screen_stable: Option<std::time::Duration>,
    timeout: f64,
    json: bool,
) -> ExecResult {
    if idle_time.is_none() && text.is_none() && screen_stable.is_none() {
        return ExecResult::Exit {
            code: 2,
            message: Some("wait requires at least one of --idle-time, --text, or --screen-stable".to_string()),
            output: None,
        };
    }

    if !timeout.is_finite() || !(0.0..=86_400.0).contains(&timeout) {
        return ExecResult::Exit { code: 2, message: Some(format!("invalid timeout: {timeout} (max 86400)")), output: None };
    }

    let mut conditions = Vec::new();
    if let Some(dur) = idle_time {
        let secs = dur.as_secs_f64();
        if !(0.0..=86_400.0).contains(&secs) {
            return ExecResult::Exit { code: 2, message: Some(format!("invalid idle-time: {secs} (max 86400)")), output: None };
        }
        conditions.push(WaitCondition::OutputIdle { quiet_ms: (secs * 1000.0) as u64 });
    }
    if let Some(pattern) = text {
        conditions.push(WaitCondition::TextMatch { text: pattern });
    }
    if let Some(dur) = screen_stable {
        let secs = dur.as_secs_f64();
        if !(0.0..=86_400.0).contains(&secs) {
            return ExecResult::Exit { code: 2, message: Some(format!("invalid screen-stable: {secs} (max 86400)")), output: None };
        }
        conditions.push(WaitCondition::ScreenStable { stable_ms: (secs * 1000.0) as u64 });
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

fn execute_expect(
    service: &SessionService,
    id: String,
    text: String,
    since: Option<u64>,
    since_marker: Option<String>,
    timeout: f64,
    json: bool,
) -> ExecResult {
    let offset = match (since, &since_marker) {
        (Some(o), _) => o,
        (_, Some(name)) => match service.resolve_marker(&id, name) {
            Ok(o) => o,
            Err(e) => return ExecResult::Exit { code: 2, message: Some(e), output: None },
        },
        _ => {
            return ExecResult::Exit { code: 2, message: Some("expect requires --since or --since-marker".to_string()), output: None };
        }
    };

    if !timeout.is_finite() || !(0.0..=86_400.0).contains(&timeout) {
        return ExecResult::Exit { code: 2, message: Some(format!("invalid timeout: {timeout} (max 86400)")), output: None };
    }
    let timeout_ms = (timeout * 1000.0) as u64;

    let (status, elapsed_ms) = match service.expect(&id, &text, offset, timeout_ms) {
        Ok(v) => v,
        Err(e) => return ExecResult::Exit { code: 2, message: Some(e), output: None },
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
                ExecResult::Exit { code: 1, message: Some("expect timed out".to_string()), output: None }
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
    if let Some(ref err) = session.error {
        return format!("{}\t<inspect failed: {}>", session.id, err);
    }
    let mut fields =
        vec![session.id.clone(), format_session_status(&session.status).to_string(), crate::vt::vt_engine_label(session.vt_engine)];
    if let Some(cwd) = &session.cwd {
        fields.push(cwd.display().to_string());
    } else if let Some(cmd) = &session.cmd {
        fields.push(cmd.clone());
    }
    if !session.tags.is_empty() {
        fields.push(format!("tags={}", session.tags.join(",")));
    }
    fields.join("\t")
}

fn format_session_status(status: &crate::protocol::SessionStatus) -> &'static str {
    match status {
        crate::protocol::SessionStatus::Attached => "attached",
        crate::protocol::SessionStatus::Detached => "detached",
    }
}

fn normalize_cli_tags(mut tags: Vec<String>) -> Result<Vec<String>, String> {
    if let Some(tag) = tags.iter().find(|tag| tag.is_empty()) {
        return Err(format!("tag must not be empty: {tag:?}"));
    }
    crate::runtime::normalize_tags(&mut tags);
    Ok(tags)
}

fn parse_tag_mutations(mutations: Vec<String>) -> Result<(Vec<String>, Vec<String>), String> {
    let mut add = Vec::new();
    let mut remove = Vec::new();
    for mutation in mutations {
        let (is_add, tag) = if let Some(tag) = mutation.strip_prefix('+') {
            (true, tag)
        } else if let Some(tag) = mutation.strip_prefix('-') {
            (false, tag)
        } else {
            return Err(format!("tag mutation must start with + or -: {mutation}"));
        };
        if tag.is_empty() {
            return Err("tag mutation must include a tag after + or -".to_string());
        }
        if is_add {
            remove.retain(|existing| existing != tag);
            if !add.iter().any(|existing| existing == tag) {
                add.push(tag.to_string());
            }
        } else {
            add.retain(|existing| existing != tag);
            if !remove.iter().any(|existing| existing == tag) {
                remove.push(tag.to_string());
            }
        }
    }
    crate::runtime::normalize_tags(&mut add);
    crate::runtime::normalize_tags(&mut remove);
    Ok((add, remove))
}

fn run_list_watch_command(service: &SessionService, selectors: &[String], json: bool) -> Result<(), String> {
    use std::io::Write;

    let (mut client, snapshot) = service.connect_directory(selectors)?;
    let mut stdout = std::io::stdout().lock();
    if json {
        writeln!(
            stdout,
            "{}",
            serde_json::to_string(&serde_json::json!({"kind": "snapshot", "sessions": snapshot.sessions}))
                .map_err(|err| format!("serialize directory snapshot: {err}"))?
        )
        .map_err(|err| format!("write directory snapshot: {err}"))?;
    } else {
        writeln!(stdout, "{}", format_directory_snapshot(&snapshot)).map_err(|err| format!("write directory snapshot: {err}"))?;
    }
    stdout.flush().map_err(|err| format!("flush directory snapshot: {err}"))?;

    loop {
        let delta = client.read_directory_delta().map_err(|err| format!("read directory delta: {err}"))?;
        if json {
            writeln!(
                stdout,
                "{}",
                serde_json::to_string(&serde_json::json!({"kind": "delta", "delta": delta}))
                    .map_err(|err| format!("serialize directory delta: {err}"))?
            )
            .map_err(|err| format!("write directory delta: {err}"))?;
        } else {
            writeln!(stdout, "{}", format_directory_delta(&delta)).map_err(|err| format!("write directory delta: {err}"))?;
        }
        stdout.flush().map_err(|err| format!("flush directory delta: {err}"))?;
    }
}

fn format_directory_snapshot(snapshot: &crate::packet::DirectorySnapshot) -> String {
    if snapshot.sessions.is_empty() {
        "snapshot\tempty".to_string()
    } else {
        snapshot.sessions.iter().map(|entry| format!("snapshot\t{}", format_directory_entry(entry))).collect::<Vec<_>>().join("\n")
    }
}

fn format_directory_delta(delta: &crate::packet::DirectoryDelta) -> String {
    let mut lines = Vec::new();
    lines.extend(delta.upserted.iter().map(|entry| format!("upsert\t{}", format_directory_entry(entry))));
    lines.extend(delta.removed_session_ids.iter().map(|id| format!("remove\t{id}")));
    if lines.is_empty() {
        "delta\tempty".to_string()
    } else {
        lines.join("\n")
    }
}

fn format_directory_entry(entry: &crate::packet::DirectoryEntry) -> String {
    let mut fields = vec![
        entry.session_id.clone(),
        entry.state.clone(),
        format!("{}x{}", entry.cols, entry.rows),
        format!("controllers={}", entry.controller_count),
        format!("watchers={}", entry.watcher_count),
        format!("recreatable={}", if entry.recreatable { "yes" } else { "no" }),
    ];
    if !entry.tags.is_empty() {
        fields.push(format!("tags={}", entry.tags.join(",")));
    }
    fields.join("\t")
}

fn run_packets_command(service: &SessionService, id: &str, count: usize) -> Result<Vec<String>, String> {
    let (mut client, directory) = service.connect_packets(id)?;
    if !directory.sessions.iter().any(|entry| entry.session_id == id) {
        return Err(format!("session {id} was not present in packet directory"));
    }
    const DEBUG_CHANNEL: u32 = 1;
    // read-only probe: never steals input/resize authority from a real client
    client.open_channel(DEBUG_CHANNEL, id, crate::packet::ChannelRole::Watcher).map_err(|err| format!("open packet channel: {err}"))?;

    let mut lines = Vec::with_capacity(count);
    let mut previous_modes = None;
    for _ in 0..count {
        let packet = client.read_render(DEBUG_CHANNEL).map_err(|err| format!("read packet render: {err}"))?;
        let update = packet.update;
        lines.push(format_packet_summary(&update, previous_modes));
        previous_modes = Some(update.terminal_modes);
        client.ack(DEBUG_CHANNEL, update.render_generation).map_err(|err| format!("ack packet render: {err}"))?;
    }
    Ok(lines)
}

fn format_packet_summary(update: &crate::provider::TerminalRenderUpdate, previous_modes: Option<crate::vt::TerminalModeState>) -> String {
    let full_replace_ops =
        update.ops.iter().filter(|op| op.kind == crate::provider::TerminalRenderUpdateOpKind::FullVisibleReplace).count();
    let row_replace_ops = update.ops.iter().filter(|op| op.kind == crate::provider::TerminalRenderUpdateOpKind::RowReplace).count();
    let scroll_copy_ops = update.ops.iter().filter(|op| op.kind == crate::provider::TerminalRenderUpdateOpKind::ScrollCopy).count();
    let changed_rows: u16 = update.ops.iter().map(|op| op.row_count).sum();
    format!(
        "gen={} ops={} full={} rows={} scroll={} changed_rows={} mode_changes={} images={}/{}",
        update.render_generation,
        update.ops.len(),
        full_replace_ops,
        row_replace_ops,
        scroll_copy_ops,
        changed_rows,
        format_mode_changes(previous_modes, update.terminal_modes),
        update.image_resources.len(),
        update.image_placements.len()
    )
}

fn format_mode_changes(previous: Option<crate::vt::TerminalModeState>, current: crate::vt::TerminalModeState) -> String {
    let Some(previous) = previous else {
        return "initial".to_string();
    };
    let mut changes = Vec::new();
    if previous.active_alternate_screen != current.active_alternate_screen {
        changes.push(format!("alt_screen={}", current.active_alternate_screen));
    }
    if previous.application_cursor_keys != current.application_cursor_keys {
        changes.push(format!("app_cursor={}", current.application_cursor_keys));
    }
    if previous.alternate_scroll != current.alternate_scroll {
        changes.push(format!("alt_scroll={}", current.alternate_scroll));
    }
    if previous.mouse_tracking != current.mouse_tracking {
        changes.push(format!("mouse_tracking={}", current.mouse_tracking));
    }
    if previous.mouse_tracking_mode != current.mouse_tracking_mode {
        changes.push(format!("mouse_mode={:?}", current.mouse_tracking_mode));
    }
    if previous.mouse_report_format != current.mouse_report_format {
        changes.push(format!("mouse_format={:?}", current.mouse_report_format));
    }
    if previous.mouse_sgr != current.mouse_sgr {
        changes.push(format!("mouse_sgr={}", current.mouse_sgr));
    }
    if previous.mouse_sgr_pixels != current.mouse_sgr_pixels {
        changes.push(format!("mouse_sgr_pixels={}", current.mouse_sgr_pixels));
    }
    if changes.is_empty() {
        "none".to_string()
    } else {
        changes.join(",")
    }
}

fn format_inspect_human(result: &crate::protocol::InspectResult) -> String {
    use comfy_table::{presets::NOTHING, Table};

    let mut table = Table::new();
    table.load_preset(NOTHING);

    table.add_row(vec!["session", &result.session.id]);
    table.add_row(vec!["state", &result.session.state]);
    table.add_row(vec!["vt_engine", &format!("{} ({})", result.session.vt_engine, result.session.vt_engine_status)]);
    table.add_row(vec!["functional_vt", if result.session.functional_vt_available { "yes" } else { "no" }]);
    if !result.session.tags.is_empty() {
        table.add_row(vec!["tags", &result.session.tags.join(", ")]);
    }
    table.add_row(vec!["terminal", &format!("{}x{}", result.terminal.cols, result.terminal.rows)]);
    table.add_row(vec!["leader_pid", &result.process.leader_pid.to_string()]);
    if let Some(fg) = result.process.foreground_pgid {
        table.add_row(vec!["fg_pgid", &fg.to_string()]);
    }
    if let Some(ref cwd) = result.process.leader_cwd {
        table.add_row(vec!["leader_cwd", &cwd.display().to_string()]);
    }
    if let Some(ref cwd) = result.process.foreground_cwd {
        table.add_row(vec!["fg_cwd", &cwd.display().to_string()]);
    }
    if !result.attachments.is_empty() {
        let attachments = result.attachments.iter().map(|attachment| attachment.role.as_str()).collect::<Vec<_>>().join(", ");
        table.add_row(vec!["attachments", &attachments]);
    }
    table.add_row(vec!["recording", if result.recording.active { "active" } else { "off" }]);
    if !result.recording.markers.is_empty() {
        let markers_str = result.recording.markers.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", ");
        table.add_row(vec!["markers", &markers_str]);
    }

    table.to_string()
}

fn parse_signal_name(name: &str) -> Result<i32, String> {
    let normalized = name.to_uppercase();
    let normalized = normalized.trim_start_matches("SIG");
    crate::platform::signals::signal_number(normalized)
}

fn parse_signal_target(target: &str) -> Result<crate::protocol::SignalTarget, String> {
    match target {
        "foreground" => Ok(crate::protocol::SignalTarget::Foreground),
        "leader" => Ok(crate::protocol::SignalTarget::Leader),
        "tree" => Ok(crate::protocol::SignalTarget::Tree),
        other => Err(format!("unknown signal target: {other}")),
    }
}

fn parse_terminal_size(value: &str) -> Result<TerminalSize, String> {
    let (cols, rows) =
        value.split_once('x').or_else(|| value.split_once('X')).ok_or_else(|| "size must be formatted as COLSxROWS".to_string())?;
    let cols = cols.parse::<u16>().map_err(|_| "columns must be a positive integer up to 65535".to_string())?;
    let rows = rows.parse::<u16>().map_err(|_| "rows must be a positive integer up to 65535".to_string())?;
    if cols == 0 || rows == 0 {
        return Err("size dimensions must be greater than zero".to_string());
    }
    Ok(TerminalSize { cols, rows })
}

fn parse_runtime_name(value: &str) -> Result<String, String> {
    crate::runtime::validate_runtime_name(value)?;
    Ok(value.to_string())
}

fn parse_positive_usize(value: &str) -> Result<usize, String> {
    let count = value.parse::<usize>().map_err(|err| err.to_string())?;
    if count == 0 {
        Err("count must be at least 1".to_string())
    } else {
        Ok(count)
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
