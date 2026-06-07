pub mod passthrough;
pub mod support;

#[cfg(feature = "ghostty-vt")]
pub mod ghostty;
#[cfg(feature = "ghostty-vt")]
mod ghostty_ffi;

use clap::ValueEnum;
use serde::{Deserialize, Serialize};
pub use support::{
    functional_vt_available, nonfunctional_build_error, vt_engine_label, vt_engine_status, BUILD_SUPPORT_MESSAGE, FUNCTIONAL_ENGINE_NAME,
    FUNCTIONAL_ENGINE_STATUS, NONFUNCTIONAL_BUILD_MESSAGE, PLACEHOLDER_ENGINE_STATUS, VT_ENGINE_HELP, VT_SUPPORT_POLICY,
};

use crate::provider::{
    DirtyState, TerminalRenderUpdate, TerminalScrollbackExtent, TerminalScrollbarState, TerminalSnapshot, TerminalViewportKind,
    ViewportCommand, ViewportCommandOutcome,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct ClientCapabilities {
    pub color_level: ColorLevel,
    pub kitty_keyboard: bool,
}

impl ClientCapabilities {
    pub fn new(color_level: ColorLevel, kitty_keyboard: bool) -> Self {
        Self { color_level, kitty_keyboard }
    }

    pub fn conservative_fallback() -> Self {
        Self::new(ColorLevel::Sixteen, false)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ColorLevel {
    Sixteen,
    Ansi256,
    #[default]
    TrueColor,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TerminalColors {
    pub default_foreground: Option<Rgb>,
    pub default_background: Option<Rgb>,
    pub default_cursor: Option<Rgb>,
}

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct CellFlags: u16 {
        const BOLD          = 1 << 0;
        const ITALIC        = 1 << 1;
        const FAINT         = 1 << 2;
        const BLINK         = 1 << 3;
        const INVERSE       = 1 << 4;
        const INVISIBLE     = 1 << 5;
        const STRIKETHROUGH = 1 << 6;
        const OVERLINE      = 1 << 7;
        const UNDERLINE     = 1 << 8;
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CellWidth {
    #[default]
    Narrow,
    Wide,
    SpacerTail,
    SpacerHead,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResolvedCell {
    pub graphemes: Vec<u32>,
    pub fg: Rgb,
    pub bg: Rgb,
    pub underline_color: Option<Rgb>,
    pub flags: CellFlags,
    pub underline_style: u32,
    pub width: CellWidth,
    pub protected: bool,
    pub semantic: u32,
    pub has_hyperlink: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CursorStyle {
    Bar,
    #[default]
    Block,
    Underline,
    BlockHollow,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CursorState {
    pub col: u16,
    pub row: u16,
    pub visible: bool,
    pub style: CursorStyle,
    pub blink: bool,
    pub wide_tail: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScreenGrid {
    pub cells: Vec<ResolvedCell>,
    pub cols: u16,
    pub rows: u16,
    pub cursor: CursorState,
    pub dirty_rows: Vec<u16>,
}

impl ScreenGrid {
    pub fn cell(&self, col: u16, row: u16) -> Option<&ResolvedCell> {
        if col < self.cols && row < self.rows {
            self.cells.get((row as usize) * (self.cols as usize) + (col as usize))
        } else {
            None
        }
    }

    pub fn row_text(&self, row: u16) -> String {
        if row >= self.rows {
            return String::new();
        }
        let start = (row as usize) * (self.cols as usize);
        let end = start + (self.cols as usize);
        let mut s = String::with_capacity(self.cols as usize);
        for cell in &self.cells[start..end] {
            if matches!(cell.width, CellWidth::SpacerTail | CellWidth::SpacerHead) {
                continue;
            }
            if cell.graphemes.is_empty() {
                s.push(' ');
            } else {
                for &cp in &cell.graphemes {
                    if let Some(ch) = char::from_u32(cp) {
                        s.push(ch);
                    }
                }
            }
        }
        s
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TerminalModeState {
    pub active_alternate_screen: bool,
    pub application_cursor_keys: bool,
    pub alternate_scroll: bool,
    pub mouse_tracking: bool,
    pub mouse_sgr: bool,
    pub mouse_sgr_pixels: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum VtEngineKind {
    Passthrough,
    Ghostty,
}

impl VtEngineKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passthrough => "passthrough",
            Self::Ghostty => "ghostty",
        }
    }

    pub fn ensure_available(self) -> Result<(), String> {
        match self {
            Self::Passthrough => Ok(()),
            Self::Ghostty => {
                #[cfg(feature = "ghostty-vt")]
                {
                    Ok(())
                }
                #[cfg(not(feature = "ghostty-vt"))]
                {
                    Err("vt engine ghostty is not compiled into this cleat build".to_string())
                }
            }
        }
    }
}

// NOTE: VtEngine is intentionally not Send. Engines may wrap foreign terminal-state
// handles that are only accessed from the single session daemon event loop.
pub trait VtEngine {
    fn feed(&mut self, bytes: &[u8]) -> Result<(), String>;
    fn resize(&mut self, cols: u16, rows: u16) -> Result<(), String>;
    fn supports_replay(&self) -> bool;
    fn replay_payload(&self, capabilities: &ClientCapabilities) -> Result<Option<Vec<u8>>, String>;
    fn screen_text(&self) -> Result<String, String>;
    fn screen_grid(&mut self) -> Result<ScreenGrid, String>;
    fn render_update(&mut self, dirty: DirtyState) -> Result<TerminalRenderUpdate, String> {
        let snapshot = TerminalSnapshot::from_screen_grid(self.screen_grid()?, dirty);
        Ok(TerminalRenderUpdate::from_snapshot(snapshot))
    }
    fn size(&self) -> (u16, u16);
    fn terminal_mode_state(&self) -> Result<TerminalModeState, String> {
        Ok(TerminalModeState::default())
    }
    fn scrollback_extent(&self) -> Result<TerminalScrollbackExtent, String> {
        let (_, rows) = self.size();
        Ok(TerminalScrollbackExtent { normal_scrollback_rows: 0, live_rows: rows, alternate_screen: false })
    }
    fn scrollbar_state(&self) -> Result<TerminalScrollbarState, String> {
        let extent = self.scrollback_extent()?;
        let kind = if extent.alternate_screen { TerminalViewportKind::LiveAlternate } else { TerminalViewportKind::LiveNormal };
        Ok(TerminalScrollbarState::for_live_viewport(kind, extent.live_rows))
    }
    fn scroll_viewport(&mut self, command: ViewportCommand) -> Result<ViewportCommandOutcome, String> {
        match command {
            ViewportCommand::Top | ViewportCommand::Bottom | ViewportCommand::DeltaRows(_) => Ok(ViewportCommandOutcome::NoOp),
        }
    }

    /// Reply bytes (DSR, DECRQM, DA, ...) the engine has buffered since the
    /// last call. Default is empty for engines that don't synthesize replies.
    fn drain_replies(&mut self) -> Vec<u8> {
        Vec::new()
    }
}

#[cfg(test)]
pub(crate) fn make_default_vt_engine(cols: u16, rows: u16) -> Box<dyn VtEngine> {
    make_vt_engine_with_colors(default_vt_engine_kind(), cols, rows, TerminalColors::default())
        .expect("default vt engine should always be available")
}

pub(crate) fn make_vt_engine_with_colors(
    kind: VtEngineKind,
    cols: u16,
    rows: u16,
    colors: TerminalColors,
) -> Result<Box<dyn VtEngine>, String> {
    kind.ensure_available()?;
    Ok(select_vt_engine(kind, cols, rows, colors))
}

pub fn default_vt_engine_kind() -> VtEngineKind {
    select_default_vt_engine_kind()
}

#[cfg(feature = "ghostty-vt")]
fn select_vt_engine(kind: VtEngineKind, cols: u16, rows: u16, colors: TerminalColors) -> Box<dyn VtEngine> {
    match kind {
        VtEngineKind::Passthrough => Box::new(passthrough::PassthroughVtEngine::new(cols, rows)),
        VtEngineKind::Ghostty => Box::new(ghostty::GhosttyVtEngine::new_with_colors(cols, rows, colors)),
    }
}

#[cfg(feature = "ghostty-vt")]
fn select_default_vt_engine_kind() -> VtEngineKind {
    VtEngineKind::Ghostty
}

#[cfg(not(feature = "ghostty-vt"))]
fn select_vt_engine(kind: VtEngineKind, cols: u16, rows: u16, _colors: TerminalColors) -> Box<dyn VtEngine> {
    match kind {
        VtEngineKind::Passthrough => Box::new(passthrough::PassthroughVtEngine::new(cols, rows)),
        VtEngineKind::Ghostty => unreachable!("availability check should reject ghostty when feature-disabled"),
    }
}

#[cfg(not(feature = "ghostty-vt"))]
fn select_default_vt_engine_kind() -> VtEngineKind {
    VtEngineKind::Passthrough
}

#[cfg(test)]
mod tests {
    use super::{TerminalColors, VtEngineKind};

    #[cfg(feature = "ghostty-vt")]
    #[test]
    fn ghostty_engine_smoke_constructs_resizes_and_drops() {
        let mut engine = super::make_default_vt_engine(80, 24);

        assert_eq!(super::default_vt_engine_kind(), VtEngineKind::Ghostty);
        assert_eq!(engine.size(), (80, 24));

        engine.resize(120, 40).expect("resize ghostty engine");

        assert_eq!(engine.size(), (120, 40));
    }

    #[test]
    fn passthrough_engine_is_always_available() {
        assert!(super::make_vt_engine_with_colors(VtEngineKind::Passthrough, 80, 24, TerminalColors::default()).is_ok());
    }

    #[cfg(not(feature = "ghostty-vt"))]
    #[test]
    fn ghostty_engine_is_rejected_when_feature_disabled() {
        let err = match super::make_vt_engine_with_colors(VtEngineKind::Ghostty, 80, 24, TerminalColors::default()) {
            Ok(_) => panic!("ghostty should be unavailable"),
            Err(err) => err,
        };
        assert!(err.contains("not compiled"));
    }

    #[cfg(feature = "ghostty-vt")]
    #[test]
    fn ghostty_engine_drains_da1_reply_after_feed() {
        let mut engine = super::make_default_vt_engine(80, 24);
        engine.feed(b"\x1b[c").expect("feed DA1");
        let reply = engine.drain_replies();
        assert_eq!(reply, b"\x1b[?62;22c".to_vec());
        // Second drain is empty — buffer is consumed.
        assert!(engine.drain_replies().is_empty());
    }

    #[cfg(feature = "ghostty-vt")]
    #[test]
    fn ghostty_engine_answers_cursor_position_report() {
        let mut engine = super::make_default_vt_engine(80, 24);
        // Move cursor to row 5, col 10 (1-based in CPR output), then ask.
        // ESC[5;10H = CUP, ESC[6n = DSR CPR.
        engine.feed(b"\x1b[5;10H\x1b[6n").expect("feed CUP+DSR");
        let reply = engine.drain_replies();
        assert_eq!(reply, b"\x1b[5;10R".to_vec());
    }
}
