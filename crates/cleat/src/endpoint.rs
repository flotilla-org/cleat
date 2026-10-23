//! Endpoints: where a session's daemon is and how to reach it.
//!
//! An endpoint is what a session catalog publishes about a daemon (Flotilla's
//! `action.primary.direct.*` facts): a transport kind, the ssh destination for
//! a remote one, the daemon's runtime root and name, and optionally the Cleat
//! the remote host runs. Resolving it here gives every client the same rules
//! for which kinds exist, what each field may contain, and the exact bridge
//! command, instead of each client building its own `ssh … cleat proxy`.
//!
//! A `local` endpoint is reached through the ordinary daemon provider for its
//! runtime root and daemon. An `ssh` endpoint is reached through a command
//! transport running `cleat proxy` on the host. Any other kind is invalid,
//! never guessed at: falling back to a local daemon could attach a different
//! session that happens to share the name.

use std::fmt;

/// How an endpoint's daemon is reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndpointKind {
    /// A daemon on this machine, through its runtime root's socket.
    Local,
    /// A daemon on `host`, through `ssh host cleat proxy`.
    Ssh,
}

/// An endpoint as a catalog publishes it. Empty strings are absent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Endpoint {
    /// `local` or `ssh`.
    pub transport: String,
    /// The ssh destination, which the user's ssh configuration resolves.
    pub host: String,
    /// The daemon's runtime root; empty for the one Cleat discovers.
    pub runtime_root: String,
    /// The daemon's name; empty for the default daemon.
    pub daemon: String,
    /// The Cleat the remote host runs, as one path (a leading `~/` is the
    /// remote home); empty for `cleat` on its non-interactive PATH. It is a
    /// path, not a command line: arguments can't be added through it.
    pub cleat_command: String,
}

/// Why an endpoint can't be reached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndpointError(String);

impl fmt::Display for EndpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EndpointError {}

impl EndpointError {
    pub(crate) fn new(reason: impl Into<String>) -> Self {
        Self(reason.into())
    }
}

impl Endpoint {
    /// The endpoint's kind, once every field it uses is valid.
    ///
    /// No field may contain control characters or start with `-`. The host and
    /// daemon may not contain whitespace either; a runtime root or Cleat path
    /// may, since paths can. An `ssh` endpoint needs a host.
    pub fn check(&self) -> Result<EndpointKind, EndpointError> {
        let kind = match self.transport.as_str() {
            "local" => EndpointKind::Local,
            "ssh" => EndpointKind::Ssh,
            other => return Err(EndpointError(format!("unknown endpoint transport {other:?}"))),
        };
        if kind == EndpointKind::Ssh && self.host.is_empty() {
            return Err(EndpointError("an ssh endpoint needs a host".into()));
        }
        let fields: [(&str, &str, bool); 4] = [
            ("host", &self.host, false),
            ("daemon", &self.daemon, false),
            ("runtime root", &self.runtime_root, true),
            ("cleat command", &self.cleat_command, true),
        ];
        for (name, value, spaces) in fields {
            if value.starts_with('-') {
                return Err(EndpointError(format!("endpoint {name} {value:?} starts with '-'")));
            }
            if value.chars().any(|c| c.is_control() || (!spaces && c.is_whitespace())) {
                return Err(EndpointError(format!("endpoint {name} {value:?} contains a control or whitespace character")));
            }
        }
        Ok(kind)
    }

    /// The command whose stdio carries the packet stream to an `ssh`
    /// endpoint's daemon: `ssh -T -o BatchMode=yes -- <host> <remote command>`.
    /// BatchMode makes ssh fail with a reason instead of prompting, since no
    /// terminal is there to prompt on. `--` keeps the host from being read as
    /// an option. The remote command is one string for the login shell ssh
    /// hands it to (see [`remote_command`](Self::remote_command)).
    pub fn bridge_argv(&self) -> Result<Vec<String>, EndpointError> {
        match self.check()? {
            EndpointKind::Local => Err(EndpointError("a local endpoint needs no bridge".into())),
            EndpointKind::Ssh => {
                Ok(["ssh", "-T", "-o", "BatchMode=yes", "--", &self.host, &self.remote_command()].into_iter().map(String::from).collect())
            }
        }
    }

    /// `cleat proxy [--daemon=<d>] [--runtime-root=<r>]` with each word quoted
    /// for a POSIX login shell. Values follow `=`, so one starting with `-` is
    /// still the flag's value.
    pub fn remote_command(&self) -> String {
        let cleat = if self.cleat_command.is_empty() { "cleat".to_string() } else { command_word(&self.cleat_command) };
        let mut command = format!("{cleat} proxy");
        if !self.daemon.is_empty() {
            command.push_str(&format!(" --daemon={}", shell_word(&self.daemon)));
        }
        if !self.runtime_root.is_empty() {
            command.push_str(&format!(" --runtime-root={}", shell_word(&self.runtime_root)));
        }
        command
    }
}

/// `arg` as one literal word for a POSIX shell: unchanged when it is only
/// characters no shell treats specially, otherwise single-quoted with `'\''`
/// for each quote. `~` and `=` are always quoted: shells differ on expanding a
/// `~` after `=`, and an unquoted `=` could make a first word an assignment.
pub fn shell_word(arg: &str) -> String {
    let plain = !arg.is_empty() && arg.bytes().all(|c| c.is_ascii_alphanumeric() || b"/._-:@+".contains(&c));
    if plain {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', r"'\''"))
}

/// The command word: [`shell_word`], except that a leading `~/` stays outside
/// the quotes, where every POSIX shell expands it to the home directory.
fn command_word(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) if !rest.is_empty() => format!("~/{}", shell_word(rest)),
        _ => shell_word(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ssh(host: &str) -> Endpoint {
        Endpoint { transport: "ssh".into(), host: host.into(), ..Endpoint::default() }
    }

    // The bridge is exactly ssh without prompting, `--` before the host, and
    // one remote command string (the design note's transport seam).
    #[test]
    fn bridge_runs_ssh_to_the_host_with_one_remote_command() {
        let endpoint = Endpoint { daemon: "work".into(), runtime_root: "/var/lib/it's here".into(), ..ssh("crew@fleet-3") };
        assert_eq!(endpoint.bridge_argv().unwrap(), [
            "ssh",
            "-T",
            "-o",
            "BatchMode=yes",
            "--",
            "crew@fleet-3",
            r"cleat proxy --daemon=work --runtime-root='/var/lib/it'\''s here'"
        ]);
    }

    // Defaults add no flags; the remote PATH's cleat runs.
    #[test]
    fn default_daemon_and_root_add_no_flags() {
        assert_eq!(ssh("h").remote_command(), "cleat proxy");
    }

    // A local endpoint is reached through the daemon provider, not a bridge.
    #[test]
    fn local_endpoint_has_no_bridge() {
        let local = Endpoint { transport: "local".into(), ..Endpoint::default() };
        assert_eq!(local.check(), Ok(EndpointKind::Local));
        assert!(local.bridge_argv().is_err());
    }

    // Unknown or empty kinds, and ssh without a host, are refused with a reason.
    #[test]
    fn unknown_kinds_and_missing_hosts_are_refused() {
        for transport in ["", "ssh ", "SSH", "tcp"] {
            let endpoint = Endpoint { transport: transport.into(), host: "h".into(), ..Endpoint::default() };
            assert!(endpoint.check().is_err(), "{transport:?} accepted");
        }
        assert!(ssh("").check().is_err());
    }
}
