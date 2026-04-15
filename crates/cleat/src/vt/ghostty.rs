use super::{
    ghostty_ffi::{
        self, GhosttyCellWide, GhosttyFormatterFormat, GhosttyFormatterTerminalOptions, GhosttyRenderStateCursorVisualStyle,
        GhosttyRenderStateDirty, GhosttyStyle, RenderStateHandle, RowCellsHandle, RowIteratorHandle, TerminalHandle,
    },
    CellFlags, CellWidth, ClientCapabilities, ColorLevel, CursorState, CursorStyle, ResolvedCell, Rgb, ScreenGrid, VtEngine,
};

const DEFAULT_MAX_SCROLLBACK: usize = 10_000;

pub struct GhosttyVtEngine {
    terminal: TerminalHandle,
    render_state: RenderStateHandle,
    row_iter: RowIteratorHandle,
    row_cells: RowCellsHandle,
    cols: u16,
    rows: u16,
    saw_output: bool,
    cached_grid: Option<ScreenGrid>,
}

impl GhosttyVtEngine {
    pub fn new(cols: u16, rows: u16) -> Self {
        let terminal = TerminalHandle::new(cols, rows, DEFAULT_MAX_SCROLLBACK).expect("create ghostty terminal");
        let render_state = RenderStateHandle::new().expect("create ghostty render state");
        let row_iter = RowIteratorHandle::new().expect("create ghostty row iterator");
        let row_cells = RowCellsHandle::new().expect("create ghostty row cells");
        Self { terminal, render_state, row_iter, row_cells, cols, rows, saw_output: false, cached_grid: None }
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

        let wide_tail = self.render_state.get_cursor_viewport_wide_tail()?;

        Ok(CursorState { col, row, visible, style, wide_tail })
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

        let dirty = self.render_state.get_dirty()?;
        if dirty == GhosttyRenderStateDirty::False {
            if let Some(ref cached) = self.cached_grid {
                return Ok(cached.clone());
            }
        }

        let cols = self.render_state.get_cols()?;
        let rows = self.render_state.get_rows()?;
        let colors = self.render_state.get_colors()?;

        let default_fg = Rgb { r: colors.foreground.r, g: colors.foreground.g, b: colors.foreground.b };
        let default_bg = Rgb { r: colors.background.r, g: colors.background.g, b: colors.background.b };

        let mut partial = dirty == GhosttyRenderStateDirty::Partial;
        let row_stride = cols as usize;

        // Reuse the cached cell vec when doing a partial update.
        let mut cells = if partial { self.cached_grid.take().map(|g| g.cells).unwrap_or_default() } else { Vec::new() };
        if cells.len() != row_stride * (rows as usize) {
            // Dimensions changed or no cache — force a full rebuild.
            partial = false;
            cells.clear();
            cells.reserve(row_stride * (rows as usize));
        }

        self.render_state.populate_row_iterator(&mut self.row_iter)?;

        let mut row_idx: usize = 0;
        while self.row_iter.next() {
            let skip = partial && !self.row_iter.get_dirty().unwrap_or(true);
            if skip {
                row_idx += 1;
                continue;
            }

            self.row_iter.populate_cells(&mut self.row_cells)?;
            let row_start = row_idx * row_stride;
            let mut col_idx: usize = 0;
            while self.row_cells.next() {
                let graphemes_len = self.row_cells.get_graphemes_len()?;
                let graphemes = if graphemes_len > 0 {
                    let mut buf = vec![0u32; graphemes_len as usize];
                    self.row_cells.get_graphemes_buf(&mut buf)?;
                    buf
                } else {
                    Vec::new()
                };

                let fg = match self.row_cells.get_fg_color()? {
                    Some(c) => Rgb { r: c.r, g: c.g, b: c.b },
                    None => default_fg,
                };
                let bg = match self.row_cells.get_bg_color()? {
                    Some(c) => Rgb { r: c.r, g: c.g, b: c.b },
                    None => default_bg,
                };

                let style = self.row_cells.get_style()?;
                let flags = flags_from_ghostty_style(&style);

                let width = match self.row_cells.get_wide()? {
                    GhosttyCellWide::Narrow => CellWidth::Narrow,
                    GhosttyCellWide::Wide => CellWidth::Wide,
                    GhosttyCellWide::SpacerTail => CellWidth::SpacerTail,
                    GhosttyCellWide::SpacerHead => CellWidth::SpacerHead,
                };

                let cell = ResolvedCell { graphemes, fg, bg, flags, width };
                let idx = row_start + col_idx;
                if idx < cells.len() {
                    cells[idx] = cell;
                } else {
                    cells.push(cell);
                }
                col_idx += 1;
            }
            row_idx += 1;
        }

        let cursor = self.read_cursor_state()?;

        self.render_state.set_dirty(GhosttyRenderStateDirty::False)?;

        let grid = ScreenGrid { cells, cols, rows, cursor };
        self.cached_grid = Some(grid.clone());
        Ok(grid)
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
        // 0 = no underline; non-zero values are single/double/curly/dotted/dashed
        flags |= CellFlags::UNDERLINE;
    }
    flags
}
