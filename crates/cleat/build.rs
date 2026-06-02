use std::{
    env,
    path::{Path, PathBuf},
};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    if env::var_os("CARGO_FEATURE_GHOSTTY_VT").is_none() {
        println!("cargo:rustc-env=CLEAT_FUNCTIONAL_VT_AVAILABLE=0");
        println!("cargo:warning=building cleat without ghostty-vt; this binary is non-functional for real terminal usage");
        println!("cargo:warning=Ghostty is currently the only functional VT engine");
        println!("cargo:warning=passthrough is a placeholder/testing engine only");
        if cfg!(target_os = "windows") {
            println!("cargo:warning=run ./tools/prepare-ghostty-vt.ps1 and rebuild with --features ghostty-vt for a functional binary");
        } else {
            println!("cargo:warning=run ./tools/prepare-ghostty-vt.sh and rebuild with --features ghostty-vt for a functional binary");
        }
        return;
    }
    if !ghostty_supported_target() {
        panic!("ghostty-vt feature requires Linux, macOS, or Windows");
    }

    println!("cargo:rerun-if-env-changed=CLEAT_GHOSTTY_PREFIX");
    println!("cargo:rustc-env=CLEAT_FUNCTIONAL_VT_AVAILABLE=1");

    let repo_root = repo_root().unwrap_or_else(|err| panic!("{err}"));
    let install = ghostty_install(&repo_root).unwrap_or_else(|err| panic!("{err}"));
    watch_ghostty_install(&install.prefix);
    if cfg!(target_os = "windows") && install.link_mode == LinkMode::Dynamic {
        copy_windows_runtime_dll(&install).unwrap_or_else(|err| panic!("{err}"));
    }

    println!("cargo:rustc-env=CLEAT_GHOSTTY_PREFIX={}", install.prefix.display());
    println!("cargo:rustc-link-search=native={}", install.lib_dir.display());
    match install.link_mode {
        LinkMode::Static => println!("cargo:rustc-link-lib=static={}", static_link_name()),
        LinkMode::Dynamic => println!("cargo:rustc-link-lib=dylib=ghostty-vt"),
    }
    if install.link_mode == LinkMode::Dynamic && cfg!(any(target_os = "linux", target_os = "macos")) {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", install.lib_dir.display());
    }
}

struct GhosttyInstall {
    prefix: PathBuf,
    lib_dir: PathBuf,
    link_mode: LinkMode,
    shared_lib: Option<PathBuf>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LinkMode {
    Static,
    Dynamic,
}

fn ghostty_install(repo_root: &Path) -> Result<GhosttyInstall, String> {
    let prefix = ghostty_prefix(repo_root)?;
    if !prefix.exists() {
        return Err(missing_ghostty_install_message(&prefix, format!("missing Ghostty install prefix at {}", prefix.display())));
    }

    let include_dir = prefix.join("include");
    if !include_dir.exists() {
        return Err(missing_ghostty_install_message(&prefix, format!("missing Ghostty headers under {}", include_dir.display())));
    }

    let header = include_dir.join("ghostty").join("vt.h");
    if !header.exists() {
        return Err(missing_ghostty_install_message(&prefix, format!("missing ghostty header at {}", header.display())));
    }

    let lib_dir = prefix.join("lib");
    if !lib_dir.exists() {
        return Err(missing_ghostty_install_message(&prefix, format!("missing Ghostty library directory at {}", lib_dir.display())));
    }

    let shared_lib = shared_library_path(&prefix, &lib_dir);
    if cfg!(target_os = "windows") {
        let import_lib = lib_dir.join(import_library_filename());
        if !shared_lib.exists() {
            return Err(missing_ghostty_install_message(&prefix, format!("missing ghostty DLL at {}", shared_lib.display())));
        }
        if !import_lib.exists() {
            return Err(missing_ghostty_install_message(&prefix, format!("missing ghostty import library at {}", import_lib.display())));
        }
        return Ok(GhosttyInstall { prefix, lib_dir, link_mode: LinkMode::Dynamic, shared_lib: Some(shared_lib) });
    }

    let static_lib = lib_dir.join(static_library_filename());
    if static_lib.exists() {
        return Ok(GhosttyInstall { prefix, lib_dir, link_mode: LinkMode::Static, shared_lib: None });
    }

    if !shared_lib.exists() {
        return Err(missing_ghostty_install_message(
            &prefix,
            format!("missing ghostty library; expected {} or {}", static_lib.display(), shared_lib.display()),
        ));
    }

    Ok(GhosttyInstall { prefix, lib_dir, link_mode: LinkMode::Dynamic, shared_lib: Some(shared_lib) })
}

fn ghostty_prefix(repo_root: &Path) -> Result<PathBuf, String> {
    if let Some(explicit) = env::var_os("CLEAT_GHOSTTY_PREFIX").map(PathBuf::from) {
        return Ok(explicit);
    }

    Ok(repo_root.join(".tools/ghostty-install"))
}

fn repo_root() -> Result<PathBuf, String> {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").ok_or_else(|| "CARGO_MANIFEST_DIR is not set while resolving the repository root".to_string())?,
    );
    manifest_dir
        .parent()
        .and_then(|path| path.parent())
        .map(Path::to_path_buf)
        .ok_or_else(|| format!("could not determine repository root from {}", manifest_dir.display()))
}

fn watch_ghostty_install(prefix: &Path) {
    let lib_dir = prefix.join("lib");
    let header = prefix.join("include/ghostty/vt.h");
    println!("cargo:rerun-if-changed={}", header.display());
    println!("cargo:rerun-if-changed={}", lib_dir.join(static_library_filename()).display());
    println!("cargo:rerun-if-changed={}", lib_dir.join(shared_library_filename()).display());
    if cfg!(target_os = "windows") {
        println!("cargo:rerun-if-changed={}", prefix.join("bin").join(shared_library_filename()).display());
        println!("cargo:rerun-if-changed={}", lib_dir.join(import_library_filename()).display());
    }
}

fn missing_ghostty_install_message(prefix: &Path, reason: String) -> String {
    if cfg!(target_os = "windows") {
        format!(
            "ghostty-vt feature requires a prepared Ghostty install prefix. {reason}.\n\
run ./tools/prepare-ghostty-vt.ps1 and retry with:\n\
$env:CLEAT_GHOSTTY_PREFIX=\"{}\"; $env:PATH=\"{}\\bin;{}\\lib;$env:PATH\"; cargo build -p cleat --locked --features ghostty-vt",
            prefix.display(),
            prefix.display(),
            prefix.display()
        )
    } else {
        format!(
            "ghostty-vt feature requires a prepared Ghostty install prefix. {reason}.\n\
run ./tools/prepare-ghostty-vt.sh and retry with:\n\
CLEAT_GHOSTTY_PREFIX=\"{}\" cargo build -p cleat --locked --features ghostty-vt",
            prefix.display()
        )
    }
}

fn static_library_filename() -> &'static str {
    if cfg!(target_os = "windows") {
        "ghostty-vt-static.lib"
    } else {
        "libghostty-vt.a"
    }
}

fn static_link_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "ghostty-vt-static"
    } else {
        "ghostty-vt"
    }
}

fn shared_library_filename() -> &'static str {
    if cfg!(target_os = "linux") {
        "libghostty-vt.so"
    } else if cfg!(target_os = "macos") {
        "libghostty-vt.dylib"
    } else if cfg!(target_os = "windows") {
        "ghostty-vt.dll"
    } else {
        panic!("ghostty-vt feature requires Linux, macOS, or Windows")
    }
}

fn shared_library_path(prefix: &Path, lib_dir: &Path) -> PathBuf {
    let lib_path = lib_dir.join(shared_library_filename());
    if cfg!(target_os = "windows") {
        let bin_path = prefix.join("bin").join(shared_library_filename());
        if bin_path.exists() {
            return bin_path;
        }
    }
    lib_path
}

fn import_library_filename() -> &'static str {
    "ghostty-vt.lib"
}

fn copy_windows_runtime_dll(install: &GhosttyInstall) -> Result<(), String> {
    let dll = install.shared_lib.as_ref().ok_or_else(|| "dynamic Ghostty install has no DLL path".to_string())?;
    let profile_dir = profile_dir_from_out_dir()?;
    let target = profile_dir.join(shared_library_filename());
    std::fs::copy(dll, &target).map_err(|err| format!("copy {} to {}: {err}", dll.display(), target.display()))?;
    println!("cargo:rerun-if-changed={}", dll.display());
    Ok(())
}

fn profile_dir_from_out_dir() -> Result<PathBuf, String> {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").ok_or_else(|| "OUT_DIR is not set".to_string())?);
    out_dir
        .ancestors()
        .nth(3)
        .map(Path::to_path_buf)
        .ok_or_else(|| format!("could not determine Cargo profile directory from OUT_DIR={}", out_dir.display()))
}

fn ghostty_supported_target() -> bool {
    cfg!(any(target_os = "linux", target_os = "macos", target_os = "windows"))
}
