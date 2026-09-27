//! Durable roll bookkeeping. Mutations require the logical generation lock.
use std::{collections::BTreeSet, fs, io::Write};

use crate::runtime::RuntimeLayout;

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct GenerationRecovery {
    pub high_water: u64,
    pub unpublished: BTreeSet<u64>,
    // Tombstones fence a spawned process which reaches startup after reclamation.
    pub reclaimed: BTreeSet<u64>,
    pub retirement: Option<Retirement>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Retirement {
    pub old: String,
    pub successor: u64,
}

impl GenerationRecovery {
    pub fn load(layout: &RuntimeLayout) -> Result<Self, String> {
        let path = layout.root().join(format!(".{}.generations.json", layout.logical_name()));
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| format!("read generation recovery: {e}")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("read generation recovery: {e}")),
        }
    }

    pub fn save(&self, layout: &RuntimeLayout) -> Result<(), String> {
        let temporary = layout.root().join(format!(".generations-{}", uuid::Uuid::new_v4()));
        let result = (|| -> Result<(), String> {
            let mut file = fs::File::create(&temporary).map_err(|e| format!("create temporary journal: {e}"))?;
            file.write_all(&serde_json::to_vec(self).map_err(|e| format!("encode journal: {e}"))?)
                .map_err(|e| format!("write temporary journal: {e}"))?;
            file.sync_all().map_err(|e| format!("sync temporary journal: {e}"))?;
            fs::rename(&temporary, layout.root().join(format!(".{}.generations.json", layout.logical_name())))
                .map_err(|e| format!("replace journal: {e}"))?;
            #[cfg(unix)]
            fs::File::open(layout.root()).and_then(|dir| dir.sync_all()).map_err(|e| format!("sync journal directory: {e}"))?;
            Ok(())
        })();
        let _ = fs::remove_file(temporary);
        result.map_err(|e| format!("persist generation recovery: {e}"))
    }

    pub fn check_start(&self, layout: &RuntimeLayout) -> Result<(), String> {
        if layout.generation().is_some_and(|generation| self.reclaimed.contains(&generation)) {
            return Err(format!("daemon generation {} was reclaimed; use --server {}", layout.daemon_name(), layout.logical_name()));
        }
        Ok(())
    }

    pub fn reclaim(&mut self, layout: &RuntimeLayout) -> Result<(), String> {
        let current = layout.generation();
        let candidates: BTreeSet<_> = self.unpublished.union(&self.reclaimed).copied().collect();
        for generation in candidates {
            if Some(generation) == current {
                continue;
            }
            let candidate = layout.clone().with_daemon(format!("{}@{generation}", layout.logical_name()))?;
            if self.reclaimed.contains(&generation) && !candidate.daemon_dir().exists() {
                continue;
            }
            let Some(_lease) = candidate.try_lock_daemon_lifetime()? else { continue };
            // Older binaries do not hold the lifetime lock. Keep live or ambiguous
            // registrations, reachable sockets, and every retained session directory.
            if candidate.daemon_pid_path().exists()
                && crate::platform::daemon::is_session_daemon_alive(candidate.root(), candidate.daemon_name())
            {
                continue;
            }
            if crate::platform::ipc::try_connect_session_stream(&candidate.socket_path()).is_ok() {
                continue;
            }
            match fs::read_dir(candidate.sessions_dir()) {
                Ok(mut entries) => {
                    if entries.next().is_some() {
                        continue;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("inspect unpublished sessions: {e}")),
            }
            // Persist the fence before removing anything, including on crash retry.
            self.high_water = self.high_water.max(generation);
            self.reclaimed.insert(generation);
            self.unpublished.remove(&generation);
            self.save(layout)?;
            match fs::remove_dir_all(candidate.daemon_dir()) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("reclaim unpublished generation: {e}")),
            }
        }
        Ok(())
    }
}

impl RuntimeLayout {
    /// Outside the generation directory so removing it cannot split the lock.
    /// The daemon holds this from before creating paths through final cleanup.
    pub(crate) fn try_lock_daemon_lifetime(&self) -> Result<Option<fs::File>, String> {
        self.ensure_root()?;
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.root().join(format!(".{}.lifetime.lock", self.daemon_name())))
            .map_err(|e| format!("open daemon lifetime lock: {e}"))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(e) => Err(format!("lock daemon lifetime: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reclamation_preserves_live_hosts_and_recordings_then_fences_delayed_startup() {
        let temp = tempfile::tempdir().unwrap();
        let layout = RuntimeLayout::new(temp.path().to_path_buf());
        layout.prepare_generation().unwrap();
        let unpublished = layout.allocate_unpublished_generation().unwrap();
        let lease = unpublished.try_lock_daemon_lifetime().unwrap().unwrap();
        let mut recovery = GenerationRecovery::load(&layout).unwrap();
        recovery.reclaim(&layout).unwrap();
        assert!(unpublished.daemon_dir().exists());
        drop(lease);
        fs::create_dir_all(unpublished.session_dir("explicit")).unwrap();
        let recording = unpublished.session_dir("explicit").join("session.cast");
        fs::write(&recording, "history").unwrap();
        recovery.reclaim(&layout).unwrap();
        assert_eq!(fs::read_to_string(recording).unwrap(), "history");
        fs::remove_dir_all(unpublished.session_dir("explicit")).unwrap();
        fs::write(unpublished.daemon_pid_path(), "0").unwrap();
        fs::write(unpublished.daemon_dir().join("build.json"), "{}").unwrap();
        // Parallel Unix process-spawn tests can briefly inherit our lock until
        // exec, even after we drop the owning descriptor. Reclamation is
        // deliberately retryable when a lifetime lock is still held.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while unpublished.daemon_dir().exists() {
            recovery.reclaim(&layout).unwrap();
            assert!(std::time::Instant::now() < deadline, "unpublished generation was not reclaimed");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(unpublished.prepare_generation().unwrap_err().contains("reclaimed"));
        assert!(crate::session::run_session_daemon(unpublished.root(), unpublished.daemon_name()).unwrap_err().contains("reclaimed"));
        assert!(!unpublished.daemon_dir().exists());
        assert_eq!(layout.allocate_generation().unwrap().generation(), Some(3));
    }

    #[test]
    fn reclamation_finishes_after_a_crash_between_tombstone_and_delete() {
        let temp = tempfile::tempdir().unwrap();
        let layout = RuntimeLayout::new(temp.path().to_path_buf());
        layout.prepare_generation().unwrap();
        let unpublished = layout.allocate_unpublished_generation().unwrap();
        let mut recovery = GenerationRecovery::load(&layout).unwrap();
        recovery.unpublished.remove(&2);
        recovery.reclaimed.insert(2);
        recovery.save(&layout).unwrap();
        GenerationRecovery::load(&layout).unwrap().reclaim(&layout).unwrap();
        assert!(!unpublished.daemon_dir().exists());
        assert_eq!(layout.allocate_generation().unwrap().generation(), Some(3));
    }

    #[test]
    fn high_water_survives_all_generation_directories_being_removed() {
        let temp = tempfile::tempdir().unwrap();
        let layout = RuntimeLayout::new(temp.path().to_path_buf());
        let first = layout.prepare_generation().unwrap();
        let second = layout.allocate_unpublished_generation().unwrap();
        fs::remove_dir_all(first.daemon_dir()).unwrap();
        fs::remove_dir_all(second.daemon_dir()).unwrap();
        assert_eq!(layout.allocate_generation().unwrap().generation(), Some(3));
    }

    #[test]
    fn corrupt_journal_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let layout = RuntimeLayout::new(temp.path().to_path_buf());
        let first = layout.prepare_generation().unwrap();
        fs::write(temp.path().join(".default.generations.json"), "broken").unwrap();
        assert!(layout.allocate_generation().is_err());
        assert!(first.prepare_generation().is_err());
        assert!(first.daemon_dir().exists());
    }
}
