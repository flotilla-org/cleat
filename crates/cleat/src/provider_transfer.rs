//! Stable C handles retain their identity while their backend changes.
use super::*;

#[derive(Default)]
pub(super) struct TransferState {
    #[cfg_attr(not(unix), allow(dead_code))]
    root: PathBuf,
    hosting: String,
    #[cfg_attr(not(unix), allow(dead_code))]
    identity: AttachmentIdentity,
    error: String,
    #[cfg(unix)]
    holder: Option<(std::os::unix::net::UnixStream, std::fs::File, PathBuf)>,
}

impl TransferState {
    pub(super) fn new(root: PathBuf, identity: AttachmentIdentity) -> Self {
        Self { root, identity, ..Self::default() }
    }
}

/// Move an in-process session to an existing named daemon. Calls are serialized
/// with all other session operations; false preserves the previous backend.
/// # Safety
/// `session` must be live and the name must address `daemon_name_len` bytes.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_transfer(session: *mut CleatSession, daemon_name: *const u8, daemon_name_len: usize) -> bool {
    let Some(session) = (unsafe { session.as_mut() }) else {
        return false;
    };
    let result = read_optional_utf8(daemon_name, daemon_name_len)
        .map_err(|err| err.to_string())
        .and_then(|name| name.ok_or_else(|| "daemon name is required".to_string()))
        .and_then(|name| transfer_session(session, name));
    finish(session, result)
}

/// Adopt a daemon session into this process without replacing its C handle.
/// # Safety
/// `session` must be live and exclusively owned for the duration of this call.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_adopt(session: *mut CleatSession) -> bool {
    let Some(session) = (unsafe { session.as_mut() }) else {
        return false;
    };
    let result = adopt_session(session);
    finish(session, result)
}

fn finish(session: &mut CleatSession, result: Result<(), String>) -> bool {
    match result {
        Ok(()) => {
            session.transfer.error.clear();
            notify_wake(&session.wake);
            true
        }
        Err(err) => {
            session.transfer.error = err;
            false
        }
    }
}

/// Current hosting: `in_process` or `daemon:<name@generation>`.
/// # Safety
/// `session` and `out` must be valid. The borrowed string lasts until the next
/// hosting query, transfer, adoption, or destruction of the session.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_hosting(session: *mut CleatSession, out: *mut CleatStr) -> bool {
    let (Some(session), Some(out)) = (unsafe { session.as_mut() }, unsafe { out.as_mut() }) else {
        return false;
    };
    session.transfer.hosting = match &session.backend {
        SessionBackend::InProcess(_) => "in_process".into(),
        SessionBackend::Daemon(daemon) => {
            let layout = daemon.connection.channel_layout(daemon.channel);
            format!("daemon:{}", layout.resolved().unwrap_or(layout).daemon_name())
        }
        SessionBackend::Mock(_) => return false,
    };
    *out = CleatStr { ptr: session.transfer.hosting.as_ptr(), len: session.transfer.hosting.len() };
    true
}

/// Error from the last failed transfer/adoption, or an empty string on success.
/// # Safety
/// `session` and `out` must be valid. The string is borrowed until the next move
/// attempt or session destruction.
#[no_mangle]
pub unsafe extern "C" fn cleat_session_transfer_error(session: *const CleatSession, out: *mut CleatStr) -> bool {
    let (Some(session), Some(out)) = (unsafe { session.as_ref() }, unsafe { out.as_mut() }) else {
        return false;
    };
    *out = CleatStr { ptr: session.transfer.error.as_ptr(), len: session.transfer.error.len() };
    true
}

#[cfg(not(unix))]
fn transfer_session(_: &mut CleatSession, _: String) -> Result<(), String> {
    Err("session transfer is not supported on this platform".into())
}
#[cfg(not(unix))]
fn adopt_session(_: &mut CleatSession) -> Result<(), String> {
    Err("session transfer is not supported on this platform".into())
}

#[cfg(unix)]
fn stream(layout: &RuntimeLayout) -> Result<std::os::unix::net::UnixStream, String> {
    let stream = std::os::unix::net::UnixStream::connect(layout.socket_path()).map_err(|err| format!("daemon unreachable: {err}"))?;
    stream.set_read_timeout(Some(crate::transfer::DEFAULT_HANDSHAKE_TIMEOUT)).map_err(|err| err.to_string())?;
    stream.set_write_timeout(Some(crate::transfer::DEFAULT_HANDSHAKE_TIMEOUT)).map_err(|err| err.to_string())?;
    Ok(stream)
}

#[cfg(unix)]
fn transfer_session(session: &mut CleatSession, name: String) -> Result<(), String> {
    check_borrows(session)?;
    use std::io::Write;
    let SessionBackend::InProcess(embedded) = &session.backend else {
        return Err("session is not in-process".into());
    };
    let layout = RuntimeLayout::new(session.transfer.root.clone()).with_daemon(name)?.resolved()?;
    let mut stream = stream(&layout)?;
    stream.write_all(b"POST /transfer/embedded HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: cleat-transfer/1\r\nContent-Length: 0\r\n\r\n").map_err(|err| err.to_string())?;
    let response = crate::http_uds::read_transfer_response_head(&mut stream).map_err(|err| err.to_string())?;
    if response.status != http::StatusCode::SWITCHING_PROTOCOLS {
        return Err(format!("daemon refused embedded transfer: {}", response.status));
    }
    // Admission failures must be discovered before releasing the source.
    let _ = crate::provider_daemon::connect_packet_stream(&layout, &[])?;
    let result = (|| {
        let mut source = embedded.actor.prepare_transfer()?;
        // An initial embedded source starts a new recording at the snapshot.
        // An adopted source continues the daemon-owned recording descriptor.
        if session.transfer.holder.is_none() {
            source.recording = None;
            source.markers.clear();
            source.recording_paused = false;
        }
        let offer = crate::embedded_transfer::offer(source)?;
        let id = offer.manifest.session.id.clone();
        let epoch = offer.manifest.hosting_epoch;
        let mut stream = crate::transfer::handshake_stream(stream, &offer.manifest, &offer.fds).map_err(|err| err.message().to_string())?;
        let tail = embedded.actor.release_transfer()?;
        let destination = layout.session_dir(&id);
        let address = format!("daemon:{}", layout.daemon_name());
        let relocation = match &session.transfer.holder {
            Some((_, _, source)) if *source != destination => {
                Some(crate::embedded_transfer::RelocatedDirectory::prepare(source, &destination)?)
            }
            _ => None,
        };
        if session.transfer.holder.is_none() {
            // The target prepared a recording at the snapshot. Append the tail
            // before the durable epoch commit, so a lost COMMIT frame cannot
            // lose already-read output from the recording.
            let file = std::fs::OpenOptions::new()
                .append(true)
                .open(destination.join(crate::recording::CAST_FILE_NAME))
                .map_err(|err| err.to_string())?;
            let mut recorder = crate::recording::SessionRecorder::adopt_append(&destination, file)?;
            recorder.output(&tail, std::time::Duration::ZERO);
            recorder.transferred(epoch, &address, std::time::Duration::ZERO);
            recorder.flush();
        }
        if crate::hosting_epoch::read(&destination).map_err(|err| err.to_string())? != epoch - 1 {
            return Err("stale holder: hosting epoch changed during transfer".into());
        }
        if let Err(err) = crate::hosting_epoch::increment(&destination) {
            if crate::hosting_epoch::read(&destination).ok() != Some(epoch) {
                return Err(err.to_string());
            }
        }
        if let Some(relocation) = relocation {
            relocation.commit();
        }
        // The durable epoch is the commit point. The target can recover a
        // lost COMMIT frame from it; the old actor must never be resumed.
        let pid = match embedded.actor.commit_transfer(epoch, address) {
            Ok(pid) => pid,
            Err(err) => {
                eprintln!("embedded transfer committed; actor release failed: {err}");
                None
            }
        };
        if let (Some(pid), Some(mut writer)) = (pid, offer.status_writer) {
            let _ = std::thread::Builder::new().name("cleat-child-status".into()).spawn(move || {
                let mut status = 0;
                loop {
                    // SAFETY: waitpid writes one status integer for our own child.
                    let result = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, 0) };
                    if result > 0 {
                        let _ = writer.write_all(&status.to_be_bytes());
                        break;
                    }
                    if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                        break;
                    }
                }
            });
        }
        // COMMIT is irreversible. A lost confirmation must not resurrect the old
        // actor: the same handle reconnects to the authoritative daemon.
        let _ = crate::transfer::write_commit(&mut stream, &tail);
        let _ = crate::transfer::read_committed(&mut stream);
        let wake = Arc::clone(&session.wake);
        let connection = DaemonConnection::open(layout, Vec::new(), Arc::new(move || notify_wake(&wake)));
        let (width, height) = geometry_cell_size_to_backend(session.geometry);
        let geometry = TerminalResizeEvent {
            cols: offer.manifest.size.cols,
            rows: offer.manifest.size.rows,
            cell_width_px: width as f32,
            cell_height_px: height as f32,
        };
        let (channel, slot) = connection.open_session_channel(
            id.clone(),
            geometry,
            crate::packet::ChannelRole::Controller,
            session.transfer.identity.clone(),
        );
        Ok(DaemonSession { dedicated_connection: true, id, connection, channel, slot, images: Vec::new(), links: Vec::new() })
    })();
    match result {
        Ok(daemon) => {
            session.backend = SessionBackend::Daemon(daemon);
            session.transfer.holder = None;
            Ok(())
        }
        Err(err) => {
            let _ = embedded.actor.abort_transfer();
            Err(err)
        }
    }
}

#[cfg(unix)]
fn adopt_session(session: &mut CleatSession) -> Result<(), String> {
    check_borrows(session)?;
    let SessionBackend::Daemon(daemon) = &session.backend else {
        return Err("session is not daemon-backed".into());
    };
    let identity = daemon.slot.lock().map_err(|_| "session channel lock poisoned")?.identity.clone();
    let layout = daemon.connection.channel_layout(daemon.channel).resolved()?;
    let mut inspect_stream = stream(&layout)?;
    crate::http_uds::write_request(&mut inspect_stream, http::Method::GET, &format!("/sessions/{}", daemon.id), &[])
        .map_err(|err| err.to_string())?;
    let response = crate::http_uds::read_response(&mut inspect_stream).map_err(|err| err.to_string())?;
    let inspect: crate::protocol::InspectResult =
        serde_json::from_slice(&response.body).map_err(|err| format!("inspect before adoption: {err}"))?;
    let dir = layout.session_dir(&daemon.id);
    let holder = crate::embedded_transfer::lock_holder(&dir)?;
    let mut stream = stream(&layout)?;
    crate::http_uds::write_request_with_epoch(
        &mut stream,
        http::Method::POST,
        &format!("/sessions/{}/adopt", daemon.id),
        &[],
        Some(inspect.hosting_epoch),
    )
    .map_err(|err| err.to_string())?;
    let response = crate::http_uds::read_transfer_response_head(&mut stream).map_err(|err| err.to_string())?;
    if response.status != http::StatusCode::SWITCHING_PROTOCOLS {
        return Err(format!("daemon refused adoption: {}", response.status));
    }
    let received = crate::fd_transfer::receive(&mut stream)?;
    if received.manifest.session.id != daemon.id || received.manifest.hosting_epoch != inspect.hosting_epoch + 1 {
        return Err("stale holder: unexpected adoption identity or epoch".into());
    }
    let (manifest, fds) = received.commit();
    let wake = Arc::clone(&session.wake);
    let actor = crate::embedded_transfer::actor(manifest, fds, dir.clone(), Arc::new(move || notify_wake(&wake)))?;
    actor.set_query_passthrough(false)?;
    crate::transfer::write_ready(&mut stream).map_err(|err| err.to_string())?;
    let tail = crate::transfer::read_commit(&mut stream)?.ok_or("daemon aborted adoption")?;
    actor.resume_adopted(tail)?;
    let _ = crate::transfer::write_committed(&mut stream);
    session.backend = SessionBackend::InProcess(Box::new(InProcessSession { actor }));
    session.transfer.identity = identity;
    session.transfer.holder = Some((stream, holder, dir));
    Ok(())
}

#[cfg(unix)]
fn check_borrows(session: &CleatSession) -> Result<(), String> {
    if session.last_snapshot.is_some() || session.last_render_update.is_some() {
        Err("release the current render update or snapshot before changing hosting".into())
    } else {
        Ok(())
    }
}
