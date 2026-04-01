use super::VtEngineKind;

pub const FUNCTIONAL_ENGINE_NAME: &str = "ghostty";
pub const FUNCTIONAL_ENGINE_STATUS: &str = "functional";
pub const PLACEHOLDER_ENGINE_STATUS: &str = "placeholder";
pub const VT_SUPPORT_POLICY: &str =
    "Ghostty is currently the only functional VT engine; placeholder engines are for testing/development only.";
pub const NONFUNCTIONAL_BUILD_MESSAGE: &str = "This cleat binary was built without ghostty-vt and is non-functional for real use. Ghostty is currently the only functional VT engine; passthrough is placeholder/test-only.";
pub const FUNCTIONAL_BUILD_MESSAGE: &str = "Ghostty is currently the only functional VT engine.";
pub const VT_ENGINE_HELP: &str =
    "VT engine. Ghostty is currently the only functional engine; placeholder engines are for testing/development only.";

#[cfg(feature = "ghostty-vt")]
pub const BUILD_SUPPORT_MESSAGE: &str = FUNCTIONAL_BUILD_MESSAGE;
#[cfg(not(feature = "ghostty-vt"))]
pub const BUILD_SUPPORT_MESSAGE: &str = NONFUNCTIONAL_BUILD_MESSAGE;

pub fn functional_vt_available() -> bool {
    option_env!("CLEAT_FUNCTIONAL_VT_AVAILABLE") == Some("1")
}

pub fn build_support_message() -> &'static str {
    BUILD_SUPPORT_MESSAGE
}

pub const fn vt_engine_status(engine: VtEngineKind) -> &'static str {
    match engine {
        VtEngineKind::Ghostty => FUNCTIONAL_ENGINE_STATUS,
        VtEngineKind::Passthrough => PLACEHOLDER_ENGINE_STATUS,
    }
}

pub fn vt_engine_label(engine: VtEngineKind) -> String {
    format!("{} ({})", engine.as_str(), vt_engine_status(engine))
}

pub fn nonfunctional_build_error() -> String {
    "this cleat binary was built without ghostty-vt and is non-functional for real terminal usage; run ./tools/prepare-ghostty-vt.sh and rebuild with --features ghostty-vt".to_string()
}
