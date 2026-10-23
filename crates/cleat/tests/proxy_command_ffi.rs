#![cfg(unix)]
//! The proxy command and transport-state C interface (cleat#383):
//! `cleat_proxy_command`, and the `cleat_proxy_target` and
//! `cleat_transport_state` layouts checked against the header. The live
//! transport state is in `remote_transport.rs`.

use std::{fs, process::Command};

use cleat::{provider_ffi::*, proxy_command::ProxyTarget};
use hegel::generators as gs;

fn text(s: &[u8]) -> CleatStr {
    CleatStr { ptr: s.as_ptr(), len: s.len() }
}

/// The C helper's answer: the size query, then the command read into a
/// buffer of exactly that size. None when either call refuses.
fn c_command(target: &CleatProxyTarget) -> Option<String> {
    let mut len = usize::MAX;
    if !unsafe { cleat_proxy_command(target, std::ptr::null_mut(), 0, &mut len) } {
        return None;
    }
    let mut buffer = vec![0u8; len];
    assert!(unsafe { cleat_proxy_command(target, buffer.as_mut_ptr(), buffer.len(), &mut len) });
    assert_eq!(len, buffer.len());
    Some(String::from_utf8(buffer).expect("the command is UTF-8"))
}

// Behaviour: the C helper gives exactly the Rust command for any target, and
// refuses exactly what the Rust one refuses (a NUL).
#[hegel::test(test_cases = 200)]
fn c_command_matches_the_rust_command(tc: hegel::TestCase) {
    // Short fields over quoting-relevant characters, multi-byte UTF-8 and NUL,
    // each possibly empty, so defaults, quoting and refusals all occur.
    let field = || -> String {
        tc.draw(gs::vecs(gs::sampled_from(vec!['a', '-', ' ', '\'', '~', '/', '=', 'é', '\0'])).max_size(4)).into_iter().collect()
    };
    let target = ProxyTarget { cleat: field(), daemon: field(), runtime_root: field() };
    let c = CleatProxyTarget {
        cleat: text(target.cleat.as_bytes()),
        daemon: text(target.daemon.as_bytes()),
        runtime_root: text(target.runtime_root.as_bytes()),
        ..CleatProxyTarget::default()
    };
    assert_eq!(c_command(&c), target.shell_command().ok(), "{target:?}");
}

// Behaviour: a buffer one byte short is refused (with the needed length
// reported), as are bytes that aren't UTF-8, a struct smaller than this ABI's,
// and null pointers.
#[test]
fn c_command_refuses_what_it_cannot_write() {
    let daemon = b"work";
    let target = CleatProxyTarget { daemon: text(daemon), ..CleatProxyTarget::default() };
    let needed = "cleat proxy --daemon=work".len();
    let mut buffer = vec![0u8; needed - 1];
    let mut len = 0;
    assert!(!unsafe { cleat_proxy_command(&target, buffer.as_mut_ptr(), buffer.len(), &mut len) });

    let invalid = [0xffu8];
    assert_eq!(c_command(&CleatProxyTarget { runtime_root: text(&invalid), ..target }), None);
    assert_eq!(c_command(&CleatProxyTarget { size: 8, ..target }), None);
    assert!(!unsafe { cleat_proxy_command(std::ptr::null(), std::ptr::null_mut(), 0, &mut len) });
    assert!(!unsafe { cleat_proxy_command(&target, std::ptr::null_mut(), 0, std::ptr::null_mut()) });
}

// Behaviour: the header declares the same layouts the library uses. Compiled
// with the system C compiler against include/cleat_provider.h.
#[test]
fn header_layouts_match() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("layout.c");
    fs::write(
        &source,
        r#"#include "cleat_provider.h"
#include <stdio.h>
#include <stddef.h>
int main(void) {
    printf("%zu %zu %zu %zu\n", sizeof(cleat_proxy_target), offsetof(cleat_proxy_target, cleat), offsetof(cleat_proxy_target, daemon),
        offsetof(cleat_proxy_target, runtime_root));
    printf("%zu %zu %zu %zu %zu %zu %zu %zu %zu\n", sizeof(cleat_transport_state), offsetof(cleat_transport_state, connected),
        offsetof(cleat_transport_state, retrying), offsetof(cleat_transport_state, error), offsetof(cleat_transport_state, error_capacity),
        offsetof(cleat_transport_state, error_len), offsetof(cleat_transport_state, warning), offsetof(cleat_transport_state, warning_capacity),
        offsetof(cleat_transport_state, warning_len));
    return 0;
}
"#,
    )
    .unwrap();
    let binary = dir.path().join("layout");
    let include = concat!(env!("CARGO_MANIFEST_DIR"), "/include");
    let built = Command::new("cc").arg("-I").arg(include).arg(&source).arg("-o").arg(&binary).status().unwrap();
    assert!(built.success());
    let printed = String::from_utf8(Command::new(&binary).output().unwrap().stdout).unwrap();
    use std::mem::{offset_of, size_of};
    let expected = format!(
        "{} {} {} {}\n{} {} {} {} {} {} {} {} {}\n",
        size_of::<CleatProxyTarget>(),
        offset_of!(CleatProxyTarget, cleat),
        offset_of!(CleatProxyTarget, daemon),
        offset_of!(CleatProxyTarget, runtime_root),
        size_of::<CleatTransportState>(),
        offset_of!(CleatTransportState, connected),
        offset_of!(CleatTransportState, retrying),
        offset_of!(CleatTransportState, error),
        offset_of!(CleatTransportState, error_capacity),
        offset_of!(CleatTransportState, error_len),
        offset_of!(CleatTransportState, warning),
        offset_of!(CleatTransportState, warning_capacity),
        offset_of!(CleatTransportState, warning_len),
    );
    assert_eq!(printed, expected);
}
