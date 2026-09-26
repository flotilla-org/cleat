//! Versioned, hosting-neutral transfer schema. Unknown roles are preserved.
//! Adding an optional role/field does not bump the version; changing required
//! semantics does. Receivers must reject versions outside their support window.
use serde::{Deserialize, Serialize};

use crate::{
    recording::ReplaySnapshot,
    runtime::{SessionMetadata, TerminalSize},
};

pub const MANIFEST_VERSION: u16 = 1;
pub const MIN_SUPPORTED_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FdRole(String);
impl FdRole {
    pub fn new(role: impl Into<String>) -> Self {
        Self(role.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn pty_master() -> Self {
        Self::new("pty_master")
    }
    pub fn recording() -> Self {
        Self::new("recording")
    }
    pub fn pidfd() -> Self {
        Self::new("pidfd")
    }
    pub fn child_status() -> Self {
        Self::new("child_status")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FdManifestEntry {
    pub index: usize,
    pub role: FdRole,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FdTransferManifest {
    pub version: u16,
    pub min_supported_version: u16,
    pub fds: Vec<FdManifestEntry>,
    pub session: SessionMetadata,
    /// Current size, which may differ from session.initial_size.
    pub size: TerminalSize,
    /// Cell width and height in pixels; zero means unavailable.
    pub cell_pixel_size: (u16, u16),
    pub child_pid: u32,
    pub hosting_epoch: u64,
    /// Same JSON payload as recording event S, generated from replay_payload.
    pub replay_snapshot: ReplaySnapshot,
    /// Named recording markers and their cast offsets, so marker-relative
    /// capture keeps working on the adopter.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub markers: std::collections::HashMap<String, u64>,
    /// The recording is paused (`cleat record` off) rather than absent.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recording_paused: bool,
}

#[derive(Deserialize)]
pub(crate) struct VersionHeader {
    pub version: u16,
    pub min_supported_version: u16,
}
impl VersionHeader {
    pub fn validate(&self) -> Result<(), String> {
        if self.min_supported_version == 0
            || self.min_supported_version > self.version
            || !(MIN_SUPPORTED_VERSION..=MANIFEST_VERSION).contains(&self.version)
        {
            return Err(format!("unsupported manifest version {} (minimum {})", self.version, self.min_supported_version));
        }
        Ok(())
    }
}
