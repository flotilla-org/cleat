//! The daemon side of Transfer (issue #254): releasing a hosted session to
//! another daemon and adopting one from it. The exchange itself runs on
//! workers (`crate::transfer`); this module is the servicing loop's half,
//! which only ever polls channels and performs bounded local work.

use std::{
    collections::HashMap,
    fs,
    io::Write,
    os::{fd::OwnedFd, unix::net::UnixStream},
    path::Path,
    sync::mpsc::{self, Receiver, Sender, TryRecvError},
    thread,
    time::{Duration, Instant},
};

use http::StatusCode;

use super::{
    announce_seat_state, broadcast_directory_remove, broadcast_directory_upsert, default_vt_engine, directory_entry_for_session,
    sync_packet_geometry, write_http_not_found, HostedSession, PacketClient,
};
use crate::{
    child_observation::ChildObserver,
    host::actor::SessionActor,
    http_uds,
    packet::{ChannelRedirect, ChannelRole, ControlError, SessionRedirect, MSG_CONTROL_ERROR, MSG_CONTROL_REDIRECT, PROTOCOL_VERSION},
    platform::{ipc::SessionStream, pty::PtyChild},
    protocol::TransferResult,
    runtime::{RuntimeLayout, SessionMetadata},
    session_runtime::{AdoptedSession, SessionRuntime},
    transfer::{self, AdoptionOffer, CommitOutcome, HandshakeFailure, SourceDecision, SourceEvent, TargetProtocol},
    transfer_manifest::{FdManifestEntry, FdRole, FdTransferManifest, MANIFEST_VERSION, MIN_SUPPORTED_VERSION},
};

/// Daemon-wide Transfer state, owned by the servicing loop.
pub(super) struct TransferHub {
    outgoing: Vec<OutgoingTransfer>,
    offers_tx: Sender<AdoptionOffer>,
    offers_rx: Receiver<AdoptionOffer>,
    commits_tx: Sender<CommitOutcome>,
    commits_rx: Receiver<CommitOutcome>,
    pending_adoptions: HashMap<String, PendingAdoption>,
    moved: HashMap<String, MovedSession>,
    forwarders: Vec<StatusForwarder>,
    /// Highest hosting epoch this daemon has seen per session id. An adoption
    /// must exceed it, so a stale or replayed manifest can never win.
    seen_epochs: HashMap<String, u64>,
}

struct OutgoingTransfer {
    session_id: String,
    response: SessionStream,
    target: RuntimeLayout,
    drop_incompatible: bool,
    deadline: Instant,
    events: Receiver<SourceEvent>,
    decisions: Option<Sender<SourceDecision>>,
    phase: OutgoingPhase,
    dropped_clients: Vec<String>,
}

enum OutgoingPhase {
    Probing,
    Handshaking { protocol: TargetProtocol, status_writer: UnixStream },
    Committing { result: TransferResult, since: Instant },
}

struct PendingAdoption {
    hosted: HostedSession,
    stream: UnixStream,
    epoch: u64,
}

struct MovedSession {
    redirect: SessionRedirect,
    until: Instant,
}

/// A released child this daemon still reaps, forwarding its wait status to
/// the adopter while it lives (ruling 6).
struct StatusForwarder {
    pid: libc::pid_t,
    writer: UnixStream,
}

impl TransferHub {
    pub(super) fn new() -> Self {
        let (offers_tx, offers_rx) = mpsc::channel();
        let (commits_tx, commits_rx) = mpsc::channel();
        Self {
            outgoing: Vec::new(),
            offers_tx,
            offers_rx,
            commits_tx,
            commits_rx,
            pending_adoptions: HashMap::new(),
            moved: HashMap::new(),
            forwarders: Vec::new(),
            seen_epochs: HashMap::new(),
        }
    }

    pub(super) fn note_epoch(&mut self, id: &str, epoch: u64) {
        let seen = self.seen_epochs.entry(id.to_string()).or_insert(epoch);
        *seen = (*seen).max(epoch);
    }

    /// True while a transfer is in flight either way; the daemon must not
    /// linger-exit under it.
    pub(super) fn busy(&self) -> bool {
        !self.outgoing.is_empty() || !self.pending_adoptions.is_empty()
    }

    pub(super) fn adoption_pending(&self, id: &str) -> bool {
        self.pending_adoptions.contains_key(id)
    }

    /// The redirect a request for a released session receives during the
    /// grace window; afterwards the id is plainly not found.
    pub(super) fn redirect_for(&self, id: &str) -> Option<&SessionRedirect> {
        self.moved.get(id).filter(|moved| Instant::now() < moved.until).map(|moved| &moved.redirect)
    }

    /// Accept a `cleat-transfer/1` connection: the transport receive runs on a
    /// worker; the servicing loop sees only its validated result.
    pub(super) fn accept_incoming(&self, stream: UnixStream) {
        let offers = self.offers_tx.clone();
        let spawned =
            thread::Builder::new().name("cleat-transfer-in".into()).spawn(move || transfer::run_adoption_receiver(stream, offers));
        if let Err(err) = spawned {
            eprintln!("cleat: failed to spawn transfer receiver: {err}");
        }
    }

    /// Start releasing `id` to `target`. The session is frozen from here until
    /// the transfer commits or fails.
    pub(super) fn start_outgoing(
        &mut self,
        hosted: &mut HostedSession,
        response: SessionStream,
        target: RuntimeLayout,
        drop_incompatible: bool,
        timeout: Duration,
    ) {
        let (events_tx, events) = mpsc::channel();
        let (decisions, decisions_rx) = mpsc::channel();
        let deadline = Instant::now() + timeout;
        let socket = target.socket_path();
        let spawned = thread::Builder::new()
            .name(format!("cleat-transfer-out:{}", hosted.metadata.id))
            .spawn(move || transfer::run_source_worker(socket, deadline, events_tx, decisions_rx));
        let mut transfer = OutgoingTransfer {
            session_id: hosted.metadata.id.clone(),
            response,
            target,
            drop_incompatible,
            deadline,
            events,
            decisions: Some(decisions),
            phase: OutgoingPhase::Probing,
            dropped_clients: Vec::new(),
        };
        if let Err(err) = spawned {
            transfer.respond_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("start transfer worker: {err}"));
            return;
        }
        hosted.transferring = true;
        self.outgoing.push(transfer);
    }

    /// One servicing pass: outgoing transfers, incoming adoptions, status
    /// forwarding, and redirect expiry. Never blocks on a peer.
    pub(super) fn service(
        &mut self,
        layout: &RuntimeLayout,
        sessions: &mut HashMap<String, HostedSession>,
        packet_clients: &mut Vec<PacketClient>,
    ) -> Result<bool, String> {
        let mut did_work = false;
        let mut index = 0;
        while index < self.outgoing.len() {
            match self.service_outgoing(index, layout, sessions, packet_clients)? {
                Some(worked) => {
                    did_work |= worked;
                    index += 1;
                }
                None => {
                    did_work = true;
                    self.outgoing.swap_remove(index);
                }
            }
        }
        while let Ok(offer) = self.offers_rx.try_recv() {
            did_work = true;
            self.consider_offer(layout, sessions, offer);
        }
        while let Ok((id, outcome)) = self.commits_rx.try_recv() {
            did_work = true;
            self.finish_adoption(layout, sessions, packet_clients, &id, outcome)?;
        }
        self.forwarders.retain_mut(StatusForwarder::poll);
        let now = Instant::now();
        self.moved.retain(|_, moved| now < moved.until);
        Ok(did_work)
    }

    /// `None` when the transfer is finished and must be dropped.
    fn service_outgoing(
        &mut self,
        index: usize,
        layout: &RuntimeLayout,
        sessions: &mut HashMap<String, HostedSession>,
        packet_clients: &mut Vec<PacketClient>,
    ) -> Result<Option<bool>, String> {
        let id = self.outgoing[index].session_id.clone();
        let event = match self.outgoing[index].events.try_recv() {
            Ok(event) => Some(event),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                if matches!(self.outgoing[index].phase, OutgoingPhase::Committing { .. }) {
                    Some(SourceEvent::Committed(Err("transfer worker stopped before the target confirmed".into())))
                } else {
                    Some(SourceEvent::Handshake(Err(HandshakeFailure::Failed("transfer worker stopped".into()))))
                }
            }
        };
        let transfer = &mut self.outgoing[index];
        if let OutgoingPhase::Committing { result, since } = &transfer.phase {
            let confirmed = match event {
                Some(SourceEvent::Committed(Ok(()))) => true,
                Some(SourceEvent::Committed(Err(err))) => {
                    eprintln!("transfer of {id}: committed, but the target did not confirm: {err}");
                    true
                }
                _ => since.elapsed() >= transfer::COMMIT_WAIT,
            };
            if confirmed {
                let result = result.clone();
                transfer.respond(StatusCode::OK, &result);
                return Ok(None);
            }
            return Ok(Some(false));
        }
        let Some(hosted) = sessions.get_mut(&id) else {
            transfer.respond_error(StatusCode::CONFLICT, &format!("session {id} exited during the transfer"));
            return Ok(None);
        };
        let Some(event) = event else {
            if Instant::now() < transfer.deadline {
                return Ok(Some(false));
            }
            // Target slow or stalled: keep everything, never destroy (#8).
            let message = format!(
                "transfer of session {id} to daemon:{} timed out before the target was ready; the session was left in place",
                transfer.target.daemon_name()
            );
            abort_outgoing(hosted, transfer, packet_clients, layout, StatusCode::GATEWAY_TIMEOUT, &message);
            return Ok(None);
        };
        match event {
            SourceEvent::Probed(Err(err)) => {
                abort_outgoing(hosted, transfer, packet_clients, layout, StatusCode::BAD_GATEWAY, &err);
                Ok(None)
            }
            SourceEvent::Probed(Ok(protocol)) => match prepare_outgoing(hosted, transfer, protocol, packet_clients) {
                Ok(()) => Ok(Some(true)),
                Err((status, message)) => {
                    abort_outgoing(hosted, transfer, packet_clients, layout, status, &message);
                    Ok(None)
                }
            },
            SourceEvent::Handshake(Err(failure)) => {
                let status = match failure {
                    HandshakeFailure::Refused(_) => StatusCode::CONFLICT,
                    HandshakeFailure::Failed(_) => StatusCode::BAD_GATEWAY,
                };
                abort_outgoing(hosted, transfer, packet_clients, layout, status, failure.message());
                Ok(None)
            }
            SourceEvent::Handshake(Ok(())) => {
                if Instant::now() >= transfer.deadline {
                    let message = format!("transfer of session {id} became ready after its deadline; the session was left in place");
                    abort_outgoing(hosted, transfer, packet_clients, layout, StatusCode::GATEWAY_TIMEOUT, &message);
                    return Ok(None);
                }
                self.commit_outgoing(index, layout, sessions, packet_clients)
            }
            SourceEvent::Committed(_) => Ok(Some(false)),
        }
    }

    /// READY arrived: commit. Order matters: stop reading the PTY (tail),
    /// advance the epoch, mark the recording, move the directory, then close
    /// every channel with a redirect. Only a definitive epoch failure still
    /// aborts; after that the transfer is committed (ruling on #253).
    fn commit_outgoing(
        &mut self,
        index: usize,
        layout: &RuntimeLayout,
        sessions: &mut HashMap<String, HostedSession>,
        packet_clients: &mut Vec<PacketClient>,
    ) -> Result<Option<bool>, String> {
        let transfer = &mut self.outgoing[index];
        let id = transfer.session_id.clone();
        let hosted = sessions.get_mut(&id).expect("checked by caller");
        let OutgoingPhase::Handshaking { protocol, .. } = &transfer.phase else {
            abort_outgoing(hosted, transfer, packet_clients, layout, StatusCode::INTERNAL_SERVER_ERROR, "transfer ready out of order");
            return Ok(None);
        };
        let protocol = *protocol;
        let tail = match hosted.actor.release_transfer() {
            Ok(tail) => tail,
            Err(err) => {
                abort_outgoing(hosted, transfer, packet_clients, layout, StatusCode::CONFLICT, &err);
                return Ok(None);
            }
        };
        let session_dir = layout.session_dir(&id);
        let expected = hosted.hosting_epoch.saturating_add(1);
        let epoch = match crate::hosting_epoch::increment(&session_dir) {
            Ok(epoch) => epoch,
            Err(err) => match crate::hosting_epoch::read(&session_dir) {
                Ok(epoch) if epoch == expected => {
                    eprintln!("transfer of {id}: epoch sync after rename was ambiguous ({err}); treating as committed");
                    epoch
                }
                _ => {
                    let message =
                        format!("transfer of session {id} could not advance its hosting epoch: {err}; the session was left in place");
                    abort_outgoing(hosted, transfer, packet_clients, layout, StatusCode::INTERNAL_SERVER_ERROR, &message);
                    return Ok(None);
                }
            },
        };
        if epoch != expected {
            eprintln!("transfer of {id}: hosting epoch advanced to {epoch}, expected {expected}");
        }
        let address = format!("daemon:{}", transfer.target.daemon_name());
        let reap = match hosted.actor.commit_transfer(epoch, address.clone()) {
            Ok(reap) => reap,
            Err(err) => {
                eprintln!("transfer of {id}: releasing the session actor failed after commit: {err}");
                None
            }
        };
        if let Err(err) = move_session_dir(layout, &transfer.target, &id) {
            eprintln!("transfer of {id}: {err}");
        }
        let redirect = SessionRedirect {
            session_id: id.clone(),
            address: address.clone(),
            runtime_root: transfer.target.root().display().to_string(),
            daemon: transfer.target.daemon_name().to_string(),
            hosting_epoch: epoch,
            protocol_version: protocol.version,
            min_supported_version: protocol.min_supported_version,
        };
        let Some(mut released) = sessions.remove(&id) else {
            unreachable!("session checked above");
        };
        release_transferred_session(&mut released, &redirect, packet_clients)?;
        drop(released);
        let OutgoingPhase::Handshaking { status_writer, .. } = std::mem::replace(&mut transfer.phase, OutgoingPhase::Probing) else {
            unreachable!("phase checked above");
        };
        if let Some(pid) = reap {
            self.forwarders.push(StatusForwarder { pid: pid as libc::pid_t, writer: status_writer });
        }
        let dropped_clients = std::mem::take(&mut transfer.dropped_clients);
        transfer.phase = OutgoingPhase::Committing {
            result: TransferResult { session_id: id.clone(), address, hosting_epoch: epoch, dropped_clients },
            since: Instant::now(),
        };
        if let Some(decisions) = &transfer.decisions {
            let _ = decisions.send(SourceDecision::Commit { tail });
        }
        self.moved.insert(id.clone(), MovedSession { redirect, until: Instant::now() + transfer::REDIRECT_GRACE });
        self.note_epoch(&id, epoch);
        Ok(Some(true))
    }

    /// Validate an offer and build its runtime (not yet reading the PTY), then
    /// reply READY; or refuse it. The transport already checked the envelope.
    fn consider_offer(&mut self, layout: &RuntimeLayout, sessions: &HashMap<String, HostedSession>, offer: AdoptionOffer) {
        let AdoptionOffer { received, mut stream } = offer;
        let id = received.manifest.session.id.clone();
        let epoch = received.manifest.hosting_epoch;
        let adopted = self.validate_offer(layout, sessions, &received.manifest).and_then(|()| {
            let (manifest, fds) = received.commit();
            adopt_session(layout, manifest, fds)
        });
        let hosted = match adopted {
            Ok(hosted) => hosted,
            Err(reason) => {
                let _ = transfer::write_refusal(&mut stream, &reason);
                return;
            }
        };
        if let Err(err) = transfer::write_ready(&mut stream) {
            eprintln!("transfer of {id}: could not reply ready: {err}");
            return;
        }
        let commit_stream = match stream.try_clone() {
            Ok(commit_stream) => commit_stream,
            Err(err) => {
                eprintln!("transfer of {id}: {err}");
                return;
            }
        };
        let outcomes = self.commits_tx.clone();
        let session_id = id.clone();
        if let Err(err) = thread::Builder::new()
            .name(format!("cleat-transfer-commit:{id}"))
            .spawn(move || transfer::run_commit_receiver(commit_stream, session_id, outcomes))
        {
            eprintln!("transfer of {id}: failed to spawn commit receiver: {err}");
            return;
        }
        self.pending_adoptions.insert(id, PendingAdoption { hosted, stream, epoch });
    }

    fn validate_offer(
        &self,
        layout: &RuntimeLayout,
        sessions: &HashMap<String, HostedSession>,
        manifest: &FdTransferManifest,
    ) -> Result<(), String> {
        maybe_refuse_adoption_for_test()?;
        let id = &manifest.session.id;
        crate::runtime::validate_runtime_name(id)?;
        if sessions.contains_key(id) || self.pending_adoptions.contains_key(id) {
            return Err(format!("session {id} is already live on this daemon"));
        }
        if std::fs::read_to_string(layout.daemon_dir().join("drain-state")).is_ok_and(|state| state.trim() == "draining") {
            return Err(format!("daemon:{} is draining and accepts no new sessions", layout.daemon_name()));
        }
        if layout.session_dir(id).exists() {
            return Err(format!("this daemon retains state for session {id}; purge it (cleat kill --purge) before transferring here"));
        }
        if let Some(seen) = self.seen_epochs.get(id) {
            if manifest.hosting_epoch <= *seen {
                return Err(format!("hosting epoch {} for session {id} is not newer than epoch {seen} seen here", manifest.hosting_epoch));
            }
        }
        let engine = manifest.session.vt_engine;
        if manifest.replay_snapshot.engine != engine.as_str() {
            return Err(format!(
                "replay snapshot engine {} does not match session engine {}",
                manifest.replay_snapshot.engine,
                engine.as_str()
            ));
        }
        engine.ensure_available()
    }

    fn finish_adoption(
        &mut self,
        layout: &RuntimeLayout,
        sessions: &mut HashMap<String, HostedSession>,
        packet_clients: &mut Vec<PacketClient>,
        id: &str,
        outcome: Result<Option<Vec<u8>>, String>,
    ) -> Result<(), String> {
        let Some(mut pending) = self.pending_adoptions.remove(id) else {
            return Ok(());
        };
        let session_dir = layout.session_dir(id);
        let tail = match outcome {
            Ok(Some(tail)) => tail,
            outcome => {
                // The commit frame never arrived. If the source got as far as
                // moving the directory at the new epoch it has committed:
                // adopt without the tail rather than strand the session.
                let committed = session_dir.is_dir() && crate::hosting_epoch::read(&session_dir).ok() == Some(pending.epoch);
                if !committed {
                    if let Err(err) = outcome {
                        eprintln!("transfer of {id} abandoned before commit: {err}");
                    }
                    return Ok(());
                }
                eprintln!("transfer of {id}: commit frame lost after the source committed; adopting without its output tail");
                Vec::new()
            }
        };
        if let Err(err) = pending.hosted.actor.resume_adopted(tail) {
            eprintln!("transfer of {id}: adoption failed at commit: {err}");
            return Ok(());
        }
        if !session_dir.is_dir() {
            // The source could not move its directory; keep the layout
            // invariant that a live session has one, at the adopted epoch.
            let _ = fs::create_dir_all(&session_dir);
            let _ = fs::write(session_dir.join(crate::hosting_epoch::EPOCH_FILE_NAME), format!("{}\n", pending.epoch));
        }
        self.note_epoch(id, pending.epoch);
        let _ = transfer::write_committed(&mut pending.stream);
        let entry = directory_entry_for_session(layout, &pending.hosted, packet_clients)?;
        sessions.insert(id.to_string(), pending.hosted);
        broadcast_directory_upsert(entry, packet_clients)
    }
}

impl OutgoingTransfer {
    fn respond<T: serde::Serialize>(&mut self, status: StatusCode, value: &T) {
        let _ = http_uds::write_json(&mut self.response, status, value);
    }

    fn respond_error(&mut self, status: StatusCode, message: &str) {
        let _ = http_uds::write_error(&mut self.response, status, message);
    }
}

/// The compatibility gate (ruling 9), then the manifest. Everything a client
/// negotiated was this daemon's own protocol version, so a target that
/// refuses it strands every packet attachment; stream attachments can never
/// follow a redirect at all.
fn prepare_outgoing(
    hosted: &mut HostedSession,
    transfer: &mut OutgoingTransfer,
    protocol: TargetProtocol,
    packet_clients: &[PacketClient],
) -> Result<(), (StatusCode, String)> {
    let id = hosted.metadata.id.clone();
    let packet_compatible = protocol.accepts(PROTOCOL_VERSION);
    let mut incompatible = Vec::new();
    if !packet_compatible {
        for client in packet_clients.iter().filter(|client| !client.dead) {
            for channel in client.channels.values().filter(|channel| channel.session_id == id) {
                incompatible.push(describe_client(&channel.identity, channel.role));
            }
        }
    }
    if let Some(client) = &hosted.active_client {
        incompatible.push(describe_client(&client.identity, ChannelRole::Controller));
    }
    for client in &hosted.watchers {
        incompatible.push(describe_client(&client.identity, ChannelRole::Watcher));
    }
    if !incompatible.is_empty() && !transfer.drop_incompatible {
        return Err((
            StatusCode::CONFLICT,
            format!(
                "transfer of session {id} refused: {} attached client(s) cannot follow to daemon:{} (protocol {}..={}; clients speak {PROTOCOL_VERSION}): {}; rerun with --drop-incompatible to drop them",
                incompatible.len(),
                transfer.target.daemon_name(),
                protocol.min_supported_version,
                protocol.version,
                incompatible.join(", ")
            ),
        ));
    }
    transfer.dropped_clients = incompatible;

    let source = hosted.actor.prepare_transfer().map_err(|err| (StatusCode::CONFLICT, err))?;
    let mut fds: Vec<OwnedFd> = vec![source.pty_master];
    let mut roles = vec![FdRole::pty_master()];
    if let Some(recording) = source.recording {
        fds.push(recording.into());
        roles.push(FdRole::recording());
    }
    #[cfg(target_os = "linux")]
    match crate::child_observation::pidfd_open(source.child_pid) {
        Ok(pidfd) => {
            fds.push(pidfd);
            roles.push(FdRole::pidfd());
        }
        Err(err) => eprintln!("transfer of {id}: no pidfd for the child ({err}); the adopter observes it by pid"),
    }
    let (status_writer, status_reader) = match UnixStream::pair() {
        Ok(pair) => pair,
        Err(err) => {
            let _ = hosted.actor.abort_transfer();
            return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("create child status stream: {err}")));
        }
    };
    fds.push(status_reader.into());
    roles.push(FdRole::child_status());
    let version = test_manifest_version().unwrap_or(MANIFEST_VERSION);
    let manifest = FdTransferManifest {
        version,
        min_supported_version: MIN_SUPPORTED_VERSION.min(version),
        fds: roles.into_iter().enumerate().map(|(index, role)| FdManifestEntry { index, role }).collect(),
        session: source.session,
        size: source.size,
        cell_pixel_size: source.cell_pixel_size,
        child_pid: source.child_pid,
        // The epoch the adopter will hold once this host commits.
        hosting_epoch: source.hosting_epoch.saturating_add(1),
        replay_snapshot: source.replay_snapshot,
        markers: source.markers,
        recording_paused: source.recording_paused,
    };
    let Some(decisions) = &transfer.decisions else {
        let _ = hosted.actor.abort_transfer();
        return Err((StatusCode::INTERNAL_SERVER_ERROR, "transfer worker is gone".into()));
    };
    if decisions.send(SourceDecision::Proceed { manifest: Box::new(manifest), fds }).is_err() {
        let _ = hosted.actor.abort_transfer();
        return Err((StatusCode::INTERNAL_SERVER_ERROR, "transfer worker stopped".into()));
    }
    transfer.phase = OutgoingPhase::Handshaking { protocol, status_writer };
    Ok(())
}

fn describe_client(identity: &crate::protocol::AttachmentIdentity, role: ChannelRole) -> String {
    let role = match role {
        ChannelRole::Controller => "controller",
        ChannelRole::Watcher => "watcher",
    };
    format!("{} ({}, {role})", identity.name, identity.kind.as_str())
}

/// Leave the session exactly as it was: resume reading the PTY, unfreeze,
/// and tell the worker to stop. The target never adopts without COMMIT.
fn abort_outgoing(
    hosted: &mut HostedSession,
    transfer: &mut OutgoingTransfer,
    packet_clients: &mut Vec<PacketClient>,
    layout: &RuntimeLayout,
    status: StatusCode,
    message: &str,
) {
    if !matches!(transfer.phase, OutgoingPhase::Probing) {
        if let Err(err) = hosted.actor.abort_transfer() {
            eprintln!("transfer of {}: resuming the session failed: {err}", hosted.metadata.id);
        }
    }
    if let Some(decisions) = transfer.decisions.take() {
        let _ = decisions.send(SourceDecision::Abort);
    }
    unfreeze(hosted, packet_clients, layout);
    transfer.respond_error(status, message);
}

/// Apply whatever control changes were held back while frozen.
pub(super) fn unfreeze(hosted: &mut HostedSession, packet_clients: &mut Vec<PacketClient>, layout: &RuntimeLayout) {
    hosted.transferring = false;
    let _ = sync_packet_geometry(hosted);
    let _ = announce_seat_state(hosted, packet_clients);
    if let Ok(entry) = directory_entry_for_session(layout, hosted, packet_clients) {
        let _ = broadcast_directory_upsert(entry, packet_clients);
    }
}

/// Close every attachment of a released session with a redirect: packet
/// channels receive the new address first, then the close. Requests parked on
/// the session learn where it went.
fn release_transferred_session(
    hosted: &mut HostedSession,
    redirect: &SessionRedirect,
    packet_clients: &mut Vec<PacketClient>,
) -> Result<(), String> {
    let id = &redirect.session_id;
    let message = format!("session {id} moved to {}", redirect.address);
    for client in packet_clients.iter_mut() {
        let channels: Vec<u32> =
            client.channels.iter().filter(|(_, channel)| channel.session_id == *id).map(|(channel, _)| *channel).collect();
        for channel in channels {
            let _ = client.enqueue_control(MSG_CONTROL_REDIRECT, &ChannelRedirect { channel, redirect: redirect.clone() });
            let _ = client.enqueue_control(MSG_CONTROL_ERROR, &ControlError { channel, message: message.clone() });
            client.channels.remove(&channel);
        }
    }
    for mut wait in hosted.pending_waits.drain(..) {
        let _ = write_redirect(&mut wait.stream, redirect, false);
    }
    for mut expect in hosted.pending_expects.drain(..) {
        let _ = write_redirect(&mut expect.stream, redirect, false);
    }
    // Stream attachments cannot follow a redirect; dropping them closes them.
    hosted.active_client = None;
    hosted.watchers.clear();
    broadcast_directory_remove(id, packet_clients)
}

pub(super) fn write_redirect(stream: &mut SessionStream, redirect: &SessionRedirect, stale_holder: bool) -> std::io::Result<()> {
    let error = if stale_holder {
        format!(
            "stale holder: session {} moved to {} at hosting epoch {}; this holder's epoch is stale",
            redirect.session_id, redirect.address, redirect.hosting_epoch
        )
    } else {
        format!("session {} moved to {} (hosting epoch {})", redirect.session_id, redirect.address, redirect.hosting_epoch)
    };
    http_uds::write_json(stream, StatusCode::MISDIRECTED_REQUEST, &http_uds::RedirectResponse {
        error,
        redirect: redirect.clone(),
        stale_holder,
    })
}

pub(super) fn write_stale_holder(stream: &mut SessionStream, id: &str, current: u64, held: u64) -> std::io::Result<()> {
    http_uds::write_json(
        stream,
        StatusCode::CONFLICT,
        &serde_json::json!({
            "error": format!("stale holder: session {id} is at hosting epoch {current}; this holder's epoch {held} is stale"),
            "stale_holder": true,
            "hosting_epoch": current,
        }),
    )
}

/// Rename into the target's `sessions/` on the same runtime root (under the
/// target name's layout lock); copy across roots. The recorder's open
/// descriptor survives either way.
fn move_session_dir(layout: &RuntimeLayout, target: &RuntimeLayout, id: &str) -> Result<(), String> {
    let source = layout.session_dir(id);
    let source = source.as_path();
    let destination = target.session_dir(id);
    if destination.exists() {
        return Err(format!("target session directory {} already exists; left {} in place", destination.display(), source.display()));
    }
    let canonical = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if canonical(layout.root()) == canonical(target.root()) {
        let _lock = target.lock_generations()?;
        fs::create_dir_all(target.sessions_dir()).map_err(|err| format!("create {}: {err}", target.sessions_dir().display()))?;
        return fs::rename(source, &destination)
            .map_err(|err| format!("move session directory {} to {}: {err}", source.display(), destination.display()));
    }
    copy_dir(source, &destination)?;
    fs::remove_dir_all(source).map_err(|err| format!("remove copied session directory {}: {err}", source.display()))
}

fn copy_dir(source: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(|err| format!("create {}: {err}", destination.display()))?;
    for entry in fs::read_dir(source).map_err(|err| format!("read {}: {err}", source.display()))? {
        let entry = entry.map_err(|err| format!("read {}: {err}", source.display()))?;
        let path = entry.path();
        let target = destination.join(entry.file_name());
        let kind = entry.file_type().map_err(|err| format!("inspect {}: {err}", path.display()))?;
        if kind.is_dir() {
            copy_dir(&path, &target)?;
        } else if kind.is_file() {
            fs::copy(&path, &target).map_err(|err| format!("copy {}: {err}", path.display()))?;
        }
    }
    Ok(())
}

/// Build the paused runtime for an offer. Everything here is local and
/// bounded; the PTY is not read until the source commits.
fn adopt_session(layout: &RuntimeLayout, manifest: FdTransferManifest, fds: Vec<OwnedFd>) -> Result<HostedSession, String> {
    let descriptors = transfer::classify_descriptors(&manifest, fds)?;
    #[cfg(target_os = "linux")]
    let observer = match descriptors.pidfd {
        Some(pidfd) => Some(ChildObserver::from_pidfd(pidfd)),
        None => ChildObserver::new(manifest.child_pid).ok(),
    };
    #[cfg(not(target_os = "linux"))]
    let observer = {
        drop(descriptors.pidfd);
        ChildObserver::new(manifest.child_pid).ok()
    };
    let pty_child = PtyChild::adopt(descriptors.pty_master, manifest.child_pid, observer, descriptors.child_status)?;
    let id = manifest.session.id.clone();
    let session_dir = layout.session_dir(&id);
    let size = manifest.size;
    let cell_pixel_size = manifest.cell_pixel_size;
    let hosting_epoch = manifest.hosting_epoch;
    let mut engine_session: SessionMetadata = manifest.session.clone();
    engine_session.initial_size = size;
    let adopted = AdoptedSession {
        session_dir,
        session: manifest.session.clone(),
        cell_pixel_size,
        hosting_epoch,
        replay_snapshot: manifest.replay_snapshot,
        markers: manifest.markers,
        recording_paused: manifest.recording_paused,
        pty_child,
        recording: descriptors.recording,
    };
    let actor = SessionActor::spawn_adopted(size.rows, std::sync::Arc::new(|| {}), move || {
        SessionRuntime::adopt(adopted, default_vt_engine(&engine_session)?)
    })?;
    let mut hosted = HostedSession::from_actor(manifest.session, actor, hosting_epoch)?;
    hosted.applied_size = (size.cols, size.rows);
    if cell_pixel_size.0 > 0 && cell_pixel_size.1 > 0 {
        hosted.applied_cell_size = (u32::from(cell_pixel_size.0), u32::from(cell_pixel_size.1));
    }
    Ok(hosted)
}

impl StatusForwarder {
    /// Reap without blocking; forward the raw wait status once. EOF (drop)
    /// tells the adopter the status is unknown.
    fn poll(&mut self) -> bool {
        let mut status = 0;
        // SAFETY: waitpid writes one int; WNOHANG never blocks.
        let reaped = unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) };
        if reaped == 0 {
            return true;
        }
        if reaped == self.pid {
            use std::os::unix::process::ExitStatusExt;
            let _ = self.writer.set_write_timeout(Some(Duration::from_millis(250)));
            let _ = crate::child_observation::forward_status(&mut self.writer, std::process::ExitStatus::from_raw(status));
            let _ = self.writer.flush();
        }
        false
    }
}

/// Test hook: a target that refuses every adoption (debug builds only).
#[cfg(debug_assertions)]
fn maybe_refuse_adoption_for_test() -> Result<(), String> {
    match std::env::var("CLEAT_TEST_TRANSFER_REFUSE_ADOPTION") {
        Ok(reason) if !reason.is_empty() => Err(reason),
        _ => Ok(()),
    }
}

#[cfg(not(debug_assertions))]
fn maybe_refuse_adoption_for_test() -> Result<(), String> {
    Ok(())
}

/// Test hook: a source that sends a manifest version its target refuses.
#[cfg(debug_assertions)]
fn test_manifest_version() -> Option<u16> {
    std::env::var("CLEAT_TEST_TRANSFER_MANIFEST_VERSION").ok().and_then(|value| value.parse().ok())
}

#[cfg(not(debug_assertions))]
fn test_manifest_version() -> Option<u16> {
    None
}

pub(super) fn not_found_or_redirect(
    stream: &mut SessionStream,
    hub: &TransferHub,
    id: &str,
    holder_epoch: Option<u64>,
) -> Result<(), String> {
    match hub.redirect_for(id) {
        Some(redirect) => write_redirect(stream, redirect, holder_epoch.is_some_and(|epoch| epoch < redirect.hosting_epoch))
            .map_err(|err| format!("write HTTP redirect response: {err}")),
        None => write_http_not_found(stream),
    }
}
