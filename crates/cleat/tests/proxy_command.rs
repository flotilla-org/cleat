#![cfg(unix)]
//! The proxy command line (cleat#383): what a POSIX shell at the far end of a
//! route receives. The properties run a real `sh` (the same parser ssh or an
//! exec hands the command to), so the oracle is what the shell actually does,
//! not a copy of the quoting rules.

use std::{os::unix::fs::PermissionsExt, path::Path, process::Command};

use cleat::proxy_command::{shell_word, ProxyTarget};
use hegel::generators as gs;

/// Characters that cross every quoting case: shell metacharacters, both
/// quotes, the backslash, `~` and `=` (which some shells expand or treat as an
/// assignment), whitespace and control characters, a leading `-`, and
/// multi-byte UTF-8. Plain letters and digits keep the "no quoting needed"
/// path covered. NUL is left out: no shell string can carry it, and
/// `shell_command` refuses it (unit-tested in the module).
const ALPHABET: &[char] = &[
    'a', 'Z', '0', '9', '/', '.', '_', '-', ':', '@', '+', ' ', '\t', '\n', '\'', '"', '\\', '$', '`', '~', '=', '*', '?', '[', ']', '{',
    '}', '(', ')', ';', '&', '|', '<', '>', '#', '!', '%', 'é', '日', '\u{1b}',
];

fn draw_text(tc: &hegel::TestCase, max: usize) -> String {
    tc.draw(gs::vecs(gs::sampled_from(ALPHABET.to_vec())).max_size(max)).into_iter().collect()
}

/// What `sh -c <script>` prints, run with `home` as its home directory.
fn sh(script: &str, home: &Path) -> Vec<u8> {
    let output = Command::new("/bin/sh").arg("-c").arg(script).env("HOME", home).output().unwrap();
    assert!(output.status.success(), "sh failed on {script:?}: {}", String::from_utf8_lossy(&output.stderr));
    output.stdout
}

// Behaviour: shell_word makes any text exactly one literal word: the shell
// passes it through unchanged, with no expansion, splitting or assignment.
#[hegel::test(test_cases = 150)]
fn shell_word_is_one_literal_word(tc: hegel::TestCase) {
    // Any text over the alphabet, including empty, `~`-led and `=`-bearing.
    let text = draw_text(&tc, 12);
    let home = tempfile::tempdir().unwrap();
    let printed = sh(&format!("printf '%s|' {}", shell_word(&text)), home.path());
    assert_eq!(printed, format!("{text}|").into_bytes(), "word {:?}", shell_word(&text));
}

// Behaviour: a `~` is never expanded, wherever it sits in a word. These are
// the words a generator rarely draws: otherwise plain, so only the `~` decides
// whether they are quoted.
#[test]
fn tilde_is_never_expanded() {
    let home = tempfile::tempdir().unwrap();
    for text in ["~", "~/x", "a=~/b", "x:~"] {
        assert_eq!(sh(&format!("printf '%s' {}", shell_word(text)), home.path()), text.as_bytes(), "word {:?}", shell_word(text));
    }
}

/// A stand-in Cleat that prints the arguments it received, NUL-terminated.
fn fake_cleat(dir: &Path) -> String {
    let path = dir.join("it's a cleat");
    std::fs::write(&path, "#!/bin/sh\nfor arg; do printf '%s\\0' \"$arg\"; done\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.display().to_string()
}

// Behaviour: the command reaches the far end's Cleat as exactly
// `proxy [--daemon=<d>] [--runtime-root=<r>]`, whatever daemon and runtime
// root the target carries, so a value can't add words or flags.
#[hegel::test(test_cases = 100)]
fn command_reaches_cleat_as_its_argv(tc: hegel::TestCase) {
    // Any values over the alphabet, either possibly empty: whitespace, control
    // characters and a leading `-` all occur.
    let daemon = draw_text(&tc, 8);
    let runtime_root = draw_text(&tc, 10);
    let dir = tempfile::tempdir().unwrap();
    let target = ProxyTarget { cleat: fake_cleat(dir.path()), daemon: daemon.clone(), runtime_root: runtime_root.clone() };
    let command = target.shell_command().unwrap();

    let printed = sh(&command, dir.path());
    let args: Vec<String> = printed.split(|b| *b == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8(a.to_vec()).unwrap()).collect();
    let mut expected = vec!["proxy".to_string()];
    if !daemon.is_empty() {
        expected.push(format!("--daemon={daemon}"));
    }
    if !runtime_root.is_empty() {
        expected.push(format!("--runtime-root={runtime_root}"));
    }
    assert_eq!(args, expected, "command {command:?}");
}

// Behaviour: a Cleat path under `~/` runs from the far end's home, since the
// shell expands the unquoted `~/` even when the rest needs quoting.
#[test]
fn a_home_relative_cleat_runs_from_the_far_home() {
    let home = tempfile::tempdir().unwrap();
    let bin = home.path().join("bin dir");
    std::fs::create_dir(&bin).unwrap();
    let cleat = fake_cleat(&bin);
    let relative = format!("~/{}", Path::new(&cleat).strip_prefix(home.path()).unwrap().display());
    let target = ProxyTarget { cleat: relative, ..ProxyTarget::default() };
    assert_eq!(sh(&target.shell_command().unwrap(), home.path()), b"proxy\0");
}

// Behaviour: a Cleat shaped like an assignment (`a=b`) still runs as the
// command, since `=` is always quoted; unquoted, the shell would assign `a`
// and run `proxy` instead.
#[test]
fn an_assignment_shaped_cleat_runs_as_a_command() {
    let dir = tempfile::tempdir().unwrap();
    let cleat = dir.path().join("a=b");
    std::fs::rename(fake_cleat(dir.path()), &cleat).unwrap();
    let target = ProxyTarget { cleat: "a=b".into(), ..ProxyTarget::default() };
    let output = Command::new("/bin/sh").arg("-c").arg(target.shell_command().unwrap()).env("PATH", dir.path()).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, b"proxy\0");
}
