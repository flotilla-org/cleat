use std::path::Path;

pub(crate) fn executable_name() -> &'static str {
    if cfg!(windows) {
        "cleat.exe"
    } else {
        "cleat"
    }
}

pub(crate) fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

pub(crate) fn terminate_process(_pid: i32) {
    // Placeholder for a future Windows backend. Default non-Unix builds can
    // compile, but daemon process termination is not implemented there yet.
}
