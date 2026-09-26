//! Local output dependency admission. A lease lives exactly as long as its
//! output subscription; OS file locks also release leases on daemon death.
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::runtime::{ambient_session_coordinates, RuntimeLayout};

pub(crate) const HEADER: &str = "x-cleat-output-context";

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct SessionIdentity {
    runtime_root: PathBuf,
    daemon: String,
    session: String,
}

impl SessionIdentity {
    fn new(layout: &RuntimeLayout, session: &str) -> Result<Self, String> {
        crate::runtime::validate_runtime_name(session)?;
        let directory = fs::canonicalize(layout.daemon_dir()).map_err(|e| format!("resolve output daemon: {e}"))?;
        let daemon = directory.file_name().and_then(|name| name.to_str()).ok_or("invalid output daemon directory")?;
        Ok(Self {
            runtime_root: directory.parent().ok_or("missing output runtime root")?.to_owned(),
            daemon: daemon.to_owned(),
            session: session.to_owned(),
        })
    }

    fn canonical(self) -> Result<Self, String> {
        let layout = RuntimeLayout::new(self.runtime_root).with_daemon(self.daemon)?;
        if !layout.session_dir(&self.session).is_dir() {
            return Err("output source session does not exist".into());
        }
        Self::new(&layout, &self.session)
    }
}

/// `external` is an explicit assertion by an upgraded local client, never the
/// default for omitted metadata. Forwarding across hosts is not supported.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum OutputContext {
    External,
    Remote,
    Session { source: SessionIdentity },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Declaration {
    version: u32,
    context: OutputContext,
}

pub(crate) fn client_header() -> Result<String, String> {
    let context = if std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_CLIENT").is_some() {
        OutputContext::Remote
    } else {
        match ambient_session_coordinates()? {
            Some(source) => OutputContext::Session {
                source: SessionIdentity::new(
                    &RuntimeLayout::new(source.runtime_root().to_owned()).with_daemon(
                        std::env::var(crate::runtime::OUTPUT_DAEMON_ENV).map_err(|_| {
                            "physical source daemon coordinate missing; restart the containing session with an upgraded daemon"
                        })?,
                    )?,
                    source.session_id(),
                )?,
            },
            None => {
                if std::env::var_os(crate::runtime::AMBIENT_SESSION_ENV).is_some() {
                    return Err("empty or invalid CLEAT_SESSION output context".into());
                }
                OutputContext::External
            }
        }
    };
    serde_json::to_string(&Declaration { version: 1, context }).map_err(|e| e.to_string())
}

pub(crate) fn verify(
    request: &crate::http_uds::HttpRequest,
    stream: &crate::platform::ipc::SessionStream,
) -> Result<OutputContext, String> {
    let value = request.headers().get(HEADER).ok_or("output context required; upgrade the cleat client (output admission v1)")?;
    let declaration: Declaration = serde_json::from_slice(value.as_bytes()).map_err(|e| format!("invalid output context: {e}"))?;
    if declaration.version != 1 {
        return Err("unsupported output admission version; upgrade the cleat client".into());
    }
    let context = match declaration.context {
        OutputContext::Session { source } => OutputContext::Session { source: source.canonical()? },
        OutputContext::Remote => return Err("remote output relationships are unsupported by local cycle admission".into()),
        external => external,
    };
    verify_peer(&context, stream)?;
    Ok(context)
}

#[cfg(target_os = "linux")]
fn verify_peer(context: &OutputContext, stream: &crate::platform::ipc::SessionStream) -> Result<(), String> {
    use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
    let peer = getsockopt(stream, PeerCredentials).map_err(|e| format!("verify output peer: {e}"))?;
    let bytes = fs::read(format!("/proc/{}/environ", peer.pid())).map_err(|e| format!("verify output peer environment: {e}"))?;
    let environment: std::collections::HashMap<_, _> = bytes
        .split(|b| *b == 0)
        .filter_map(|entry| {
            let entry = std::str::from_utf8(entry).ok()?;
            entry.split_once('=')
        })
        .collect();
    if let Some(session) = environment.get(crate::runtime::AMBIENT_SESSION_ENV) {
        let root = environment.get(crate::runtime::RUNTIME_DIR_ENV).ok_or("peer has incomplete session coordinates")?;
        let daemon = environment
            .get(crate::runtime::OUTPUT_DAEMON_ENV)
            .ok_or("peer lacks physical daemon coordinate; restart the containing session")?;
        let source = SessionIdentity::new(&RuntimeLayout::new(PathBuf::from(root)).with_daemon((*daemon).to_owned())?, session)?;
        if *context != (OutputContext::Session { source }) {
            return Err("output context disagrees with local peer session".into());
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn verify_peer(_context: &OutputContext, _stream: &crate::platform::ipc::SessionStream) -> Result<(), String> {
    // Other platforms enforce the explicit upgraded-client declaration contract.
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct Edge {
    source: SessionIdentity,
    target: SessionIdentity,
}

pub(crate) struct OutputLease {
    // Keep the lock held until after unlinking, including on failed admission.
    _file: File,
    path: PathBuf,
}

impl Drop for OutputLease {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn coordinator_dir() -> Result<PathBuf, String> {
    #[cfg(unix)]
    let path = PathBuf::from(format!("/tmp/cleat-output-{}", unsafe { libc::geteuid() }));
    #[cfg(windows)]
    let path = PathBuf::from(std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA required for output coordination")?).join("cleat-output");
    #[cfg(windows)]
    fs::create_dir_all(&path).map_err(|e| format!("create output coordinator: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
        match fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("create output coordinator: {e}")),
        }
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|e| format!("open output coordinator: {e}"))?;
        let metadata = directory.metadata().map_err(|e| e.to_string())?;
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err("output coordinator must be a directory owned by the current user".into());
        }
        directory.set_permissions(fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    }
    Ok(path)
}

pub(crate) fn admit(context: &OutputContext, layout: &RuntimeLayout, session: &str) -> Result<Option<OutputLease>, String> {
    let OutputContext::Session { source } = context else { return Ok(None) };
    let target = SessionIdentity::new(layout, session)?;
    admit_at(&coordinator_dir()?, Edge { source: source.clone(), target }).map(Some)
}

fn admit_at(directory: &Path, edge: Edge) -> Result<OutputLease, String> {
    let operation = || -> Result<OutputLease, Box<dyn std::error::Error>> {
        let lock = OpenOptions::new().create(true).truncate(false).read(true).write(true).open(directory.join("admission.lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Err("coordinator busy; retry output admission".into()),
            Err(e) => return Err(e.into()),
        }
        let mut edges = Vec::new();
        for entry in fs::read_dir(directory)? {
            let path = entry?.path();
            if path.extension().is_none_or(|extension| extension != "lease") {
                continue;
            }
            let mut file = match OpenOptions::new().read(true).write(true).open(&path) {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            match file.try_lock() {
                Ok(()) => {
                    let _ = fs::remove_file(path);
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    let mut bytes = Vec::new();
                    file.read_to_end(&mut bytes)?;
                    edges.push(serde_json::from_slice::<Edge>(&bytes)?);
                }
                Err(e) => return Err(e.into()),
            }
        }
        if closes_cycle(&edges, &edge) {
            return Err("attachment cycle rejected before output or resize".into());
        }
        let path = directory.join(format!("{}.lease", uuid::Uuid::new_v4()));
        let mut lease = OutputLease { _file: OpenOptions::new().create_new(true).read(true).write(true).open(&path)?, path };
        serde_json::to_writer(&mut lease._file, &edge)?;
        lease._file.flush()?;
        // Shared locks permit readers on Windows too. The coordinator lock
        // prevents another admission observing the write before this lock.
        lease._file.lock_shared()?;
        Ok(lease)
    };
    operation().map_err(|e| format!("output admission: {e}"))
}

fn closes_cycle(edges: &[Edge], new: &Edge) -> bool {
    let mut pending = vec![&new.target];
    let mut visited = HashSet::new();
    while let Some(node) = pending.pop() {
        if node == &new.source {
            return true;
        }
        if visited.insert(node) {
            pending.extend(edges.iter().filter(|edge| &edge.source == node).map(|edge| &edge.target));
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(daemon: &str, session: &str) -> SessionIdentity {
        SessionIdentity { runtime_root: PathBuf::from("/runtime"), daemon: daemon.into(), session: session.into() }
    }

    fn edge(source: &str, target: &str) -> Edge {
        Edge { source: identity("default", source), target: identity("default", target) }
    }

    #[test]
    fn leases_reject_self_and_transitive_cycles_and_release_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        assert!(admit_at(dir.path(), edge("a", "a")).is_err());
        let ab = admit_at(dir.path(), edge("a", "b")).unwrap();
        let bc = admit_at(dir.path(), edge("b", "c")).unwrap();
        assert!(admit_at(dir.path(), edge("c", "a")).is_err());
        // A duplicate subscription must not disappear when the first drops.
        let second_ab = admit_at(dir.path(), edge("a", "b")).unwrap();
        drop(ab);
        assert!(admit_at(dir.path(), edge("c", "a")).is_err());
        drop(second_ab);
        let ca = admit_at(dir.path(), edge("c", "a")).unwrap();
        drop((bc, ca));
    }

    #[test]
    fn separate_daemons_and_roots_do_not_alias_equal_session_names() {
        let dir = tempfile::tempdir().unwrap();
        let source = identity("one", "same");
        let target = identity("two", "same");
        let _lease = admit_at(dir.path(), Edge { source: source.clone(), target: target.clone() }).unwrap();
        assert!(admit_at(dir.path(), Edge { source: target.clone(), target: source }).is_err());
        let mut elsewhere = target.clone();
        elsewhere.runtime_root = PathBuf::from("/other-runtime");
        assert!(admit_at(dir.path(), Edge { source: target, target: elsewhere }).is_ok());
    }

    #[test]
    fn stale_unlocked_leases_are_reaped_even_if_incomplete() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("dead.lease"), b"interrupted write").unwrap();
        assert!(admit_at(dir.path(), edge("a", "b")).is_ok());
        assert!(!dir.path().join("dead.lease").exists());
    }

    #[test]
    fn busy_coordinator_rejects_promptly_without_leaking_an_edge() {
        let dir = tempfile::tempdir().unwrap();
        let lock = File::create(dir.path().join("admission.lock")).unwrap();
        lock.lock().unwrap();
        let error = admit_at(dir.path(), edge("a", "b")).err().expect("busy coordinator must reject");
        assert!(error.contains("coordinator busy"), "{error}");
        drop(lock);
        assert!(admit_at(dir.path(), edge("b", "a")).is_ok());
    }

    #[test]
    fn simultaneous_opposing_edges_admit_exactly_one() {
        let dir = tempfile::tempdir().unwrap();
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                barrier.wait();
                admit_at(dir.path(), edge("a", "b"))
            });
            let second = scope.spawn(|| {
                barrier.wait();
                admit_at(dir.path(), edge("b", "a"))
            });
            let first = first.join().unwrap();
            let second = second.join().unwrap();
            assert_ne!(first.is_ok(), second.is_ok());
        });
    }

    #[cfg(unix)]
    #[test]
    fn runtime_and_daemon_symlinks_have_one_identity() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("default@1")).unwrap();
        std::os::unix::fs::symlink("default@1", dir.path().join("default")).unwrap();
        std::os::unix::fs::symlink("default@1", dir.path().join("alias")).unwrap();
        let link = dir.path().join("root-alias");
        std::os::unix::fs::symlink(dir.path(), &link).unwrap();
        let source = SessionIdentity::new(&RuntimeLayout::new(dir.path().to_owned()), "a").unwrap();
        let target = SessionIdentity::new(&RuntimeLayout::new(link).with_daemon("alias".into()).unwrap(), "a").unwrap();
        assert_eq!(source, target);
        fs::create_dir(dir.path().join("default@2")).unwrap();
        let successor =
            SessionIdentity::new(&RuntimeLayout::new(dir.path().to_owned()).with_daemon("default@2".into()).unwrap(), "a").unwrap();
        assert_ne!(source, successor, "physical generations are distinct daemons");
    }
}

#[cfg(test)]
mod process_tests {
    use super::*;

    #[test]
    fn lease_owner_child() {
        let Some(directory) = std::env::var_os("CLEAT_TEST_LEASE_DIRECTORY") else { return };
        let directory = PathBuf::from(directory);
        let identity =
            |session: &str| SessionIdentity { runtime_root: directory.clone(), daemon: "default".into(), session: session.into() };
        let _lease = admit_at(&directory, Edge { source: identity("a"), target: identity("b") }).unwrap();
        fs::write(directory.join("ready"), b"ready").unwrap();
        loop {
            std::thread::park_timeout(std::time::Duration::from_secs(1));
        }
    }

    #[test]
    fn killed_daemon_owner_cannot_leave_a_blocking_edge() {
        let directory = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "output_admission::process_tests::lease_owner_child"])
            .env("CLEAT_TEST_LEASE_DIRECTORY", directory.path())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !directory.path().join("ready").exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let ready = directory.path().join("ready").exists();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(ready, "lease owner did not become ready");
        let identity = |session: &str| SessionIdentity {
            runtime_root: directory.path().to_owned(),
            daemon: "default".into(),
            session: session.into(),
        };
        assert!(admit_at(directory.path(), Edge { source: identity("b"), target: identity("a") }).is_ok());
    }
}
