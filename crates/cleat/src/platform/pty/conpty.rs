//! ConPTY selection (ADR 0006).
//!
//! The inbox (kernel32) ConPTY drops Kitty graphics APC and sixel DCS; the
//! `Microsoft.Windows.Console.ConPTY` package's `conpty.dll` + `OpenConsole.exe`
//! pass them through. Cleat loads that `conpty.dll` at runtime from its own
//! executable's directory only, and falls back to the inbox ConPTY, reporting
//! the fallback, when the bundle is absent or unusable.

use std::{
    env,
    ffi::c_void,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use windows_sys::{
    core::HRESULT,
    Win32::{
        Foundation::{GetLastError, HANDLE},
        System::{
            Console::{ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole, COORD, HPCON},
            LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32},
        },
    },
};

use crate::protocol::{ConptyInfo, ConptyKind};

/// Diagnostic override read from the daemon's environment when a session starts.
/// `inbox` forces the kernel32 ConPTY; anything else (or unset) prefers the bundle.
pub const CONPTY_OVERRIDE_ENV: &str = "CLEAT_CONPTY";
pub const CONPTY_DLL: &str = "conpty.dll";
pub const OPEN_CONSOLE_EXE: &str = "OpenConsole.exe";
/// The pinned package version (from `tools/conpty.toml`) this build staged.
pub const BUNDLED_VERSION: &str = env!("CLEAT_CONPTY_VERSION");

type CreateFn = unsafe extern "system" fn(COORD, HANDLE, HANDLE, u32, *mut HPCON) -> HRESULT;
type ResizeFn = unsafe extern "system" fn(HPCON, COORD) -> HRESULT;
type CloseFn = unsafe extern "system" fn(HPCON);

/// The three pseudoconsole entry points Cleat uses, from one implementation.
#[derive(Clone, Copy)]
pub struct ConptyApi {
    create: CreateFn,
    resize: ResizeFn,
    close: CloseFn,
}

impl ConptyApi {
    const INBOX: Self = Self { create: CreatePseudoConsole, resize: ResizePseudoConsole, close: ClosePseudoConsole };

    pub fn create(&self, cols: u16, rows: u16, input: HANDLE, output: HANDLE) -> Result<HPCON, String> {
        let mut conpty = 0;
        // Flags stay 0: the pass-through probes used 0, INHERIT_CURSOR would add
        // a cursor-position query to the startup handshake, and the glyph-width
        // flags do not affect APC/DCS pass-through.
        let result = unsafe { (self.create)(COORD { X: cols as i16, Y: rows as i16 }, input, output, 0, &mut conpty) };
        if result < 0 {
            Err(format!("CreatePseudoConsole failed with HRESULT 0x{result:08x}"))
        } else {
            Ok(conpty)
        }
    }

    pub fn resize(&self, conpty: HPCON, cols: u16, rows: u16) -> Result<(), String> {
        let result = unsafe { (self.resize)(conpty, COORD { X: cols as i16, Y: rows as i16 }) };
        if result < 0 {
            Err(format!("ResizePseudoConsole failed with HRESULT 0x{result:08x}"))
        } else {
            Ok(())
        }
    }

    /// # Safety
    /// `conpty` must come from this API's `create` and not be closed already.
    pub unsafe fn close(&self, conpty: HPCON) {
        unsafe { (self.close)(conpty) }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConptyPreference {
    /// Use the bundle beside the executable, else fall back to inbox.
    Bundled,
    /// Force the inbox ConPTY (diagnostics and the pass-through regression).
    Inbox,
}

impl ConptyPreference {
    pub fn from_env() -> Self {
        match env::var(CONPTY_OVERRIDE_ENV) {
            Ok(value) if value.eq_ignore_ascii_case("inbox") => Self::Inbox,
            _ => Self::Bundled,
        }
    }
}

/// Choose the ConPTY for a new session and describe the choice.
pub fn select(preference: ConptyPreference) -> (ConptyApi, ConptyInfo) {
    let fallback_reason = match preference {
        ConptyPreference::Inbox => format!("forced by {CONPTY_OVERRIDE_ENV}=inbox"),
        ConptyPreference::Bundled => match bundled() {
            Ok((api, path)) => {
                let info = ConptyInfo {
                    kind: ConptyKind::Bundled,
                    graphics_passthrough: true,
                    version: Some(BUNDLED_VERSION.to_string()),
                    path: Some(path.clone()),
                    fallback_reason: None,
                };
                return (*api, info);
            }
            Err(reason) => reason.clone(),
        },
    };
    let info = ConptyInfo {
        kind: ConptyKind::Inbox,
        graphics_passthrough: false,
        version: None,
        path: None,
        fallback_reason: Some(fallback_reason),
    };
    (ConptyApi::INBOX, info)
}

/// The bundled ConPTY, loaded once per process. The module stays loaded for
/// the process lifetime because live pseudoconsoles hold its entry points.
fn bundled() -> &'static Result<(ConptyApi, PathBuf), String> {
    static BUNDLED: OnceLock<Result<(ConptyApi, PathBuf), String>> = OnceLock::new();
    BUNDLED.get_or_init(|| {
        let exe = env::current_exe().map_err(|err| format!("cannot locate the executable: {err}"))?;
        let dir = exe.parent().ok_or_else(|| format!("executable {} has no directory", exe.display()))?;
        load_bundled(dir)
    })
}

fn load_bundled(dir: &Path) -> Result<(ConptyApi, PathBuf), String> {
    let dll = dir.join(CONPTY_DLL);
    if !dll.is_file() {
        return Err(format!("{CONPTY_DLL} is not beside the executable in {}", dir.display()));
    }
    // conpty.dll starts OpenConsole.exe from beside itself and silently uses
    // the system conhost.exe when it is missing, which would drop graphics
    // like the inbox ConPTY while claiming to be the bundle.
    if !dir.join(OPEN_CONSOLE_EXE).is_file() {
        return Err(format!("{OPEN_CONSOLE_EXE} is not beside {}", dll.display()));
    }
    let wide: Vec<u16> = dll.as_os_str().encode_wide().chain(Some(0)).collect();
    // An absolute path with these flags loads exactly this file, resolving its
    // own imports from its directory and System32 only, never a general search.
    let module =
        unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32) };
    if module.is_null() {
        let code = unsafe { GetLastError() };
        return Err(format!("loading {} failed with Windows error {code}", dll.display()));
    }
    let symbol = |name: &[u8]| -> Result<*const c_void, String> {
        match unsafe { GetProcAddress(module, name.as_ptr()) } {
            Some(proc) => Ok(proc as *const c_void),
            None => Err(format!("{} does not export {}", dll.display(), String::from_utf8_lossy(&name[..name.len() - 1]))),
        }
    };
    // SAFETY: the package's conpty.h declares these exports with exactly the
    // kernel32 pseudoconsole signatures under Conpty-prefixed names.
    let api = unsafe {
        ConptyApi {
            create: std::mem::transmute::<*const c_void, CreateFn>(symbol(b"ConptyCreatePseudoConsole\0")?),
            resize: std::mem::transmute::<*const c_void, ResizeFn>(symbol(b"ConptyResizePseudoConsole\0")?),
            close: std::mem::transmute::<*const c_void, CloseFn>(symbol(b"ConptyClosePseudoConsole\0")?),
        }
    };
    Ok((api, dll))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forced_inbox_is_reported_as_degraded() {
        let (_, info) = select(ConptyPreference::Inbox);
        assert_eq!(info.kind, ConptyKind::Inbox);
        assert!(!info.graphics_passthrough);
        assert_eq!(info.fallback_reason.as_deref(), Some("forced by CLEAT_CONPTY=inbox"));
        assert!(info.summary().contains("degraded"), "{}", info.summary());
    }

    #[test]
    fn missing_bundle_names_the_missing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = load_bundled(dir.path()).err().expect("empty directory has no bundle");
        assert!(err.contains(CONPTY_DLL), "{err}");

        std::fs::write(dir.path().join(CONPTY_DLL), b"not a dll").expect("write placeholder");
        let err = load_bundled(dir.path()).err().expect("conpty.dll without OpenConsole.exe is not a bundle");
        assert!(err.contains(OPEN_CONSOLE_EXE), "{err}");
    }

    #[test]
    fn staged_bundle_is_selected_beside_the_executable() {
        let (_, info) = select(ConptyPreference::Bundled);
        assert_eq!(
            info.kind,
            ConptyKind::Bundled,
            "the build stages the bundle beside test executables; run tools/prepare-conpty.ps1 ({:?})",
            info.fallback_reason
        );
        assert!(info.graphics_passthrough);
        assert_eq!(info.version.as_deref(), Some(BUNDLED_VERSION));
    }
}
