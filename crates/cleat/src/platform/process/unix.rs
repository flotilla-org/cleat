use std::{fs, path::Path};

pub fn executable_name() -> &'static str {
    "cleat"
}

pub fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.is_file() && fs::metadata(path).map(|metadata| metadata.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

pub fn terminate_process(pid: i32) {
    // SAFETY: the pid was verified to belong to a cleat process before signaling it.
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
}
