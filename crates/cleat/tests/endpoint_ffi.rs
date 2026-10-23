#![cfg(unix)]
//! The endpoint C interface (cleat#383): `cleat_endpoint_check` and the
//! endpoint and transport-state structs,
//! with their C layouts checked against the header. The live ssh case is in
//! `endpoint_ssh.rs`.

use std::{fs, process::Command};

use cleat::provider_ffi::*;

fn text(s: &str) -> CleatStr {
    CleatStr { ptr: s.as_ptr(), len: s.len() }
}

fn check(endpoint: &CleatEndpoint, capacity: usize) -> (u32, String) {
    let mut reason = vec![0u8; capacity];
    let mut len = usize::MAX;
    let kind = unsafe { cleat_endpoint_check(endpoint, reason.as_mut_ptr(), capacity, &mut len) };
    (kind, String::from_utf8(reason[..len].to_vec()).expect("the reason is cut at a character boundary"))
}

// Behaviour: the C check agrees with the endpoint rules, gives the reason for
// an invalid endpoint, never splits a character when the reason is cut to fit,
// and refuses a struct smaller than this ABI's.
#[test]
fn endpoint_check_reports_kind_or_reason() {
    let (local, ssh, host) = ("local", "ssh", "crew@fleet-3");
    let valid = CleatEndpoint { transport: text(ssh), host: text(host), ..CleatEndpoint::default() };
    assert_eq!(check(&valid, 64), (CLEAT_ENDPOINT_SSH, String::new()));
    assert_eq!(check(&CleatEndpoint { transport: text(local), ..CleatEndpoint::default() }, 64).0, CLEAT_ENDPOINT_LOCAL);

    let typo = "ssh ";
    let (kind, reason) = check(&CleatEndpoint { transport: text(typo), ..valid }, 128);
    assert_eq!(kind, CLEAT_ENDPOINT_INVALID);
    assert!(reason.contains("unknown endpoint transport"), "{reason}");

    // "unknown endpoint transport \"é…" cut inside the two-byte é.
    let accented = "é";
    let prefix = "unknown endpoint transport \"".len();
    let (_, cut) = check(&CleatEndpoint { transport: text(accented), ..valid }, prefix + 1);
    assert_eq!(cut.len(), prefix, "{cut:?}");

    let dash = "-oProxyCommand=x";
    assert_eq!(check(&CleatEndpoint { host: text(dash), ..valid }, 64).0, CLEAT_ENDPOINT_INVALID);
    assert_eq!(check(&CleatEndpoint { size: 8, ..valid }, 64).0, CLEAT_ENDPOINT_INVALID);
    assert_eq!(unsafe { cleat_endpoint_check(std::ptr::null(), std::ptr::null_mut(), 0, std::ptr::null_mut()) }, CLEAT_ENDPOINT_INVALID);
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
    printf("%zu %zu %zu %zu %zu %zu\n", sizeof(cleat_endpoint), offsetof(cleat_endpoint, transport), offsetof(cleat_endpoint, host),
        offsetof(cleat_endpoint, runtime_root), offsetof(cleat_endpoint, daemon), offsetof(cleat_endpoint, cleat_command));
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
        "{} {} {} {} {} {}\n{} {} {} {} {} {} {} {} {}\n",
        size_of::<CleatEndpoint>(),
        offset_of!(CleatEndpoint, transport),
        offset_of!(CleatEndpoint, host),
        offset_of!(CleatEndpoint, runtime_root),
        offset_of!(CleatEndpoint, daemon),
        offset_of!(CleatEndpoint, cleat_command),
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
