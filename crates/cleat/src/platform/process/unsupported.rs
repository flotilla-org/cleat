use std::path::Path;

pub fn executable_name() -> &'static str {
    if cfg!(windows) {
        "cleat.exe"
    } else {
        "cleat"
    }
}

pub fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

pub fn terminate_process(_pid: i32) {}
