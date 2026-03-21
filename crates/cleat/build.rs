use std::{
    env,
    path::{Path, PathBuf},
};

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("ghostty-vt feature requires Linux or macOS");

fn main() {
    if env::var_os("CARGO_FEATURE_GHOSTTY_VT").is_none() {
        return;
    }

    println!("cargo:rerun-if-env-changed=CLEAT_GHOSTTY_PREFIX");
    println!("cargo:rerun-if-changed=build.rs");

    let repo_root = repo_root().unwrap_or_else(|err| panic!("{err}"));
    let install = ghostty_install(&repo_root).unwrap_or_else(|err| panic!("{err}"));
    watch_ghostty_install(&install.prefix);

    println!("cargo:rustc-env=CLEAT_GHOSTTY_PREFIX={}", install.prefix.display());
    println!("cargo:rustc-link-search=native={}", install.lib_dir.display());
    println!("cargo:rustc-link-lib=dylib=ghostty-vt");
    #[cfg(target_os = "linux")]
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", install.lib_dir.display());
}

struct GhosttyInstall {
    prefix: PathBuf,
    lib_dir: PathBuf,
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

    let shared_lib = lib_dir.join(shared_library_filename());
    if !shared_lib.exists() {
        return Err(missing_ghostty_install_message(&prefix, format!("missing shared ghostty library at {}", shared_lib.display())));
    }

    Ok(GhosttyInstall { prefix, lib_dir })
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
    println!("cargo:rerun-if-changed={}", lib_dir.join(shared_library_filename()).display());
}

fn missing_ghostty_install_message(prefix: &Path, reason: String) -> String {
    format!(
        "ghostty-vt feature requires a prepared Ghostty install prefix. {reason}.\n\
run ./tools/prepare-ghostty-vt.sh and retry with:\n\
LD_LIBRARY_PATH=\"{}/lib\" cargo build -p cleat --locked --features ghostty-vt",
        prefix.display()
    )
}

fn shared_library_filename() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        "libghostty-vt.so"
    }
    #[cfg(target_os = "macos")]
    {
        "libghostty-vt.dylib"
    }
}
