#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use crate::platform::process;

const PID_NAME: &str = "daemon.pid";

pub fn daemon_pid_path(root: &Path, daemon_name: &str) -> PathBuf {
    root.join(daemon_name).join(PID_NAME)
}

pub fn spawn_daemon_process(root: &Path, daemon_name: &str) -> Result<(), String> {
    let exe = resolve_cleat_executable()?;
    let mut command = Command::new(exe);
    command.arg("--runtime-root").arg(root).arg("--server").arg(daemon_name).arg("serve");
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(unix)]
    // The daemon must outlive launchers that clean up their whole process
    // group (agent harnesses, CI runners, and similar supervisors).
    // SAFETY: `setsid` is called in the child after fork and before exec; it
    // does not access shared Rust state.
    unsafe {
        command.pre_exec(|| nix::unistd::setsid().map(|_| ()).map_err(std::io::Error::from));
    }
    command.spawn().map_err(|err| format!("spawn daemon {daemon_name}: {err}"))?;
    Ok(())
}

/// Returns true if the daemon is alive, or if no PID file exists yet because
/// the daemon may still be starting. Returns false only for a definitive stale
/// PID file.
pub fn is_session_daemon_alive(root: &Path, daemon_name: &str) -> bool {
    let pid_path = daemon_pid_path(root, daemon_name);
    let Ok(contents) = fs::read_to_string(&pid_path) else {
        return true;
    };
    let Some(pid) = contents.trim().parse::<i32>().ok() else {
        return false;
    };
    is_expected_cleat_process(pid)
}

pub fn terminate_session_daemon_if_expected(root: &Path, daemon_name: &str) {
    let pid_path = daemon_pid_path(root, daemon_name);
    let Ok(Some(pid)) = fs::read_to_string(&pid_path).map(|value| value.trim().parse::<i32>().ok()) else {
        return;
    };
    if !is_expected_cleat_process(pid) {
        return;
    }
    process::terminate_process(pid);
}

fn is_expected_cleat_process(pid: i32) -> bool {
    let mut sys = System::new();
    let sysinfo_pid = Pid::from(pid as usize);
    sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[sysinfo_pid]), true, ProcessRefreshKind::nothing());
    sys.process(sysinfo_pid).map(|process| process.name().to_string_lossy().contains("cleat")).unwrap_or(false)
}

pub fn resolve_cleat_executable() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_cleat").map(PathBuf::from) {
        return Ok(path);
    }

    let sibling = current_exe_sibling(process::executable_name());
    let path_var = std::env::var_os("PATH").unwrap_or_default();

    resolve_cleat_with_sibling(sibling.as_deref(), &path_var)
}

fn resolve_cleat_with_sibling(sibling: Option<&Path>, path_var: &std::ffi::OsStr) -> Result<PathBuf, String> {
    // Prefer sibling of current executable: strongest "same version" signal.
    if let Some(path) = sibling {
        if process::is_executable_file(path) {
            return Ok(path.to_path_buf());
        }
    }

    for dir in std::env::split_paths(path_var) {
        let candidate = dir.join(process::executable_name());
        if process::is_executable_file(&candidate) {
            return Ok(candidate);
        }
    }

    Err("unable to locate cleat executable on PATH or next to current binary".into())
}

fn current_exe_sibling(name: &str) -> Option<PathBuf> {
    let current_exe = std::env::current_exe().ok()?;
    let current_dir = current_exe.parent()?;
    let candidates = [current_dir.join(name), current_dir.parent().map(|parent| parent.join(name))?];
    candidates.into_iter().find(|candidate| process::is_executable_file(candidate))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::{Mutex, OnceLock},
    };

    use super::resolve_cleat_executable;

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn resolve_cleat_executable_prefers_cargo_bin_env() {
        let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let cleat = temp.path().join(crate::platform::process::executable_name());
        fs::write(&cleat, b"#!/bin/sh\n").expect("write fake cleat");
        let original = std::env::var_os("CARGO_BIN_EXE_cleat");
        std::env::set_var("CARGO_BIN_EXE_cleat", &cleat);

        let resolved = resolve_cleat_executable().expect("resolve cleat");

        match original {
            Some(value) => std::env::set_var("CARGO_BIN_EXE_cleat", value),
            None => std::env::remove_var("CARGO_BIN_EXE_cleat"),
        }
        assert_eq!(resolved, cleat);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_cleat_executable_falls_back_to_path() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("tempdir");
        let bin_dir = temp.path().join("bin");
        fs::create_dir_all(&bin_dir).expect("create bin dir");
        let cleat = bin_dir.join("cleat");
        fs::write(&cleat, b"#!/bin/sh\n").expect("write fake cleat");
        let mut perms = fs::metadata(&cleat).expect("metadata").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&cleat, perms).expect("set executable");

        let resolved = super::resolve_cleat_with_sibling(None, std::ffi::OsStr::new(bin_dir.to_str().unwrap())).expect("resolve from path");

        assert_eq!(resolved, cleat);
        assert!(crate::platform::process::is_executable_file(&cleat));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_cleat_exe_prefers_sibling_over_path() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("tempdir");

        let sibling_dir = temp.path().join("sibling");
        fs::create_dir_all(&sibling_dir).expect("create sibling dir");
        let sibling_exe = sibling_dir.join("cleat");
        fs::write(&sibling_exe, "#!/bin/sh\n").expect("write sibling");
        fs::set_permissions(&sibling_exe, fs::Permissions::from_mode(0o755)).expect("chmod sibling");

        let path_dir = temp.path().join("path-bin");
        fs::create_dir_all(&path_dir).expect("create path dir");
        let path_exe = path_dir.join("cleat");
        fs::write(&path_exe, "#!/bin/sh\n").expect("write path");
        fs::set_permissions(&path_exe, fs::Permissions::from_mode(0o755)).expect("chmod path");

        let result = super::resolve_cleat_with_sibling(Some(&sibling_exe), std::ffi::OsStr::new(path_dir.to_str().unwrap()));

        assert_eq!(result.unwrap(), sibling_exe);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_cleat_exe_falls_back_to_path_when_no_sibling() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("tempdir");

        let path_dir = temp.path().join("path-bin");
        fs::create_dir_all(&path_dir).expect("create path dir");
        let path_exe = path_dir.join("cleat");
        fs::write(&path_exe, "#!/bin/sh\n").expect("write path");
        fs::set_permissions(&path_exe, fs::Permissions::from_mode(0o755)).expect("chmod path");

        let result = super::resolve_cleat_with_sibling(None, std::ffi::OsStr::new(path_dir.to_str().unwrap()));

        assert_eq!(result.unwrap(), path_exe);
    }
}
