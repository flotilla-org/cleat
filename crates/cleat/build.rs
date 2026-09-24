use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    emit_build_info();
    stage_bundled_conpty();

    if env::var_os("CARGO_FEATURE_GHOSTTY_VT").is_none() {
        println!("cargo:rustc-env=CLEAT_FUNCTIONAL_VT_AVAILABLE=0");
        println!("cargo:warning=building cleat without ghostty-vt (--no-default-features); this binary is non-functional for real terminal usage");
        println!("cargo:warning=Ghostty is currently the only functional VT engine");
        println!("cargo:warning=passthrough is a placeholder/testing engine only");
        if cfg!(target_os = "windows") {
            println!("cargo:warning=run ./tools/prepare-ghostty-vt.ps1 and rebuild with default features for a functional binary");
        } else {
            println!("cargo:warning=run ./tools/prepare-ghostty-vt.sh and rebuild with default features for a functional binary");
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

// Watch both repository state and tracked files so incremental builds refresh
// the identity after commits, checkouts and edits, including in Git worktrees.
fn emit_build_info() {
    let root = repo_root().expect("repository layout");
    let git = |args: &[&str]| -> Option<String> {
        let output = Command::new("git").current_dir(&root).args(args).output().ok()?;
        output.status.success().then(|| String::from_utf8_lossy(&output.stdout).trim_end().to_owned())
    };
    let sha = git(&["rev-parse", "--verify", "HEAD"]);
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).map(|status| !status.is_empty());
    // Reftable stores ref updates in its table directory instead of loose refs.
    for name in ["HEAD", "index", "packed-refs", "reftable"].into_iter().map(str::to_owned).chain(git(&["symbolic-ref", "-q", "HEAD"])) {
        if let Some(path) = git(&["rev-parse", "--git-path", &name]) {
            let mut path = root.join(path);
            // A packed branch has no loose ref yet. Watch its nearest existing
            // directory so a ref-only update that creates it invalidates Cargo,
            // including when git --git-path points into a worktree's common dir.
            if name.starts_with("refs/") {
                while !path.exists() && path.pop() {}
            }
            if path.exists() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }
    if let Some(files) = git(&["ls-files", "-z"]) {
        for file in files.split('\0').filter(|file| !file.is_empty() && !file.contains(['\n', '\r'])) {
            println!("cargo:rerun-if-changed={}", root.join(file).display());
        }
    }
    println!("cargo:rustc-env=CLEAT_GIT_SHA={}", sha.as_deref().unwrap_or("unknown"));
    println!("cargo:rustc-env=CLEAT_GIT_DIRTY={}", dirty.map(|value| value.to_string()).unwrap_or_else(|| "unknown".into()));
    for name in ["PROFILE", "OPT_LEVEL", "TARGET"] {
        println!("cargo:rustc-env=CLEAT_BUILD_{name}={}", env::var(name).unwrap_or_else(|_| "unknown".into()));
    }
}

/// Stage the pinned bundled ConPTY (`conpty.dll`, `OpenConsole.exe` and its
/// licence) beside the Windows executables this build produces (ADR 0006).
/// Without a prepared package the build still succeeds: sessions fall back to
/// the inbox ConPTY and report the degradation.
fn stage_bundled_conpty() {
    let root = repo_root().expect("repository layout");
    let pin_path = root.join("tools").join("conpty.toml");
    println!("cargo:rerun-if-changed={}", pin_path.display());
    let version = conpty_pin_version(&pin_path).unwrap_or_else(|err| panic!("{err}"));
    println!("cargo:rustc-env=CLEAT_CONPTY_VERSION={version}");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let Some((runtime, host)) = conpty_arch_dirs(&arch) else {
        println!("cargo:warning=no bundled ConPTY for target architecture {arch}; Windows sessions will use the inbox ConPTY");
        return;
    };
    let package = root.join(".tools").join("conpty").join(&version);
    let mut watched = package.clone();
    while !watched.exists() && watched.pop() {}
    println!("cargo:rerun-if-changed={}", watched.display());
    let licence = root.join("tools").join("conpty-LICENSE.txt");
    println!("cargo:rerun-if-changed={}", licence.display());
    let files = [
        (package.join("runtimes").join(runtime).join("native").join("conpty.dll"), "conpty.dll"),
        (package.join("build").join("native").join("runtimes").join(host).join("OpenConsole.exe"), "OpenConsole.exe"),
        (licence, "conpty-LICENSE.txt"),
    ];

    let profile = profile_dir_from_out_dir().unwrap_or_else(|err| panic!("{err}"));
    // Test and example executables run from deps/ and examples/, and load
    // conpty.dll only from their own directory.
    let destinations = [profile.clone(), profile.join("deps"), profile.join("examples")];
    let missing: Vec<String> =
        files.iter().filter(|(source, _)| !source.is_file()).map(|(source, _)| source.display().to_string()).collect();
    if !missing.is_empty() {
        println!(
            "cargo:warning=bundled ConPTY {version} is not prepared (missing {}); Windows sessions will fall back to the inbox ConPTY, which drops Kitty graphics and sixel. Run tools/prepare-conpty.ps1",
            missing.join(", ")
        );
        // Never leave another version's bundle to be used as if it were the pin.
        for destination in &destinations {
            for (_, name) in &files {
                let _ = std::fs::remove_file(destination.join(name));
            }
        }
        return;
    }
    for destination in &destinations {
        std::fs::create_dir_all(destination).unwrap_or_else(|err| panic!("create {}: {err}", destination.display()));
        for (source, name) in &files {
            copy_if_changed(source, &destination.join(name)).unwrap_or_else(|err| panic!("{err}"));
        }
    }
}

fn conpty_pin_version(path: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(path).map_err(|err| format!("read {}: {err}", path.display()))?;
    let mut section = "";
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if let Some(name) = line.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
            section = name.trim();
        } else if section == "conpty" {
            if let Some((key, value)) = line.split_once('=') {
                if key.trim() == "version" {
                    return Ok(value.trim().trim_matches('"').to_string());
                }
            }
        }
    }
    Err(format!("missing [conpty].version in {}", path.display()))
}

fn conpty_arch_dirs(arch: &str) -> Option<(&'static str, &'static str)> {
    match arch {
        "x86_64" => Some(("win-x64", "x64")),
        "aarch64" => Some(("win-arm64", "arm64")),
        "x86" => Some(("win-x86", "x86")),
        _ => None,
    }
}

// Skipping identical files lets a build succeed while a daemon started from
// this target directory still has conpty.dll and OpenConsole.exe open.
fn copy_if_changed(source: &Path, target: &Path) -> Result<(), String> {
    let bytes = std::fs::read(source).map_err(|err| format!("read {}: {err}", source.display()))?;
    if std::fs::read(target).is_ok_and(|existing| existing == bytes) {
        return Ok(());
    }
    std::fs::write(target, &bytes).map_err(|err| format!("copy {} to {}: {err}", source.display(), target.display()))
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
    if !shared_lib.exists() {
        if static_lib.exists() {
            return Ok(GhosttyInstall { prefix, lib_dir, link_mode: LinkMode::Static, shared_lib: None });
        }
        return Err(missing_ghostty_install_message(
            &prefix,
            format!("missing ghostty library; expected {} or {}", shared_lib.display(), static_lib.display()),
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
$env:CLEAT_GHOSTTY_PREFIX=\"{}\"; $env:PATH=\"{}\\bin;{}\\lib;$env:PATH\"; cargo build -p cleat --locked",
            prefix.display(),
            prefix.display(),
            prefix.display()
        )
    } else {
        format!(
            "ghostty-vt feature requires a prepared Ghostty install prefix. {reason}.\n\
run ./tools/prepare-ghostty-vt.sh and retry with:\n\
CLEAT_GHOSTTY_PREFIX=\"{}\" cargo build -p cleat --locked",
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
