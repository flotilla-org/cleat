use std::{
    env, fs,
    path::{Path, PathBuf},
};

use uuid::Uuid;

use crate::vt::VtEngineKind;

const SESSION_ROOT_DIR: &str = "cleat";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMetadata {
    pub id: String,
    pub vt_engine: VtEngineKind,
    pub cwd: Option<PathBuf>,
    pub cmd: Option<String>,
    pub record: bool,
}

#[derive(Debug, Clone)]
pub struct RuntimeLayout {
    root: PathBuf,
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
        }
    }

    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn ensure_root(&self) -> Result<(), String> {
        fs::create_dir_all(&self.root).map_err(|err| format!("create runtime root {}: {err}", self.root.display()))
    }

    pub fn create_session(
        &self,
        id: Option<String>,
        vt_engine: VtEngineKind,
        cwd: Option<PathBuf>,
        cmd: Option<String>,
    ) -> Result<SessionMetadata, String> {
        self.ensure_root()?;

        let id = id.unwrap_or_else(|| format!("session-{}", Uuid::new_v4()));
        let dir = self.root.join(&id);
        fs::create_dir_all(&dir).map_err(|err| format!("create session dir {}: {err}", dir.display()))?;
        Ok(SessionMetadata { id, vt_engine, cwd, cmd, record: false })
    }

    pub fn remove_session(&self, id: &str) -> Result<(), String> {
        let dir = self.root.join(id);
        if !dir.exists() {
            return Ok(());
        }
        fs::remove_dir_all(&dir).map_err(|err| format!("remove session dir {}: {err}", dir.display()))
    }
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
