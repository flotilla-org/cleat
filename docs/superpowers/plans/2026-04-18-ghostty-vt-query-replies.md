# Ghostty VT Query-Reply Plumbing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Surface libghostty's internal query-reply bytes (DSR, DECRQM, DA1/DA2/DA3, etc.) through cleat's FFI so detached sessions using the `ghostty-vt` engine actually answer capability queries instead of dropping them.

**Architecture:** Register two libghostty callbacks on construction of `TerminalHandle`:
1. `GHOSTTY_TERMINAL_OPT_WRITE_PTY` — ghostty hands us reply bytes via a C callback; we buffer them in a heap `Vec<u8>` owned by the handle.
2. `GHOSTTY_TERMINAL_OPT_DEVICE_ATTRIBUTES` — ghostty asks us what to say for DA1/DA2/DA3; we fill a struct matching cleat's existing DA1/DA2 values (VT220 + ANSI color). DA replies are then serialized and emitted through WRITE_PTY.

A new `VtEngine::drain_replies(&mut self) -> Vec<u8>` seam pulls the buffered bytes out. After each `feed`, the session loop drains replies and writes them to the pty master. The existing `DeviceAttributeTracker` is retained for the passthrough engine (where it's still the only source of DA replies) but bypassed when running ghostty (where ghostty now answers DA itself and would double-reply).

**Out of scope (follow-up work):**
- Making the VT engine authoritative in *attached* mode (requires filtering DA/DSR replies on the host-terminal input path — separate design).
- Extending passthrough to answer more than DA1/DA2.
- Wiring ENQ, XTVERSION, XTWINOPS size, and color-scheme callbacks. (Structurally similar but not needed to close the reported agent pain point.)

**Tech Stack:** Rust 1.x (stable), libghostty C FFI, zig-built static library, `#[cfg(feature = "ghostty-vt")]` gated code path. Feature-on test commands use `--features ghostty-vt`.

**Conventions:**
- Run commands from the repo root unless otherwise stated.
- All commits on the current branch; author squashes/splits at end if needed.
- Per `CLAUDE.md`, always run: `cargo build --locked`, `cargo +nightly-2026-03-12 fmt --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test --workspace --locked`.
- For feature-on validation additionally: `cargo test -p cleat --features ghostty-vt --locked`.

---

## File Structure

| File | Responsibility | Action |
|---|---|---|
| `crates/cleat/src/vt/ghostty_ffi.rs` | Rust FFI bindings to libghostty — enums, extern decls, `TerminalHandle` | Modify: add option enum, callback typedefs, `ghostty_terminal_set` extern, reply-buffer field, trampolines, `drain_replies` method |
| `crates/cleat/src/vt/mod.rs` | `VtEngine` trait definition | Modify: add `drain_replies` method with empty default |
| `crates/cleat/src/vt/ghostty.rs` | `GhosttyVtEngine` implementation | Modify: override `drain_replies` to delegate to `TerminalHandle` |
| `crates/cleat/src/vt/passthrough.rs` | `PassthroughVtEngine` implementation | No code change (inherits default `drain_replies`) |
| `crates/cleat/src/session.rs` | Session daemon loop | Modify: drain engine replies after each feed and write to pty; gate `DeviceAttributeTracker` to passthrough path |

No new files are created.

---

## Task 1: FFI — callback type declarations

**Files:**
- Modify: `crates/cleat/src/vt/ghostty_ffi.rs` (top of file, near other `#[repr(C)]` enums)

**Goal:** Introduce the Rust-side types matching `GhosttyTerminalOption`, `GhosttyTerminalWritePtyFn`, `GhosttyTerminalDeviceAttributesFn`, and the `GhosttyDeviceAttributes*` structs. No runtime wiring yet.

- [ ] **Step 1: Add the option-enum and callback typedefs.**

Append the following after the existing `GhosttyTerminalOptions` struct definition (i.e., alongside other C ABI types, before the `#[link(...)] unsafe extern "C"` block):

```rust
#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GhosttyTerminalOption {
    Userdata = 0,
    WritePty = 1,
    Bell = 2,
    Enquiry = 3,
    Xtversion = 4,
    TitleChanged = 5,
    Size = 6,
    ColorScheme = 7,
    DeviceAttributes = 8,
    Title = 9,
}

/// Callback fired synchronously from `ghostty_terminal_vt_write` when the
/// terminal wants to send reply bytes back to the pty (DSR, DECRQM, DA, ...).
pub type GhosttyTerminalWritePtyFn = unsafe extern "C" fn(
    terminal: GhosttyTerminal,
    userdata: *mut c_void,
    data: *const u8,
    len: usize,
);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GhosttyDeviceAttributesPrimary {
    pub conformance_level: u16,
    pub features: [u16; 64],
    pub num_features: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GhosttyDeviceAttributesSecondary {
    pub device_type: u16,
    pub firmware_version: u16,
    pub rom_cartridge: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GhosttyDeviceAttributesTertiary {
    pub unit_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GhosttyDeviceAttributes {
    pub primary: GhosttyDeviceAttributesPrimary,
    pub secondary: GhosttyDeviceAttributesSecondary,
    pub tertiary: GhosttyDeviceAttributesTertiary,
}

/// Callback fired when ghostty receives a DA1/DA2/DA3 query. The app fills
/// `*out_attrs` with the response shape it wants to advertise. Return true
/// to emit, false to silently drop.
pub type GhosttyTerminalDeviceAttributesFn = unsafe extern "C" fn(
    terminal: GhosttyTerminal,
    userdata: *mut c_void,
    out_attrs: *mut GhosttyDeviceAttributes,
) -> bool;
```

Add the layout asserts (matching the pattern already in this file) just below:

```rust
// Computed for 64-bit targets: 2 (u16) + 128 ([u16; 64]) + 6 bytes trailing padding
// to re-align to usize (8) + 8 (usize num_features) = 144. 32-bit targets would be 136.
// This assert targets 64-bit (cleat's supported build targets).
const _: () = assert!(std::mem::size_of::<GhosttyDeviceAttributesPrimary>() == 144);
const _: () = assert!(std::mem::size_of::<GhosttyDeviceAttributesSecondary>() == 6);
const _: () = assert!(std::mem::size_of::<GhosttyDeviceAttributesTertiary>() == 4);
```

If you get a compile error reporting a different size (e.g. 32-bit CI), update the constant to the observed value rather than weakening the assertion — ABI mismatches here are exactly what these asserts exist to catch.

- [ ] **Step 2: Add the `ghostty_terminal_set` extern.**

Inside the existing `unsafe extern "C" { ... }` block (the one with `#[link(name = "ghostty-vt")]` near line 320), add:

```rust
    fn ghostty_terminal_set(terminal: GhosttyTerminal, option: GhosttyTerminalOption, value: *const c_void) -> GhosttyResult;
```

- [ ] **Step 3: Compile.**

Run: `cargo build --features ghostty-vt --locked`

Expected: builds clean. If the `size_of` assert in Step 1 fails, note the actual size reported by the compiler error and adjust the assert to the observed value (still an exact equality — we want to catch ABI drift).

- [ ] **Step 4: Commit.**

```bash
git add crates/cleat/src/vt/ghostty_ffi.rs
git commit -m "ghostty_ffi: declare terminal option enum and DA/WritePty callback types"
```

---

## Task 2: Reply buffer + WRITE_PTY callback wiring

**Files:**
- Modify: `crates/cleat/src/vt/ghostty_ffi.rs` (`TerminalHandle` struct and its `new`, `Drop`)

**Goal:** Extend `TerminalHandle` so it owns a heap-allocated `Vec<u8>` reply buffer, registers itself as the `WRITE_PTY` callback target on construction, and exposes `drain_replies`. Buffer is accessed by the C callback via a stable raw pointer; Rust side only reads it between feeds.

- [ ] **Step 1: Write the failing test.**

Append this test to `crates/cleat/src/vt/ghostty_ffi.rs` (inside a new `#[cfg(test)] mod tests { ... }` at the end of the file):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_captures_dsr_reply_into_drain_buffer() {
        let mut term = TerminalHandle::new(80, 24, 1024).expect("new terminal");
        // CSI 6 n = DSR Cursor Position Report — should produce CSI <row> ; <col> R
        term.feed(b"\x1b[6n");
        let reply = term.drain_replies();
        assert!(
            reply.starts_with(b"\x1b[") && reply.ends_with(b"R"),
            "expected CPR reply, got {reply:?}",
        );
    }
}
```

- [ ] **Step 2: Run the test and confirm it fails.**

Run: `cargo test --features ghostty-vt -p cleat --locked vt::ghostty_ffi::tests::terminal_captures_dsr_reply`

Expected: FAIL — `drain_replies` method does not exist on `TerminalHandle`.

- [ ] **Step 3: Implement the reply buffer and callback trampoline.**

Replace the existing `TerminalHandle` struct definition (currently around line 376) and its `impl`/`Drop` blocks with:

```rust
pub struct TerminalHandle {
    raw: GhosttyTerminal,
    /// Heap-allocated so the address stays stable while the C side holds
    /// a pointer to it via userdata. The callback pushes reply bytes here.
    reply_buf: Box<Vec<u8>>,
}

unsafe extern "C" fn write_pty_trampoline(
    _terminal: GhosttyTerminal,
    userdata: *mut c_void,
    data: *const u8,
    len: usize,
) {
    if userdata.is_null() || data.is_null() || len == 0 {
        return;
    }
    // SAFETY: userdata is the raw pointer to a Box<Vec<u8>> we registered
    // when constructing this terminal; ghostty calls us synchronously from
    // vt_write, so the Box is live for the duration of the call.
    let buf = unsafe { &mut *(userdata as *mut Vec<u8>) };
    let slice = unsafe { std::slice::from_raw_parts(data, len) };
    buf.extend_from_slice(slice);
}

impl TerminalHandle {
    pub fn new(cols: u16, rows: u16, max_scrollback: usize) -> Result<Self, String> {
        let mut raw = ptr::null_mut();
        let result = unsafe { ghostty_terminal_new(ptr::null(), &mut raw, GhosttyTerminalOptions { cols, rows, max_scrollback }) };
        check_result(result, "ghostty_terminal_new")?;

        let mut reply_buf: Box<Vec<u8>> = Box::<Vec<u8>>::default();
        // The raw pointer to the *inner* Vec<u8> is what we pass as userdata.
        let userdata_ptr = (&*reply_buf) as *const Vec<u8> as *mut c_void;

        let set_user = unsafe {
            ghostty_terminal_set(raw, GhosttyTerminalOption::Userdata, userdata_ptr as *const c_void)
        };
        if let Err(err) = check_result(set_user, "ghostty_terminal_set(Userdata)") {
            unsafe { ghostty_terminal_free(raw) };
            return Err(err);
        }

        let write_pty_cb: GhosttyTerminalWritePtyFn = write_pty_trampoline;
        let set_wp = unsafe {
            ghostty_terminal_set(
                raw,
                GhosttyTerminalOption::WritePty,
                write_pty_cb as *const c_void,
            )
        };
        if let Err(err) = check_result(set_wp, "ghostty_terminal_set(WritePty)") {
            unsafe { ghostty_terminal_free(raw) };
            return Err(err);
        }

        Ok(Self { raw, reply_buf })
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<(), String> {
        let result = unsafe { ghostty_terminal_resize(self.raw, cols, rows, 1, 1) };
        check_result(result, "ghostty_terminal_resize")
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        unsafe { ghostty_terminal_vt_write(self.raw, bytes.as_ptr(), bytes.len()) };
    }

    pub fn raw(&self) -> GhosttyTerminal {
        self.raw
    }

    /// Take all reply bytes libghostty has accumulated since the last drain.
    pub fn drain_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut *self.reply_buf)
    }
}

impl Drop for TerminalHandle {
    fn drop(&mut self) {
        // Free the terminal BEFORE the reply_buf Box drops. libghostty will not
        // call our callback after this point, so the raw pointer stored in its
        // userdata becomes dead at the same instant the Box is released.
        unsafe { ghostty_terminal_free(self.raw) };
        // reply_buf drops automatically afterwards.
    }
}
```

Note: `ghostty_terminal_set` expects the function pointer **by value** (`write_pty_cb as *const c_void`). The `let write_pty_cb: GhosttyTerminalWritePtyFn = ...` binding exists purely to force a coercion check against the typedef'd fn-pointer type, so any mismatch in the trampoline's signature fails to compile here. Passing `&write_pty_cb as *const _ as *const c_void` instead causes a SIGBUS at runtime because libghostty stores the pointer directly as the callback rather than dereferencing it once.

Also: remove `#[allow(dead_code)]` from `GhosttyTerminalWritePtyFn` and the `ghostty_terminal_set` extern — this task makes them live. Leave the DA attribute-suppressions alone; Task 3 will remove those.

- [ ] **Step 4: Run the test again — expect pass.**

Run: `cargo test --features ghostty-vt -p cleat --locked vt::ghostty_ffi::tests::terminal_captures_dsr_reply`

Expected: PASS. If it fails with an empty reply, the likely causes (in order of likelihood):
1. `ghostty_terminal_set` rejected one of the values — inspect the error string.
2. The CPR reply format uses `\x1b\\` instead of `\x1b[` — unlikely but check the actual bytes returned.

- [ ] **Step 5: Commit.**

```bash
git add crates/cleat/src/vt/ghostty_ffi.rs
git commit -m "ghostty_ffi: register WRITE_PTY callback and buffer reply bytes"
```

---

## Task 3: Device-attributes callback

**Files:**
- Modify: `crates/cleat/src/vt/ghostty_ffi.rs` (`TerminalHandle::new` — add a second `ghostty_terminal_set` call)

**Goal:** Register a DA callback returning the same DA1/DA2 cleat's standalone tracker emits today (`\x1b[?62;22c` / `\x1b[>1;10;0c`). After this, feeding `CSI c` through the ghostty terminal results in `\x1b[?62;22c` in the reply buffer.

- [ ] **Step 1: Write the failing test.**

Add to the `tests` module in `crates/cleat/src/vt/ghostty_ffi.rs`:

```rust
    #[test]
    fn terminal_answers_da1_with_vt220_and_ansi_color() {
        let mut term = TerminalHandle::new(80, 24, 1024).expect("new terminal");
        term.feed(b"\x1b[c");
        let reply = term.drain_replies();
        assert_eq!(reply, b"\x1b[?62;22c".to_vec());
    }

    #[test]
    fn terminal_answers_da2_with_vt220_firmware_10() {
        let mut term = TerminalHandle::new(80, 24, 1024).expect("new terminal");
        term.feed(b"\x1b[>c");
        let reply = term.drain_replies();
        assert_eq!(reply, b"\x1b[>1;10;0c".to_vec());
    }
```

- [ ] **Step 2: Run the tests — expect fail (empty reply).**

Run: `cargo test --features ghostty-vt -p cleat --locked vt::ghostty_ffi::tests::terminal_answers_da`

Expected: both tests FAIL with empty reply (libghostty silently drops DA when no DA callback is registered).

- [ ] **Step 3: Implement the DA trampoline and register the callback.**

Add above `write_pty_trampoline` in `ghostty_ffi.rs`:

```rust
/// DA1 feature code for ANSI color (see device.h: GHOSTTY_DA_FEATURE_ANSI_COLOR).
const DA_FEATURE_ANSI_COLOR: u16 = 22;
/// VT220 conformance (see device.h: GHOSTTY_DA_CONFORMANCE_VT220).
const DA_CONFORMANCE_VT220: u16 = 62;
/// VT220 device type for DA2 (see device.h: GHOSTTY_DA_DEVICE_TYPE_VT220).
const DA_DEVICE_TYPE_VT220: u16 = 1;
/// DA2 firmware version. Matches cleat's pre-existing synthetic reply.
const DA_FIRMWARE_VERSION: u16 = 10;

unsafe extern "C" fn device_attributes_trampoline(
    _terminal: GhosttyTerminal,
    _userdata: *mut c_void,
    out_attrs: *mut GhosttyDeviceAttributes,
) -> bool {
    if out_attrs.is_null() {
        return false;
    }
    let mut features = [0u16; 64];
    features[0] = DA_FEATURE_ANSI_COLOR;
    let attrs = GhosttyDeviceAttributes {
        primary: GhosttyDeviceAttributesPrimary {
            conformance_level: DA_CONFORMANCE_VT220,
            features,
            num_features: 1,
        },
        secondary: GhosttyDeviceAttributesSecondary {
            device_type: DA_DEVICE_TYPE_VT220,
            firmware_version: DA_FIRMWARE_VERSION,
            rom_cartridge: 0,
        },
        tertiary: GhosttyDeviceAttributesTertiary { unit_id: 0 },
    };
    unsafe { *out_attrs = attrs };
    true
}
```

Then in `TerminalHandle::new`, after the `WritePty` registration block (and before the `Ok(Self { ... })` return), add:

```rust
        let da_cb: GhosttyTerminalDeviceAttributesFn = device_attributes_trampoline;
        let set_da = unsafe {
            ghostty_terminal_set(
                raw,
                GhosttyTerminalOption::DeviceAttributes,
                da_cb as *const c_void,
            )
        };
        if let Err(err) = check_result(set_da, "ghostty_terminal_set(DeviceAttributes)") {
            unsafe { ghostty_terminal_free(raw) };
            return Err(err);
        }
```

- [ ] **Step 4: Run both DA tests — expect pass.**

Run: `cargo test --features ghostty-vt -p cleat --locked vt::ghostty_ffi::tests::terminal_answers_da`

Expected: both PASS.

If DA1 passes but the emitted bytes differ in one character (e.g. extra space, different terminator), the format libghostty uses may not match cleat's exact serialization. Inspect the actual bytes and update the test assertion OR, if you need byte-for-byte compatibility with existing clients, document the deviation in the commit message.

- [ ] **Step 5: Commit.**

```bash
git add crates/cleat/src/vt/ghostty_ffi.rs
git commit -m "ghostty_ffi: advertise DA1 VT220+color and DA2 VT220 firmware 10"
```

---

## Task 4: Add `drain_replies` to the `VtEngine` trait

**Files:**
- Modify: `crates/cleat/src/vt/mod.rs` (trait definition around line 175)

**Goal:** Introduce a trait seam so the session loop can pull reply bytes from whichever engine is active without caring about the concrete type. Default returns empty — engines that have nothing to contribute (currently passthrough, and any future engine that delegates entirely to cleat-level trackers) don't need any code change.

- [ ] **Step 1: Extend the trait.**

Edit `crates/cleat/src/vt/mod.rs`, in the `pub trait VtEngine { ... }` block, add the method with its default:

```rust
pub trait VtEngine {
    fn feed(&mut self, bytes: &[u8]) -> Result<(), String>;
    fn resize(&mut self, cols: u16, rows: u16) -> Result<(), String>;
    fn supports_replay(&self) -> bool;
    fn replay_payload(&self, capabilities: &ClientCapabilities) -> Result<Option<Vec<u8>>, String>;
    fn screen_text(&self) -> Result<String, String>;
    fn screen_grid(&mut self) -> Result<ScreenGrid, String>;
    fn size(&self) -> (u16, u16);

    /// Reply bytes (DSR, DECRQM, DA, ...) the engine has buffered since the
    /// last call. Default is empty for engines that don't synthesize replies.
    fn drain_replies(&mut self) -> Vec<u8> {
        Vec::new()
    }
}
```

- [ ] **Step 2: Build to ensure trait-default compiles.**

Run: `cargo build --locked`

Expected: clean build. `PassthroughVtEngine` compiles without modification (inherits default).

- [ ] **Step 3: Commit.**

```bash
git add crates/cleat/src/vt/mod.rs
git commit -m "vt: add drain_replies to VtEngine trait with empty default"
```

---

## Task 5: Implement `drain_replies` on `GhosttyVtEngine`

**Files:**
- Modify: `crates/cleat/src/vt/ghostty.rs` (the `impl VtEngine for GhosttyVtEngine` block)

**Goal:** Route the engine-level `drain_replies` call into the underlying `TerminalHandle::drain_replies`.

- [ ] **Step 1: Write the failing test.**

Append to `crates/cleat/src/vt/mod.rs` in the `#[cfg(test)] mod tests` block (so it lives alongside the smoke test already there):

```rust
    #[cfg(feature = "ghostty-vt")]
    #[test]
    fn ghostty_engine_drains_da1_reply_after_feed() {
        let mut engine = super::make_default_vt_engine(80, 24);
        engine.feed(b"\x1b[c").expect("feed DA1");
        let reply = engine.drain_replies();
        assert_eq!(reply, b"\x1b[?62;22c".to_vec());
        // Second drain is empty — buffer is consumed.
        assert!(engine.drain_replies().is_empty());
    }
```

- [ ] **Step 2: Run the test — expect fail.**

Run: `cargo test --features ghostty-vt -p cleat --locked vt::tests::ghostty_engine_drains_da1`

Expected: FAIL with `left: b""` — the default trait impl returns empty.

- [ ] **Step 3: Override `drain_replies` on `GhosttyVtEngine`.**

In `crates/cleat/src/vt/ghostty.rs`, inside `impl VtEngine for GhosttyVtEngine { ... }`, add (placement: next to `feed`):

```rust
    fn drain_replies(&mut self) -> Vec<u8> {
        self.terminal.drain_replies()
    }
```

- [ ] **Step 4: Run the test — expect pass.**

Run: `cargo test --features ghostty-vt -p cleat --locked vt::tests::ghostty_engine_drains_da1`

Expected: PASS.

- [ ] **Step 5: Commit.**

```bash
git add crates/cleat/src/vt/ghostty.rs crates/cleat/src/vt/mod.rs
git commit -m "ghostty: wire drain_replies through to TerminalHandle"
```

---

## Task 6: Drain replies in the session loop and gate the DA tracker

**Files:**
- Modify: `crates/cleat/src/session.rs` (around lines 485-486, 862-866)

**Goal:** After each pty-output feed, ask the engine for reply bytes and write them to the pty master. Skip the standalone `DeviceAttributeTracker` when running the ghostty engine (ghostty now answers DA itself; double-replies would break detection heuristics that count responses).

- [ ] **Step 1: Gate the DA tracker to passthrough only.**

Find the line `let mut detached_da = DeviceAttributeTracker::new();` (currently line 486). Replace with:

```rust
    // The DA tracker is the only DA source for the passthrough engine.
    // The ghostty engine answers DA itself via its DeviceAttributes callback,
    // so we skip the tracker there to avoid double replies.
    let mut detached_da = match session.vt_engine {
        vt::VtEngineKind::Passthrough => Some(DeviceAttributeTracker::new()),
        vt::VtEngineKind::Ghostty => None,
    };
```

Then find the block (currently lines 862-866):

```rust
                        if active_client.is_none() {
                            for reply in detached_da.push(&buf[..n]) {
                                write_fd_all(pty_fd, &reply)?;
                            }
                        }
```

Replace with:

```rust
                        if active_client.is_none() {
                            if let Some(ref mut tracker) = detached_da {
                                for reply in tracker.push(&buf[..n]) {
                                    write_fd_all(pty_fd, &reply)?;
                                }
                            }
                            let engine_reply = vt_engine.drain_replies();
                            if !engine_reply.is_empty() {
                                write_fd_all(pty_fd, &engine_reply)?;
                            }
                        }
```

Notes on placement:
- `drain_replies` is called *after* `record_pty_output` has already fed the bytes into the engine (that call is already in the loop on line 848).
- The drain is gated on `active_client.is_none()` to match the existing DA tracker's behavior — in attached mode the real host terminal answers queries, so cleat must not inject competing replies. This preserves current attached-mode semantics; making the VT authoritative in attached mode is deliberately out of scope for this plan.

- [ ] **Step 2: Build and run existing tests.**

Run:
```bash
cargo build --locked
cargo test --workspace --locked
cargo test -p cleat --features ghostty-vt --locked
```

Expected: all existing tests still pass. Passthrough-mode DA behavior is unchanged because the tracker is still active for that engine. Ghostty-mode DA is now emitted by the engine drain.

If a test in `da.rs` or session-level tests breaks because it asserted that the DA tracker was always present regardless of engine, update it: the tracker is conditional now.

- [ ] **Step 3: Commit.**

```bash
git add crates/cleat/src/session.rs
git commit -m "session: drain engine replies in detached mode; gate DA tracker to passthrough"
```

---

## Task 7: End-to-end test — feed a query through the public engine API

**Files:**
- Modify: `crates/cleat/src/vt/mod.rs` (extend the `tests` module)

**Goal:** Demonstrate coverage beyond DA — a DSR CPR query through the trait-level `feed` API produces a reply via `drain_replies`. This is the behaviour change users will actually notice: capability-detection code paths that previously hung on CPR now get answered.

- [ ] **Step 1: Write the test.**

Append to the `tests` module in `crates/cleat/src/vt/mod.rs`:

```rust
    #[cfg(feature = "ghostty-vt")]
    #[test]
    fn ghostty_engine_answers_cursor_position_report() {
        let mut engine = super::make_default_vt_engine(80, 24);
        // Move cursor to row 5, col 10 (1-based in CPR output), then ask.
        // ESC[5;10H = CUP, ESC[6n = DSR CPR.
        engine.feed(b"\x1b[5;10H\x1b[6n").expect("feed CUP+DSR");
        let reply = engine.drain_replies();
        assert_eq!(reply, b"\x1b[5;10R".to_vec());
    }
```

- [ ] **Step 2: Run the test.**

Run: `cargo test --features ghostty-vt -p cleat --locked vt::tests::ghostty_engine_answers_cursor_position_report`

Expected: PASS.

If it fails with a different format (e.g. libghostty emits `\x1b[?5;10R` or uses 0-based coordinates), adjust the assertion to match the actual bytes and record the discrepancy in the commit message. This is a characterization test — we're pinning down libghostty's exact output so future drift is visible.

- [ ] **Step 3: Commit.**

```bash
git add crates/cleat/src/vt/mod.rs
git commit -m "test: ghostty engine answers DSR CPR via drain_replies"
```

---

## Task 8: Full validation sweep

**Goal:** Make sure every gate CLAUDE.md specifies is green before declaring done.

- [ ] **Step 1: Format check.**

Run: `cargo +nightly-2026-03-12 fmt --check`

Expected: no output (clean).

If it fails, run `cargo +nightly-2026-03-12 fmt` and amend the prior commit on the file that tripped it:

```bash
git add -u
git commit --amend --no-edit
```

Only amend the immediately-prior commit. If fmt issues span multiple earlier commits, make a new fixup commit instead (`git commit -m "fmt"`).

- [ ] **Step 2: Clippy — feature off.**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

Expected: no warnings.

- [ ] **Step 3: Clippy — feature on.**

Run: `cargo clippy --workspace --all-targets --features cleat/ghostty-vt --locked -- -D warnings`

Expected: no warnings. Any new `unsafe` blocks added in `ghostty_ffi.rs` should already have a `// SAFETY:` comment per the pattern already used in that file; if clippy flags one you missed, add it.

- [ ] **Step 4: Test — feature off.**

Run: `cargo test --workspace --locked`

Expected: all pass.

- [ ] **Step 5: Test — feature on.**

Run: `cargo test -p cleat --features ghostty-vt --locked`

Expected: all pass, including the new tests from Tasks 2, 3, 5, 7.

- [ ] **Step 6: Build — feature on, release.**

Run: `cargo build -p cleat --features ghostty-vt --locked --release`

Expected: clean build. (Release build catches the occasional debug-only assertion or inlining difference.)

- [ ] **Step 7: No commit — this task gates merge, not new changes.**

---

## Post-implementation notes (for the PR description, not a commit)

- DA replies for the ghostty engine now go through ghostty's DEVICE_ATTRIBUTES callback, not cleat's `DeviceAttributeTracker`. Passthrough is unchanged.
- DSR, DECRQM, and any other queries libghostty can answer (per the WRITE_PTY callback coverage) now flow back to the child process in detached mode.
- Attached mode behavior is unchanged: the host terminal remains authoritative when a client is attached. Making the VT authoritative regardless of attachment is a follow-up that requires input-path escape-sequence filtering — out of scope here.
- Not covered by this change: kitty-keyboard query (`CSI ? u`), kitty-graphics query (`APC G...q=...`), XTGETTCAP (`DCS + q ... ST`). These do not appear to go through WRITE_PTY in libghostty; a separate investigation is needed before we can claim full parity with real kitty/ghostty.
