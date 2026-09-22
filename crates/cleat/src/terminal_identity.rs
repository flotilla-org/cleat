//! Child terminal identity belongs to the session engine, not its launcher.
use std::{
    ffi::{OsStr, OsString},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use crate::vt::VtEngineKind;

pub(crate) const IDENTITY_VARIABLES: &[&str] = &["TERM", "TERM_PROGRAM", "TERM_PROGRAM_VERSION", "COLORTERM"];

#[derive(Debug, PartialEq)]
pub(crate) struct TerminalIdentity {
    pub term: &'static str,
    pub program: Option<&'static str>,
    pub color: Option<&'static str>,
}

pub(crate) fn select(engine: VtEngineKind, mut available: impl FnMut(&str) -> bool) -> TerminalIdentity {
    match engine {
        VtEngineKind::Ghostty => TerminalIdentity {
            term: if available("xterm-ghostty") { "xterm-ghostty" } else { "xterm-256color" },
            program: Some("ghostty"),
            color: Some("truecolor"),
        },
        // This engine is a byte relay/test placeholder, not a functional VT.
        VtEngineKind::Passthrough => TerminalIdentity { term: "dumb", program: None, color: None },
    }
}

pub(crate) fn name_eq(a: &OsStr, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(OsStr::new(b))
    } else {
        a == OsStr::new(b)
    }
}

pub(crate) fn is_identity(name: &OsStr) -> bool {
    IDENTITY_VARIABLES.iter().any(|candidate| name_eq(name, candidate))
}

pub(crate) fn defaults(
    engine: VtEngineKind,
    env: &[(OsString, OsString)],
    overrides: &[(String, String)],
    cwd: Option<&std::path::Path>,
) -> Vec<(OsString, OsString)> {
    let has_override = |name| overrides.iter().any(|(key, _)| name_eq(OsStr::new(key), name));
    let identity = select(engine, |term| !has_override("TERM") && terminfo_available(term, env, overrides, cwd));
    [("TERM", Some(identity.term)), ("TERM_PROGRAM", identity.program), ("COLORTERM", identity.color)]
        .into_iter()
        .filter(|(name, _)| !has_override(name))
        .filter_map(|(name, value)| value.map(|v| (OsString::from(name), OsString::from(v))))
        .collect()
}

fn terminfo_available(term: &str, env: &[(OsString, OsString)], overrides: &[(String, String)], cwd: Option<&std::path::Path>) -> bool {
    // Native Windows has no standard terminfo lookup utility. Retain the
    // portable fallback there; an explicit TERM remains available to callers.
    if cfg!(windows) {
        return false;
    }
    let mut command = Command::new("infocmp");
    command
        .arg("-x")
        .arg(term)
        .env_clear()
        .envs(env.iter().cloned())
        .envs(overrides.iter().cloned())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    let deadline = Instant::now() + Duration::from_millis(200);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(2)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn engine_profiles_choose_available_terminfo_without_outer_identity() {
        for available in [true, false] {
            let identity = select(VtEngineKind::Ghostty, |term| {
                assert_eq!(term, "xterm-ghostty");
                available
            });
            assert_eq!(identity.term, if available { "xterm-ghostty" } else { "xterm-256color" });
            assert_eq!(identity.program, Some("ghostty"));
            assert_eq!(identity.color, Some("truecolor"));
        }
        assert_eq!(select(VtEngineKind::Passthrough, |_| panic!("no probe for placeholder")), TerminalIdentity {
            term: "dumb",
            program: None,
            color: None
        });
    }
    #[test]
    fn explicit_overrides_win_and_skip_terminfo_lookup() {
        let defaults =
            defaults(VtEngineKind::Ghostty, &[], &[("TERM".into(), "custom-term".into()), ("TERM_PROGRAM".into(), "custom".into())], None);
        assert_eq!(defaults, vec![("COLORTERM".into(), "truecolor".into())]);
    }
    #[cfg(unix)]
    #[test]
    fn available_profile_resolves_representative_terminfo_capabilities() {
        let env: Vec<_> = std::env::vars_os().collect();
        let identity = select(VtEngineKind::Ghostty, |term| terminfo_available(term, &env, &[], None));
        let Ok(colors) = Command::new("tput").args(["-T", identity.term, "colors"]).output() else {
            return;
        };
        assert!(colors.status.success(), "selected TERM must resolve on this host");
        assert_eq!(String::from_utf8_lossy(&colors.stdout).trim(), "256");
        let cup = Command::new("tput").args(["-T", identity.term, "cup", "2", "3"]).output().unwrap();
        assert!(cup.status.success());
        assert_eq!(cup.stdout, b"\x1b[3;4H");
    }
}
