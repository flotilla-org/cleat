//! Connect-only access to a forwarded or remote daemon's Unix socket.
//! No runtime layout is discovered, and no daemon is started or recovered.
use std::path::Path;

use crate::{
    packet::{ChannelRole, DirectorySnapshot, PacketClient},
    platform::ipc::SessionStream,
    protocol::AttachmentIdentity,
    session::ForegroundAttach,
};

pub fn connect_packets(socket: &Path) -> Result<(PacketClient<SessionStream>, DirectorySnapshot), String> {
    let (stream, directory) = connect(socket)?;
    Ok((PacketClient::new(stream), directory))
}

pub fn attach(socket: &Path, id: &str, identity: AttachmentIdentity, strict: bool, take: bool) -> Result<ForegroundAttach, String> {
    crate::runtime::validate_runtime_name(id)?;
    let (stream, directory) = connect(socket)?;
    crate::session::attach_packet_stream((stream, directory), id, identity, ChannelRole::Controller, strict, take, true)
}

pub(crate) fn ensure_supported() -> Result<(), String> {
    if !cfg!(unix) {
        return Err("--socket: Unix socket endpoints are unsupported on this platform".into());
    }
    Ok(())
}

fn connect(socket: &Path) -> Result<(SessionStream, DirectorySnapshot), String> {
    ensure_supported()?;
    crate::provider_daemon::connect_packet_endpoint(socket, &[], &crate::output_admission::remote_client_header()?)
}
