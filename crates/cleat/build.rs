use std::{
    env,
    fs,
    ffi::OsStr,
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

    println!("cargo:rustc-env=CLEAT_GHOSTTY_PREFIX={}", install.prefix.display());
    println!("cargo:rustc-link-search=native={}", install.lib_dir.display());

    match install.link_mode {
        LinkMode::Static => {
            if let Some(static_deps) = static_link_dependencies(&repo_root) {
                println!("cargo:rustc-link-search=native={}", static_deps.simdutf_dir.display());
                println!("cargo:rustc-link-search=native={}", static_deps.highway_dir.display());
                println!("cargo:rustc-link-search=native={}", static_deps.cxx_dir.display());
                println!("cargo:rustc-link-search=native={}", static_deps.cxxabi_dir.display());
                println!("cargo:rustc-link-search=native={}", static_deps.unwind_dir.display());
                println!("cargo:rustc-link-lib=static=ghostty-vt");
                println!("cargo:rustc-link-lib=static=simdutf");
                println!("cargo:rustc-link-lib=static=highway");
                println!("cargo:rustc-link-lib=static=c++");
                println!("cargo:rustc-link-lib=static=c++abi");
                println!("cargo:rustc-link-lib=static=unwind");
                println!("cargo:rustc-link-lib=dylib=ubsan");
            } else {
                println!("cargo:rustc-link-lib=dylib=ghostty-vt");
                #[cfg(target_os = "linux")]
                println!("cargo:rustc-link-arg=-Wl,-rpath,{}", install.lib_dir.display());
            }
        }
        LinkMode::Shared => {
            println!("cargo:rustc-link-lib=dylib=ghostty-vt");
            #[cfg(target_os = "linux")]
            println!("cargo:rustc-link-arg=-Wl,-rpath,{}", install.lib_dir.display());
        }
    }
}

struct GhosttyInstall {
    prefix: PathBuf,
    lib_dir: PathBuf,
    link_mode: LinkMode,
}

#[derive(Clone, Copy)]
enum LinkMode {
    Static,
    Shared,
}

struct StaticLinkDependencies {
    simdutf_dir: PathBuf,
    highway_dir: PathBuf,
    cxx_dir: PathBuf,
    cxxabi_dir: PathBuf,
    unwind_dir: PathBuf,
}

fn ghostty_install(repo_root: &Path) -> Result<GhosttyInstall, String> {
    let prefix = ghostty_prefix(repo_root)?;
    if !prefix.exists() {
        return Err(format!(
            "ghostty-vt feature requires a Ghostty install prefix at {}",
            prefix.display()
        ));
    }

    let include_dir = prefix.join("include");
    if !include_dir.exists() {
        return Err(format!(
            "ghostty-vt feature requires Ghostty headers under {}",
            include_dir.display()
        ));
    }

    let header = include_dir.join("ghostty").join("vt.h");
    if !header.exists() {
        return Err(format!(
            "ghostty-vt feature requires ghostty headers at {}",
            header.display()
        ));
    }

    let lib_dir = prefix.join("lib");
    if !lib_dir.exists() {
        return Err(format!(
            "ghostty-vt feature requires a Ghostty library directory at {}",
            lib_dir.display()
        ));
    }

    let static_lib = lib_dir.join("libghostty-vt.a");
    if static_lib.exists() {
        return Ok(GhosttyInstall {
            prefix,
            lib_dir,
            link_mode: LinkMode::Static,
        });
    }

    let shared_lib = lib_dir.join(shared_library_filename());
    if shared_lib.exists() {
        return Ok(GhosttyInstall {
            prefix,
            lib_dir,
            link_mode: LinkMode::Shared,
        });
    }

    Err(format!(
        "ghostty-vt feature requires {} or {} under {}",
        static_lib.display(),
        shared_lib.display(),
        lib_dir.display()
    ))
}

fn ghostty_prefix(repo_root: &Path) -> Result<PathBuf, String> {
    if let Some(explicit) = env::var_os("CLEAT_GHOSTTY_PREFIX").map(PathBuf::from) {
        return Ok(explicit);
    }

    Ok(repo_root.join(".tools/ghostty-install"))
}

fn repo_root() -> Result<PathBuf, String> {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR")
            .ok_or_else(|| "CARGO_MANIFEST_DIR is not set while resolving the repository root".to_string())?,
    );
    manifest_dir.parent().and_then(|path| path.parent()).map(Path::to_path_buf).ok_or_else(|| {
        format!(
            "could not determine repository root from {}",
            manifest_dir.display()
        )
    })
}

fn static_link_dependencies(repo_root: &Path) -> Option<StaticLinkDependencies> {
    let cache_root = repo_root.join(".tools/ghostty-src/.zig-cache/o");
    let simdutf_dir = find_library_dir(&cache_root, "libsimdutf.a")?;
    let highway_dir = find_library_dir(&cache_root, "libhighway.a")?;
    let cxx_dir = find_library_dir(&zig_cache_root(), "libc++.a")?;
    let cxxabi_dir = find_library_dir(&zig_cache_root(), "libc++abi.a")?;
    let unwind_dir = find_library_dir(&zig_cache_root(), "libunwind.a")?;

    Some(StaticLinkDependencies {
        simdutf_dir,
        highway_dir,
        cxx_dir,
        cxxabi_dir,
        unwind_dir,
    })
}

fn zig_cache_root() -> PathBuf {
    let home = env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
    home.join(".cache/zig/o")
}

fn find_library_dir(root: &Path, library_name: &str) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir).ok()?;
        for entry in entries {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|name| name == OsStr::new(library_name)) {
                return path.parent().map(Path::to_path_buf);
            }
        }
    }

    None
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
