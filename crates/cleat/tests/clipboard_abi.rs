use cleat::provider_ffi::*;

#[test]
fn older_abi_is_rejected_and_mock_reports_no_clipboard_capability() {
    // ABI compatibility is explicit; unsupported engines must never advertise
    // writes or fabricate events, including in the Rust-only build.
    unsafe {
        let old = cleat_provider_open(&CleatProviderDesc { abi_version: CLEAT_PROVIDER_ABI_VERSION - 1, ..Default::default() });
        assert!(old.is_null());
        let provider = cleat_provider_open(&CleatProviderDesc {
            abi_version: CLEAT_PROVIDER_ABI_VERSION,
            backend: CLEAT_PROVIDER_BACKEND_MOCK,
            ..Default::default()
        });
        assert!(!provider.is_null());
        let session = cleat_session_create(provider, &CleatSessionDesc::default());
        assert!(!session.is_null());
        assert!(!cleat_session_clipboard_supported(session));
        for _ in 0..3 {
            assert!(cleat_session_acquire_clipboard_event(session).is_null());
        }
        assert_eq!(cleat_session_clipboard_dropped(session), 0);
        cleat_clipboard_event_release(std::ptr::null());
        cleat_session_destroy(session);
        cleat_provider_close(provider);
    }
}

#[test]
fn passthrough_vt_reports_unsupported_without_effects() {
    use cleat::vt::{passthrough::PassthroughVtEngine, VtEngine};
    // The placeholder engine must work without Ghostty and honestly report
    // that it cannot turn child bytes into clipboard effects.
    let mut vt = PassthroughVtEngine::new(80, 24);
    assert!(!vt.clipboard_supported());
    vt.feed(b"\x1b]52;c;aGVsbG8=\x07").unwrap();
    assert!(vt.drain_clipboard().0.is_empty());
}
