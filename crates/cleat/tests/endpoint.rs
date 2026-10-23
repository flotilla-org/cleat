#![cfg(unix)]
//! Endpoint resolution (cleat#383): which endpoints are valid, and the bridge
//! command a POSIX login shell on the far end receives. The shell properties
//! run a real `sh` (the same parser ssh hands the remote command to), so the
//! oracle is what the shell actually does, not a copy of the quoting rules.

use std::{os::unix::fs::PermissionsExt, path::Path, process::Command};

use cleat::endpoint::{shell_word, Endpoint, EndpointKind};
use hegel::generators as gs;

/// Characters that cross every quoting case: shell metacharacters, both
/// quotes, the backslash, `~` and `=` (which some shells expand or treat as an
/// assignment), whitespace and control characters, a leading `-`, and
/// multi-byte UTF-8. Plain letters and digits keep the "no quoting needed"
/// path covered.
const ALPHABET: &[char] = &[
    'a', 'Z', '0', '9', '/', '.', '_', '-', ':', '@', '+', ' ', '\t', '\n', '\'', '"', '\\', '$', '`', '~', '=', '*', '?', '[', ']', '{',
    '}', '(', ')', ';', '&', '|', '<', '>', '#', '!', '%', 'é', '日', '\u{1b}',
];

fn draw_text(tc: &hegel::TestCase, allowed: impl Fn(char) -> bool, max: usize) -> String {
    let alphabet: Vec<char> = ALPHABET.iter().copied().filter(|c| allowed(*c)).collect();
    tc.draw(gs::vecs(gs::sampled_from(alphabet)).max_size(max)).into_iter().collect()
}

/// What `sh -c <script>` prints, run with `home` as its home directory.
fn sh(script: &str, home: &Path) -> Vec<u8> {
    let output = Command::new("sh").arg("-c").arg(script).env("HOME", home).output().unwrap();
    assert!(output.status.success(), "sh failed on {script:?}: {}", String::from_utf8_lossy(&output.stderr));
    output.stdout
}

// Behaviour: shell_word makes any text exactly one literal word: the shell
// passes it through unchanged, with no expansion, splitting or assignment.
#[hegel::test(test_cases = 150)]
fn shell_word_is_one_literal_word(tc: hegel::TestCase) {
    // Any text over the alphabet, including empty, `~`-led and `=`-bearing.
    let text = draw_text(&tc, |_| true, 12);
    let home = tempfile::tempdir().unwrap();
    let printed = sh(&format!("printf '%s|' {}", shell_word(&text)), home.path());
    assert_eq!(printed, format!("{text}|").into_bytes(), "word {:?}", shell_word(&text));
}

/// A stand-in Cleat that prints the arguments it received, NUL-terminated.
fn fake_cleat(dir: &Path) -> String {
    let path = dir.join("it's a cleat");
    std::fs::write(&path, "#!/bin/sh\nfor arg; do printf '%s\\0' \"$arg\"; done\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.display().to_string()
}

// Behaviour: the remote command reaches the far end's Cleat as exactly
// `proxy [--daemon=<d>] [--runtime-root=<r>]`, whatever valid daemon and
// runtime root the endpoint carries, so a value can't add words or flags.
#[hegel::test(test_cases = 100)]
fn remote_command_reaches_cleat_as_its_argv(tc: hegel::TestCase) {
    // Valid values only, per Endpoint::check: no control characters, a daemon
    // without whitespace, a root that may hold spaces. Either may be empty. A
    // leading `-` is prevented by prefixing a letter when one is drawn.
    let daemon = draw_text(&tc, |c| !c.is_control() && !c.is_whitespace(), 8);
    let root = draw_text(&tc, |c| !c.is_control(), 10);
    let lead = |s: String| if s.starts_with('-') { format!("x{s}") } else { s };
    let (daemon, root) = (lead(daemon), lead(root));
    let dir = tempfile::tempdir().unwrap();
    let endpoint = Endpoint {
        transport: "ssh".into(),
        host: "h".into(),
        daemon: daemon.clone(),
        runtime_root: root.clone(),
        cleat_command: fake_cleat(dir.path()),
    };
    assert_eq!(endpoint.check(), Ok(EndpointKind::Ssh));

    let printed = sh(&endpoint.remote_command(), dir.path());
    let args: Vec<String> = printed.split(|b| *b == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8(a.to_vec()).unwrap()).collect();
    let mut expected = vec!["proxy".to_string()];
    if !daemon.is_empty() {
        expected.push(format!("--daemon={daemon}"));
    }
    if !root.is_empty() {
        expected.push(format!("--runtime-root={root}"));
    }
    assert_eq!(args, expected, "remote command {:?}", endpoint.remote_command());
}

// Behaviour: a Cleat path under `~/` runs from the remote home, since the
// shell expands the unquoted `~/` even when the rest needs quoting.
#[test]
fn a_home_relative_cleat_runs_from_the_remote_home() {
    let home = tempfile::tempdir().unwrap();
    let bin = home.path().join("bin dir");
    std::fs::create_dir(&bin).unwrap();
    let cleat = fake_cleat(&bin);
    let relative = format!("~/{}", Path::new(&cleat).strip_prefix(home.path()).unwrap().display());
    let endpoint = Endpoint { transport: "ssh".into(), host: "h".into(), cleat_command: relative, ..Endpoint::default() };
    assert_eq!(sh(&endpoint.remote_command(), home.path()), b"proxy\0");
}

// Behaviour (Endpoint::check's contract): an endpoint is valid exactly when
// its transport is `local` or `ssh`, an ssh endpoint has a host, and no field
// holds a control character or starts with `-`, with whitespace also refused
// in the host and daemon. A valid ssh endpoint's bridge puts `--` right before
// the host, so the host can never be read as an ssh option.
#[hegel::test(test_cases = 300)]
fn check_accepts_exactly_the_valid_endpoints(tc: hegel::TestCase) {
    // Transports cover both kinds, near misses (case, trailing space) and the
    // empty string; each field draws from the whole alphabet, so leading `-`,
    // whitespace and control characters all occur.
    let transport = tc.draw(gs::sampled_from(vec!["local", "ssh", "SSH", "ssh ", "", "tcp"]));
    let host = draw_text(&tc, |_| true, 6);
    let daemon = draw_text(&tc, |_| true, 6);
    let runtime_root = draw_text(&tc, |_| true, 6);
    let cleat_command = draw_text(&tc, |_| true, 6);
    let endpoint = Endpoint { transport: transport.into(), host, daemon, runtime_root, cleat_command };

    let clean = |s: &str, spaces: bool| !s.starts_with('-') && !s.chars().any(|c| c.is_control() || (!spaces && c.is_whitespace()));
    let fields_ok = clean(&endpoint.host, false)
        && clean(&endpoint.daemon, false)
        && clean(&endpoint.runtime_root, true)
        && clean(&endpoint.cleat_command, true);
    let expected = match transport {
        "local" if fields_ok => Some(EndpointKind::Local),
        "ssh" if fields_ok && !endpoint.host.is_empty() => Some(EndpointKind::Ssh),
        _ => None,
    };
    assert_eq!(endpoint.check().ok(), expected, "{endpoint:?}");
    if expected == Some(EndpointKind::Ssh) {
        let argv = endpoint.bridge_argv().unwrap();
        assert_eq!(&argv[..6], ["ssh", "-T", "-o", "BatchMode=yes", "--", endpoint.host.as_str()]);
        assert_eq!(argv.len(), 7);
    } else {
        assert!(endpoint.bridge_argv().is_err());
    }
}

// Behaviour: a Cleat command shaped like an assignment (`a=b`) still runs as
// the command, since `=` is always quoted; unquoted, the shell would assign
// `a` and run `proxy` instead.
#[test]
fn an_assignment_shaped_cleat_runs_as_a_command() {
    let dir = tempfile::tempdir().unwrap();
    let cleat = dir.path().join("a=b");
    std::fs::rename(fake_cleat(dir.path()), &cleat).unwrap();
    let endpoint = Endpoint { transport: "ssh".into(), host: "h".into(), cleat_command: "a=b".into(), ..Endpoint::default() };
    let output = Command::new("/bin/sh").arg("-c").arg(endpoint.remote_command()).env("PATH", dir.path()).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, b"proxy\0");
}
