use std::{
    env, fmt, fs,
    path::{Path, PathBuf},
};

use uuid::Uuid;

use crate::vt::{TerminalColors, VtEngineKind};

const SESSION_ROOT_DIR: &str = "cleat";
const SESSIONS_DIR: &str = "sessions";
pub const DEFAULT_DAEMON_NAME: &str = "default";
pub const DEFAULT_TERMINAL_COLS: u16 = 80;
pub const DEFAULT_TERMINAL_ROWS: u16 = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TerminalSize {
    pub cols: u16,
    pub rows: u16,
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self { cols: DEFAULT_TERMINAL_COLS, rows: DEFAULT_TERMINAL_ROWS }
    }
}

impl fmt::Display for TerminalSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}", self.cols, self.rows)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionMetadata {
    pub id: String,
    pub vt_engine: VtEngineKind,
    pub cwd: Option<PathBuf>,
    pub cmd: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub record: bool,
    pub initial_size: TerminalSize,
    pub colors: TerminalColors,
}

#[derive(Debug, Clone)]
pub struct RuntimeLayout {
    root: PathBuf,
    daemon_name: String,
}

impl RuntimeLayout {
    pub fn discover() -> Self {
        Self {
            root: discover_runtime_root(
                env::var_os("CLEAT_RUNTIME_DIR"),
                env::var_os("XDG_RUNTIME_DIR"),
                env::var_os("TMPDIR"),
                env::temp_dir(),
            ),
            daemon_name: DEFAULT_DAEMON_NAME.to_string(),
        }
    }

    pub fn new(root: PathBuf) -> Self {
        Self { root, daemon_name: DEFAULT_DAEMON_NAME.to_string() }
    }

    pub fn with_daemon(mut self, daemon_name: String) -> Result<Self, String> {
        validate_runtime_name(&daemon_name)?;
        self.daemon_name = daemon_name;
        Ok(self)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn daemon_name(&self) -> &str {
        &self.daemon_name
    }

    pub fn daemon_dir(&self) -> PathBuf {
        self.root.join(&self.daemon_name)
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.daemon_dir().join(SESSIONS_DIR)
    }

    pub fn session_dir(&self, id: &str) -> PathBuf {
        self.sessions_dir().join(id)
    }

    pub fn socket_path(&self) -> PathBuf {
        self.daemon_dir().join("socket")
    }

    pub fn daemon_pid_path(&self) -> PathBuf {
        self.daemon_dir().join("daemon.pid")
    }

    pub fn foreground_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("foreground")
    }

    pub fn ensure_root(&self) -> Result<(), String> {
        fs::create_dir_all(&self.root).map_err(|err| format!("create runtime root {}: {err}", self.root.display()))
    }

    pub fn ensure_daemon_dirs(&self) -> Result<(), String> {
        fs::create_dir_all(self.sessions_dir()).map_err(|err| format!("create daemon directory {}: {err}", self.daemon_dir().display()))
    }

    pub fn create_session(
        &self,
        id: Option<String>,
        vt_engine: VtEngineKind,
        cwd: Option<PathBuf>,
        cmd: Option<String>,
    ) -> Result<SessionMetadata, String> {
        self.ensure_daemon_dirs()?;

        let id = id.unwrap_or_else(|| format!("session-{}", Uuid::new_v4()));
        validate_runtime_name(&id)?;
        let dir = self.session_dir(&id);
        fs::create_dir_all(&dir).map_err(|err| format!("create session dir {}: {err}", dir.display()))?;
        Ok(self.session_metadata(id, vt_engine, cwd, cmd))
    }

    pub fn session_metadata(&self, id: String, vt_engine: VtEngineKind, cwd: Option<PathBuf>, cmd: Option<String>) -> SessionMetadata {
        SessionMetadata {
            id,
            vt_engine,
            cwd,
            cmd,
            tags: Vec::new(),
            record: false,
            initial_size: TerminalSize::default(),
            colors: TerminalColors::default(),
        }
    }

    pub fn remove_session(&self, id: &str) -> Result<(), String> {
        let dir = self.session_dir(id);
        if !dir.exists() {
            return Ok(());
        }
        fs::remove_dir_all(&dir).map_err(|err| format!("remove session dir {}: {err}", dir.display()))
    }
}

pub fn validate_runtime_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("name must not be empty".to_string());
    }
    if name == "." || name == ".." {
        return Err(format!("invalid filesystem-safe name: {name}"));
    }
    if name.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')) {
        Ok(())
    } else {
        Err(format!("invalid filesystem-safe name: {name}"))
    }
}

pub fn normalize_tags(tags: &mut Vec<String>) {
    tags.sort();
    tags.dedup();
}

fn discover_runtime_root(
    explicit_root: Option<std::ffi::OsString>,
    xdg_runtime_dir: Option<std::ffi::OsString>,
    tmpdir: Option<std::ffi::OsString>,
    default_tmp: PathBuf,
) -> PathBuf {
    if let Some(explicit_root) = explicit_root {
        return PathBuf::from(explicit_root);
    }
    if let Some(xdg_runtime_dir) = xdg_runtime_dir {
        return PathBuf::from(xdg_runtime_dir).join(SESSION_ROOT_DIR);
    }
    if let Some(tmpdir) = tmpdir {
        return PathBuf::from(tmpdir).join(format!("{SESSION_ROOT_DIR}-{}", current_uid()));
    }
    default_tmp.join(format!("{SESSION_ROOT_DIR}-{}", current_uid()))
}

#[cfg(unix)]
fn current_uid() -> u32 {
    unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, path::PathBuf};

    use super::discover_runtime_root;

    #[test]
    fn discover_runtime_root_prefers_explicit_root() {
        let root = discover_runtime_root(
            Some(OsString::from("/custom/root")),
            Some(OsString::from("/xdg/runtime")),
            Some(OsString::from("/tmpdir")),
            PathBuf::from("/tmp"),
        );
        assert_eq!(root, PathBuf::from("/custom/root"));
    }

    #[test]
    fn discover_runtime_root_prefers_xdg_before_tmpdir() {
        let root =
            discover_runtime_root(None, Some(OsString::from("/xdg/runtime")), Some(OsString::from("/tmpdir")), PathBuf::from("/tmp"));
        assert_eq!(root, PathBuf::from("/xdg/runtime/cleat"));
    }
}
