//! Build identity shared by CLI diagnostics and the daemon HTTP status endpoint.
use std::{fmt, sync::OnceLock};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildInfo {
    pub version: String,
    pub git_sha: Option<String>,
    /// Whether tracked files were modified at build time; None without Git.
    pub dirty: Option<bool>,
    pub profile: String,
    pub opt_level: String,
    pub target: String,
    pub protocol_version: u16,
    pub ghostty_vt: bool,
}

impl BuildInfo {
    pub fn current() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").into(),
            git_sha: match env!("CLEAT_GIT_SHA") {
                "unknown" => None,
                sha => Some(sha.into()),
            },
            dirty: env!("CLEAT_GIT_DIRTY").parse().ok(),
            profile: env!("CLEAT_BUILD_PROFILE").into(),
            opt_level: env!("CLEAT_BUILD_OPT_LEVEL").into(),
            target: env!("CLEAT_BUILD_TARGET").into(),
            protocol_version: crate::packet::PROTOCOL_VERSION,
            ghostty_vt: cfg!(feature = "ghostty-vt"),
        }
    }
}

impl fmt::Display for BuildInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sha = self.git_sha.as_deref().unwrap_or("unknown");
        let short_sha: String = sha.chars().take(12).collect();
        let dirty = match self.dirty {
            Some(true) => "-dirty",
            Some(false) => "",
            None => " (dirty unknown)",
        };
        write!(
            f,
            "{} ({}{}, {}, opt {}, {}, protocol {}, vt {})",
            self.version,
            short_sha,
            dirty,
            self.profile,
            self.opt_level,
            self.target,
            self.protocol_version,
            if self.ghostty_vt { "ghostty" } else { "passthrough" }
        )
    }
}

pub fn version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| BuildInfo::current().to_string())
}

/// Older daemons return the root status without build metadata.
#[derive(Debug, Deserialize)]
pub(crate) struct DaemonBuildStatus {
    #[serde(default)]
    pub generation: Option<u64>,
    #[serde(default)]
    pub build: Option<BuildInfo>,
    #[serde(default = "serving_state")]
    pub drain_state: String,
    #[serde(default)]
    pub session_count: usize,
}

pub(crate) fn serving_state() -> String {
    "serving".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_distinguishes_clean_dirty_and_unknown_builds() {
        let mut build = BuildInfo::current();
        build.git_sha = Some("0123456789abcdef".into());
        build.dirty = Some(false);
        assert!(build.to_string().contains("0123456789ab,"));
        build.dirty = Some(true);
        assert!(build.to_string().contains("0123456789ab-dirty,"));
        build.git_sha = None;
        build.dirty = None;
        assert!(build.to_string().contains("unknown (dirty unknown)"));
    }

    #[test]
    fn legacy_daemon_status_has_unknown_build() {
        let status: DaemonBuildStatus = serde_json::from_str(r#"{"service":"cleat-session","session":"default","ok":true}"#).unwrap();
        assert!(status.build.is_none());
    }
}
