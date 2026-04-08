use super::{
    ghostty_ffi::{
        self, GhosttyFormatterFormat, GhosttyFormatterTerminalOptions, GhosttyRenderStateCursorVisualStyle, GhosttyRenderStateDirty,
        GhosttyStyle, RenderStateHandle, RowCellsHandle, RowIteratorHandle, TerminalHandle,
    },
    CellFlags, ClientCapabilities, ColorLevel, CursorState, CursorStyle, ResolvedCell, Rgb, ScreenGrid, VtEngine,
};

const DEFAULT_MAX_SCROLLBACK: usize = 10_000;

pub struct GhosttyVtEngine {
    terminal: TerminalHandle,
    render_state: RenderStateHandle,
    cols: u16,
    rows: u16,
    saw_output: bool,
}

impl GhosttyVtEngine {
    pub fn new(cols: u16, rows: u16) -> Self {
        let terminal = TerminalHandle::new(cols, rows, DEFAULT_MAX_SCROLLBACK).expect("create ghostty terminal");
        let render_state = RenderStateHandle::new().expect("create ghostty render state");
        Self { terminal, render_state, cols, rows, saw_output: false }
    }

    fn read_cursor_state(&self) -> Result<CursorState, String> {
        let visible = self.render_state.get_cursor_visible()?;
        let in_viewport = self.render_state.get_cursor_viewport_has_value()?;

        if !visible || !in_viewport {
            return Ok(CursorState { visible, ..CursorState::default() });
        }

        let col = self.render_state.get_cursor_viewport_x()?;
        let row = self.render_state.get_cursor_viewport_y()?;
        let style = match self.render_state.get_cursor_visual_style()? {
            GhosttyRenderStateCursorVisualStyle::Bar => CursorStyle::Bar,
            GhosttyRenderStateCursorVisualStyle::Block => CursorStyle::Block,
            GhosttyRenderStateCursorVisualStyle::Underline => CursorStyle::Underline,
            GhosttyRenderStateCursorVisualStyle::BlockHollow => CursorStyle::BlockHollow,
        };

        Ok(CursorState { col, row, visible, style })
    }
}

impl VtEngine for GhosttyVtEngine {
    fn feed(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.terminal.feed(bytes);
        if !bytes.is_empty() {
            self.saw_output = true;
        }
        Ok(())
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<(), String> {
        self.terminal.resize(cols, rows)?;
        self.cols = cols;
        self.rows = rows;
        Ok(())
    }

    fn supports_replay(&self) -> bool {
        true
    }

    fn replay_payload(&self, capabilities: &ClientCapabilities) -> Result<Option<Vec<u8>>, String> {
        if !self.saw_output {
            return Ok(None);
        }
        let mut options = GhosttyFormatterTerminalOptions::init();
        options.emit = GhosttyFormatterFormat::Vt;
        options.extra.modes = true;
        options.extra.scrolling_region = true;
        options.extra.pwd = true;
        options.extra.keyboard = capabilities.kitty_keyboard;
        options.extra.screen.cursor = true;
        options.extra.screen.style = true;
        options.extra.screen.hyperlink = true;
        options.extra.screen.protection = true;
        options.extra.screen.kitty_keyboard = capabilities.kitty_keyboard;
        options.extra.screen.charsets = true;
        options.extra.palette = matches!(capabilities.color_level, ColorLevel::Ansi256 | ColorLevel::TrueColor);

        let payload = ghostty_ffi::format_terminal_alloc(self.terminal.raw(), options)?;
        Ok((!payload.is_empty()).then_some(payload))
    }

    fn screen_text(&self) -> Result<String, String> {
        let mut options = GhosttyFormatterTerminalOptions::init();
        options.emit = GhosttyFormatterFormat::Plain;
        let payload = ghostty_ffi::format_terminal_alloc(self.terminal.raw(), options)?;
        String::from_utf8(payload).map_err(|err| format!("ghostty plain-text snapshot was not valid utf-8: {err}"))
    }

    fn screen_grid(&mut self) -> Result<ScreenGrid, String> {
        self.render_state.update(&self.terminal)?;
        let cols = self.render_state.get_cols()?;
        let rows = self.render_state.get_rows()?;
        let colors = self.render_state.get_colors()?;

        let default_fg = Rgb { r: colors.foreground.r, g: colors.foreground.g, b: colors.foreground.b };
        let default_bg = Rgb { r: colors.background.r, g: colors.background.g, b: colors.background.b };

        let mut cells = Vec::with_capacity((cols as usize) * (rows as usize));

        let mut row_iter = RowIteratorHandle::new()?;
        self.render_state.populate_row_iterator(&mut row_iter)?;

        let mut row_cells = RowCellsHandle::new()?;
        while row_iter.next() {
            row_iter.populate_cells(&mut row_cells)?;
            while row_cells.next() {
                let graphemes_len = row_cells.get_graphemes_len()?;
                let graphemes = if graphemes_len > 0 {
                    let mut buf = vec![0u32; graphemes_len as usize];
                    row_cells.get_graphemes_buf(&mut buf)?;
                    buf
                } else {
                    Vec::new()
                };

                let fg = match row_cells.get_fg_color()? {
                    Some(c) => Rgb { r: c.r, g: c.g, b: c.b },
                    None => default_fg,
                };
                let bg = match row_cells.get_bg_color()? {
                    Some(c) => Rgb { r: c.r, g: c.g, b: c.b },
                    None => default_bg,
                };

                let style = row_cells.get_style()?;
                let flags = flags_from_ghostty_style(&style);

                cells.push(ResolvedCell { graphemes, fg, bg, flags });
            }
        }

        let cursor = self.read_cursor_state()?;

        // Clear dirty state — cleat is the renderer.
        self.render_state.set_dirty(GhosttyRenderStateDirty::False)?;

        Ok(ScreenGrid { cells, cols, rows, cursor })
    }

    fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }
}

fn flags_from_ghostty_style(style: &GhosttyStyle) -> CellFlags {
    let mut flags = CellFlags::empty();
    if style.bold {
        flags |= CellFlags::BOLD;
    }
    if style.italic {
        flags |= CellFlags::ITALIC;
    }
    if style.faint {
        flags |= CellFlags::FAINT;
    }
    if style.blink {
        flags |= CellFlags::BLINK;
    }
    if style.inverse {
        flags |= CellFlags::INVERSE;
    }
    if style.invisible {
        flags |= CellFlags::INVISIBLE;
    }
    if style.strikethrough {
        flags |= CellFlags::STRIKETHROUGH;
    }
    if style.overline {
        flags |= CellFlags::OVERLINE;
    }
    if style.underline != 0 {
        flags |= CellFlags::UNDERLINE;
    }
    flags
}
