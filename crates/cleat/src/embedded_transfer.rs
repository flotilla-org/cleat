//! Shared Unix embedded-host Transfer machinery. Ownership changes only at COMMIT.
use std::{
    fs::File,
    os::{fd::OwnedFd, unix::net::UnixStream},
    path::PathBuf,
};

use crate::{
    host::actor::{SessionActor, WakeCallback},
    runtime::SessionMetadata,
    session_runtime::{AdoptedSession, SessionRuntime, TransferSource},
    transfer_manifest::{FdManifestEntry, FdRole, FdTransferManifest, MANIFEST_VERSION, MIN_SUPPORTED_VERSION},
};

pub(crate) struct Offer {
    pub manifest: FdTransferManifest,
    pub fds: Vec<OwnedFd>,
    pub status_writer: Option<UnixStream>,
}

pub(crate) fn offer(source: TransferSource) -> Result<Offer, String> {
    let mut fds = vec![source.pty_master];
    let mut roles = vec![FdRole::pty_master()];
    if let Some(recording) = source.recording {
        fds.push(recording.into());
        roles.push(FdRole::recording());
    }
    #[cfg(target_os = "linux")]
    if let Ok(pidfd) = crate::child_observation::pidfd_open(source.child_pid) {
        fds.push(pidfd);
        roles.push(FdRole::pidfd());
    }
    let status_writer = if let Some(status) = source.upstream_status {
        fds.push(status);
        None
    } else {
        let (writer, reader) = UnixStream::pair().map_err(|err| err.to_string())?;
        fds.push(reader.into());
        Some(writer)
    };
    roles.push(FdRole::child_status());
    Ok(Offer {
        manifest: FdTransferManifest {
            version: MANIFEST_VERSION,
            min_supported_version: MIN_SUPPORTED_VERSION,
            fds: roles.into_iter().enumerate().map(|(index, role)| FdManifestEntry { index, role }).collect(),
            session: source.session,
            size: source.size,
            cell_pixel_size: source.cell_pixel_size,
            child_pid: source.child_pid,
            hosting_epoch: source.hosting_epoch.checked_add(1).ok_or("hosting epoch exhausted")?,
            replay_snapshot: source.replay_snapshot,
            markers: source.markers,
            recording_paused: source.recording_paused,
        },
        fds,
        status_writer,
    })
}

pub(crate) fn actor(
    manifest: FdTransferManifest,
    fds: Vec<OwnedFd>,
    session_dir: PathBuf,
    wake: WakeCallback,
) -> Result<SessionActor, String> {
    if manifest.replay_snapshot.engine != manifest.session.vt_engine.as_str() {
        return Err("snapshot engine does not match session".into());
    }
    let descriptors = crate::transfer::classify_descriptors(&manifest, fds)?;
    #[cfg(target_os = "linux")]
    let observer = match descriptors.pidfd {
        Some(fd) => Some(crate::child_observation::ChildObserver::from_pidfd(fd)),
        None => crate::child_observation::ChildObserver::new(manifest.child_pid).ok(),
    };
    #[cfg(not(target_os = "linux"))]
    let observer = {
        drop(descriptors.pidfd);
        crate::child_observation::ChildObserver::new(manifest.child_pid).ok()
    };
    let pty_child = crate::platform::pty::PtyChild::adopt(descriptors.pty_master, manifest.child_pid, observer, descriptors.child_status)?;
    let size = manifest.size;
    let metadata: SessionMetadata = manifest.session.clone();
    let adopted = AdoptedSession {
        session_dir,
        session: manifest.session,
        cell_pixel_size: manifest.cell_pixel_size,
        hosting_epoch: manifest.hosting_epoch,
        replay_snapshot: manifest.replay_snapshot,
        markers: manifest.markers,
        recording_paused: manifest.recording_paused,
        pty_child,
        recording: descriptors.recording,
    };
    SessionActor::spawn_adopted(size.rows, wake, move || {
        let engine = crate::vt::make_vt_engine_with_colors(metadata.vt_engine, size.cols, size.rows, metadata.colors)?;
        SessionRuntime::adopt(adopted, engine)
    })
}

/// A separate open file description held only by the embedded process. The
/// kernel drops this lock even after SIGKILL, including if the daemon restarts.
pub(crate) fn lock_holder(dir: &std::path::Path) -> Result<File, String> {
    use std::os::fd::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("embedded-holder.lock"))
        .map_err(|err| err.to_string())?;
    // SAFETY: file is a live descriptor; flock does not take ownership.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("session already has an embedded holder".into());
    }
    Ok(file)
}

pub(crate) fn held_elsewhere(dir: &std::path::Path) -> bool {
    dir.join("embedded-holder.lock").exists() && lock_holder(dir).is_err()
}

/// An embedded holder may return to a different daemon. The target has a
/// private, paused snapshot directory at READY. Park it while moving the
/// retained directory into place; until the epoch commits, Drop rolls back.
pub(crate) struct RelocatedDirectory {
    source: PathBuf,
    target: PathBuf,
    prepared: PathBuf,
    committed: bool,
}

impl RelocatedDirectory {
    pub(crate) fn prepare(source: &std::path::Path, target: &std::path::Path) -> Result<Self, String> {
        let prepared = target.with_extension(format!("prepared-{}", uuid::Uuid::new_v4()));
        std::fs::rename(target, &prepared).map_err(|err| format!("park prepared directory: {err}"))?;
        if let Err(err) = std::fs::rename(source, target) {
            let _ = std::fs::rename(&prepared, target);
            return Err(format!("move retained recording: {err}"));
        }
        Ok(Self { source: source.into(), target: target.into(), prepared, committed: false })
    }

    pub(crate) fn commit(mut self) {
        self.committed = true;
        let _ = std::fs::remove_dir_all(&self.prepared);
    }
}

impl Drop for RelocatedDirectory {
    fn drop(&mut self) {
        if !self.committed {
            if let Err(err) = std::fs::rename(&self.target, &self.source) {
                eprintln!("restore retained recording directory: {err}");
                return;
            }
            let _ = std::fs::rename(&self.prepared, &self.target);
        }
    }
}
