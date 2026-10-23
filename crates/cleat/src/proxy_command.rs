//! The far end of a command transport, as one shell command line.
//!
//! A command transport carries the packet stream over a command's stdio, and
//! whatever route the client arranges (ssh, a jump host, an exec into a VM or
//! container, a tunnel an orchestrator set up) ends by running
//! `cleat proxy` next to the daemon. Most such routes hand the far end one
//! string for a POSIX shell. This module builds that string with each word
//! quoted, so a daemon name or runtime root is always one literal argument.
//!
//! It chooses no route. Which hosts exist, how to reach them and with what
//! options is the client's (or its orchestrator's) business; a client passes
//! its own argv, with this command inside it where the route needs one, to
//! `cleat_provider_open_transport`.

use std::fmt;

/// Which daemon `cleat proxy` relays to, and which Cleat runs it. Empty
/// strings are absent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProxyTarget {
    /// The Cleat at the far end, as one path (a leading `~/` is that end's
    /// home); empty for `cleat` on its PATH. A path, not a command line: a
    /// route that needs a wrapper puts it around the whole command.
    pub cleat: String,
    /// The daemon's name; empty for the default daemon.
    pub daemon: String,
    /// The daemon's runtime root; empty for the one Cleat discovers.
    pub runtime_root: String,
}

/// Why a target can't be put on a command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyCommandError(String);

impl fmt::Display for ProxyCommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ProxyCommandError {}

impl ProxyTarget {
    /// `cleat proxy [--daemon=<d>] [--runtime-root=<r>]`, each word quoted
    /// for a POSIX shell. Values follow `=`, so one starting with `-` is still
    /// the flag's value; the far end's Cleat checks the values themselves.
    /// Only a NUL is refused, since no shell string can carry one.
    pub fn shell_command(&self) -> Result<String, ProxyCommandError> {
        for (name, value) in [("cleat", &self.cleat), ("daemon", &self.daemon), ("runtime root", &self.runtime_root)] {
            if value.contains('\0') {
                return Err(ProxyCommandError(format!("proxy {name} contains a NUL")));
            }
        }
        let cleat = if self.cleat.is_empty() { "cleat".to_string() } else { command_word(&self.cleat) };
        let mut command = format!("{cleat} proxy");
        if !self.daemon.is_empty() {
            command.push_str(&format!(" --daemon={}", shell_word(&self.daemon)));
        }
        if !self.runtime_root.is_empty() {
            command.push_str(&format!(" --runtime-root={}", shell_word(&self.runtime_root)));
        }
        Ok(command)
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

    // Defaults add no flags; the far end's PATH supplies cleat.
    #[test]
    fn default_target_adds_no_flags() {
        assert_eq!(ProxyTarget::default().shell_command().unwrap(), "cleat proxy");
    }

    // Plain values stay readable; others are single-quoted.
    #[test]
    fn values_are_quoted_only_when_needed() {
        let target = ProxyTarget { daemon: "work".into(), runtime_root: "/var/lib/it's here".into(), ..ProxyTarget::default() };
        assert_eq!(target.shell_command().unwrap(), r"cleat proxy --daemon=work --runtime-root='/var/lib/it'\''s here'");
    }

    // A NUL can't reach the far end in any shell string, so it is refused.
    #[test]
    fn nul_is_refused() {
        for target in [
            ProxyTarget { cleat: "a\0b".into(), ..ProxyTarget::default() },
            ProxyTarget { daemon: "a\0".into(), ..ProxyTarget::default() },
            ProxyTarget { runtime_root: "\0".into(), ..ProxyTarget::default() },
        ] {
            assert!(target.shell_command().is_err(), "{target:?}");
        }
    }
}
