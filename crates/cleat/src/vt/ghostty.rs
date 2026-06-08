use super::{
    ghostty_ffi::{
        self, GhosttyCellContentTag, GhosttyCellSemanticContent, GhosttyCellWide, GhosttyFormatterFormat, GhosttyFormatterTerminalOptions,
        GhosttyRenderStateCursorVisualStyle, GhosttyRenderStateDirty, GhosttyRowData, GhosttyRowSemanticPrompt, GhosttyStyle,
        GhosttyStyleColor, GhosttyStyleColorTag, GhosttyTerminalScreen, GhosttyTerminalScrollViewport, RenderStateHandle, RowCellsHandle,
        RowIteratorHandle, TerminalHandle, GHOSTTY_MODE_ALT_SCROLL, GHOSTTY_MODE_DECCKM, GHOSTTY_MODE_SGR_MOUSE,
        GHOSTTY_MODE_SGR_PIXELS_MOUSE,
    },
    CellFlags, CellWidth, ClientCapabilities, ColorLevel, CursorState, CursorStyle, ResolvedCell, Rgb, ScreenGrid, TerminalColors,
    TerminalModeState, VtEngine,
};
use crate::provider::{
    DirtyState, TerminalCellFlags, TerminalCellWidth, TerminalCursor as ProviderCursor, TerminalCursorStyle as ProviderCursorStyle,
    TerminalImagePlacement, TerminalImageResource, TerminalRenderCell, TerminalRenderRow, TerminalRenderStyle, TerminalRenderUpdate,
    TerminalRenderUpdateOp, TerminalRenderUpdateOpKind, TerminalRgb, TerminalScrollbackExtent, TerminalScrollbarState, TerminalStyleColor,
    TerminalStyleColorTag, TerminalViewportKind, ViewportCommand, ViewportCommandOutcome,
};

const DEFAULT_MAX_SCROLLBACK: usize = 10_000;
// Match libghostty-vt's own default (320 MB). A lower limit silently evicts the
// oldest images (placements included) once the decoded RGBA footprint exceeds
// it. For example, three 2269x2620 images are ~22.7 MB each = ~68 MB, which a
// 64 MB cap would push the oldest placement out of.
const DEFAULT_KITTY_IMAGE_STORAGE_LIMIT: u64 = 320 * 1000 * 1000;

pub struct GhosttyVtEngine {
    terminal: TerminalHandle,
    render_state: RenderStateHandle,
    row_iter: RowIteratorHandle,
    row_cells: RowCellsHandle,
    cols: u16,
    rows: u16,
    cell_width_px: u32,
    cell_height_px: u32,
    saw_output: bool,
    cached_grid: Option<ScreenGrid>,
}

impl GhosttyVtEngine {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self::new_with_colors(cols, rows, TerminalColors::default())
    }

    pub fn new_with_colors(cols: u16, rows: u16, colors: TerminalColors) -> Self {
        let terminal = TerminalHandle::new(cols, rows, DEFAULT_MAX_SCROLLBACK).expect("create ghostty terminal");
        let mut terminal = terminal;
        apply_colors(&mut terminal, colors).expect("configure ghostty terminal colors");
        terminal.set_kitty_image_storage_limit(DEFAULT_KITTY_IMAGE_STORAGE_LIMIT).expect("configure ghostty kitty image storage");
        let render_state = RenderStateHandle::new().expect("create ghostty render state");
        let row_iter = RowIteratorHandle::new().expect("create ghostty row iterator");
        let row_cells = RowCellsHandle::new().expect("create ghostty row cells");
        Self {
            terminal,
            render_state,
            row_iter,
            row_cells,
            cols,
            rows,
            cell_width_px: 1,
            cell_height_px: 1,
            saw_output: false,
            cached_grid: None,
        }
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

        let blink = self.render_state.get_cursor_blinking()?;
        let wide_tail = self.render_state.get_cursor_viewport_wide_tail()?;

        Ok(CursorState { col, row, visible, style, blink, wide_tail })
    }

    fn read_render_row(
        &mut self,
        row: u16,
        cols: u16,
        raw_row: ghostty_ffi::GhosttyRow,
        default_fg: Rgb,
        default_bg: Rgb,
        dirty: bool,
    ) -> Result<(TerminalRenderRow, Vec<ResolvedCell>), String> {
        self.row_iter.populate_cells(&mut self.row_cells)?;

        let mut render_cells = Vec::with_capacity(cols as usize);
        let mut resolved_cells = Vec::with_capacity(cols as usize);
        while self.row_cells.next() {
            let graphemes_len = self.row_cells.get_graphemes_len()?;
            let graphemes = if graphemes_len > 0 {
                let mut buf = vec![0u32; graphemes_len as usize];
                self.row_cells.get_graphemes_buf(&mut buf)?;
                buf
            } else {
                Vec::new()
            };

            let resolved_fg = self.row_cells.get_fg_color()?.map(rgb_from_ghostty).unwrap_or(default_fg);
            let resolved_bg = self.row_cells.get_bg_color()?.map(rgb_from_ghostty).unwrap_or(default_bg);
            let style = self.row_cells.get_style()?;
            let flags = flags_from_ghostty_style(&style);
            let underline_style = u32::try_from(style.underline).unwrap_or(0);
            let underline_color = rgb_from_ghostty_style_color(style.underline_color);
            let protected = self.row_cells.get_protected()?;
            let has_hyperlink = self.row_cells.get_has_hyperlink()?;
            let semantic = semantic_from_ghostty(self.row_cells.get_semantic_content()?);
            let width = cell_width_from_ghostty(self.row_cells.get_wide()?);

            resolved_cells.push(ResolvedCell {
                graphemes: graphemes.clone(),
                fg: resolved_fg,
                bg: resolved_bg,
                underline_color,
                flags,
                underline_style,
                width,
                protected,
                semantic,
                has_hyperlink,
            });

            render_cells.push(TerminalRenderCell {
                graphemes,
                style: TerminalRenderStyle {
                    flags: terminal_cell_flags_from_vt(flags),
                    width: terminal_cell_width_from_vt(width),
                    resolved_fg: terminal_rgb_from_rgb(resolved_fg),
                    resolved_bg: terminal_rgb_from_rgb(resolved_bg),
                    fg_color: terminal_style_color_from_ghostty(style.fg_color),
                    bg_color: terminal_style_color_from_ghostty(style.bg_color),
                    underline_style,
                    underline_color: terminal_style_color_from_ghostty(style.underline_color),
                    protected,
                    semantic,
                    has_hyperlink,
                    hyperlink_id: 0,
                    content_tag: content_tag_from_ghostty(self.row_cells.get_content_tag()?),
                    has_text: self.row_cells.get_has_text()?,
                    has_styling: self.row_cells.get_has_styling()?,
                    style_id: self.row_cells.get_style_id()?,
                },
            });
        }

        Ok((
            TerminalRenderRow {
                row,
                col_count: cols,
                cells: render_cells,
                wrap: ghostty_ffi::row_get_bool(raw_row, GhosttyRowData::Wrap, "ghostty_row_get(Wrap)")?,
                wrap_continuation: ghostty_ffi::row_get_bool(
                    raw_row,
                    GhosttyRowData::WrapContinuation,
                    "ghostty_row_get(WrapContinuation)",
                )?,
                has_graphemes: ghostty_ffi::row_get_bool(raw_row, GhosttyRowData::Grapheme, "ghostty_row_get(Grapheme)")?,
                has_styling: ghostty_ffi::row_get_bool(raw_row, GhosttyRowData::Styled, "ghostty_row_get(Styled)")?,
                has_hyperlink: ghostty_ffi::row_get_bool(raw_row, GhosttyRowData::Hyperlink, "ghostty_row_get(Hyperlink)")?,
                semantic_prompt: row_semantic_prompt_from_ghostty(ghostty_ffi::row_get_semantic_prompt(raw_row)?),
                has_kitty_virtual_placeholder: ghostty_ffi::row_get_bool(
                    raw_row,
                    GhosttyRowData::KittyVirtualPlaceholder,
                    "ghostty_row_get(KittyVirtualPlaceholder)",
                )?,
                dirty,
            },
            resolved_cells,
        ))
    }
}

fn apply_colors(terminal: &mut TerminalHandle, colors: TerminalColors) -> Result<(), String> {
    terminal.set_default_foreground(colors.default_foreground.map(rgb_to_ghostty))?;
    terminal.set_default_background(colors.default_background.map(rgb_to_ghostty))?;
    terminal.set_default_cursor(colors.default_cursor.map(rgb_to_ghostty))?;
    Ok(())
}

fn rgb_to_ghostty(rgb: Rgb) -> ghostty_ffi::GhosttyColorRgb {
    ghostty_ffi::GhosttyColorRgb { r: rgb.r, g: rgb.g, b: rgb.b }
}

impl VtEngine for GhosttyVtEngine {
    fn feed(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.terminal.feed(bytes);
        if !bytes.is_empty() {
            self.saw_output = true;
        }
        Ok(())
    }

    fn drain_replies(&mut self) -> Vec<u8> {
        self.terminal.drain_replies()
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<(), String> {
        self.terminal.resize(cols, rows, self.cell_width_px, self.cell_height_px)?;
        self.cols = cols;
        self.rows = rows;
        Ok(())
    }

    fn set_cell_size(&mut self, cell_width_px: u32, cell_height_px: u32) -> Result<(), String> {
        self.cell_width_px = cell_width_px.max(1);
        self.cell_height_px = cell_height_px.max(1);
        self.terminal.resize(self.cols, self.rows, self.cell_width_px, self.cell_height_px)
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
                let mut grid = cached.clone();
                grid.cursor = self.read_cursor_state()?;
                self.cached_grid = Some(grid.clone());
                return Ok(grid);
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
        let mut dirty_rows = Vec::new();
        while self.row_iter.next() {
            let skip = partial && !self.row_iter.get_dirty().unwrap_or(true);
            if skip {
                row_idx += 1;
                continue;
            }
            if partial {
                dirty_rows.push(u16::try_from(row_idx).unwrap_or(u16::MAX));
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
                let underline_style = u32::try_from(style.underline).unwrap_or(0);
                let underline_color = rgb_from_ghostty_style_color(style.underline_color);
                let protected = self.row_cells.get_protected()?;
                let has_hyperlink = self.row_cells.get_has_hyperlink()?;
                let semantic = match self.row_cells.get_semantic_content()? {
                    GhosttyCellSemanticContent::Output => 0,
                    GhosttyCellSemanticContent::Input => 1,
                    GhosttyCellSemanticContent::Prompt => 2,
                };

                let width = match self.row_cells.get_wide()? {
                    GhosttyCellWide::Narrow => CellWidth::Narrow,
                    GhosttyCellWide::Wide => CellWidth::Wide,
                    GhosttyCellWide::SpacerTail => CellWidth::SpacerTail,
                    GhosttyCellWide::SpacerHead => CellWidth::SpacerHead,
                };

                let cell =
                    ResolvedCell { graphemes, fg, bg, underline_color, flags, underline_style, width, protected, semantic, has_hyperlink };
                let idx = row_start + col_idx;
                if idx < cells.len() {
                    cells[idx] = cell;
                } else {
                    cells.push(cell);
                }
                col_idx += 1;
            }
            self.row_iter.set_dirty(false)?;
            row_idx += 1;
        }

        let cursor = self.read_cursor_state()?;

        self.render_state.set_dirty(GhosttyRenderStateDirty::False)?;

        let grid = ScreenGrid { cells, cols, rows, cursor, dirty_rows };
        self.cached_grid = Some(grid.clone());
        Ok(grid)
    }

    fn render_update(&mut self, dirty: DirtyState) -> Result<TerminalRenderUpdate, String> {
        self.render_state.update(&self.terminal)?;

        let render_dirty = self.render_state.get_dirty()?;
        let cols = self.render_state.get_cols()?;
        let rows = self.render_state.get_rows()?;
        let colors = self.render_state.get_colors()?;
        let default_fg = rgb_from_ghostty(colors.foreground);
        let default_bg = rgb_from_ghostty(colors.background);
        let had_cache = self.cached_grid.is_some();
        let effective_dirty = effective_render_dirty(dirty, render_dirty, had_cache);
        let row_stride = cols as usize;
        let expected_cell_count = row_stride * rows as usize;

        let mut cached_cells = match self.cached_grid.take() {
            Some(grid) if grid.cols == cols && grid.rows == rows && grid.cells.len() == expected_cell_count => grid.cells,
            _ => vec![ResolvedCell::default(); expected_cell_count],
        };

        let mut update_rows = Vec::new();
        let mut dirty_rows = Vec::new();
        if effective_dirty != DirtyState::Clean {
            self.render_state.populate_row_iterator(&mut self.row_iter)?;
            let mut row_idx: usize = 0;
            while self.row_iter.next() {
                let row_dirty = self.row_iter.get_dirty().unwrap_or(true);
                let include_row = match effective_dirty {
                    DirtyState::Clean => false,
                    DirtyState::Partial => row_dirty,
                    DirtyState::Full => true,
                };
                if include_row {
                    let row = u16::try_from(row_idx).unwrap_or(u16::MAX);
                    let raw_row = self.row_iter.get_raw_row()?;
                    let (render_row, resolved_cells) = self.read_render_row(row, cols, raw_row, default_fg, default_bg, row_dirty)?;
                    let row_start = row_idx * row_stride;
                    for (offset, cell) in resolved_cells.into_iter().enumerate() {
                        let idx = row_start + offset;
                        if idx < cached_cells.len() {
                            cached_cells[idx] = cell;
                        }
                    }
                    if effective_dirty == DirtyState::Partial {
                        dirty_rows.push(row);
                    }
                    update_rows.push(render_row);
                    self.row_iter.set_dirty(false)?;
                }
                row_idx += 1;
            }
            self.render_state.set_dirty(GhosttyRenderStateDirty::False)?;
        }

        let effective_dirty =
            if effective_dirty == DirtyState::Partial && update_rows.is_empty() { DirtyState::Clean } else { effective_dirty };
        let cursor = self.read_cursor_state()?;
        self.cached_grid = Some(ScreenGrid { cells: cached_cells, cols, rows, cursor, dirty_rows });

        let ops = match effective_dirty {
            DirtyState::Clean => Vec::new(),
            DirtyState::Full => vec![TerminalRenderUpdateOp {
                kind: TerminalRenderUpdateOpKind::FullVisibleReplace,
                first_row: 0,
                row_count: rows,
                col_count: cols,
                rows: update_rows,
                src_row: 0,
                dst_row: 0,
            }],
            DirtyState::Partial => update_rows
                .into_iter()
                .map(|row| TerminalRenderUpdateOp {
                    kind: TerminalRenderUpdateOpKind::RowReplace,
                    first_row: row.row,
                    row_count: 1,
                    col_count: cols,
                    rows: vec![row],
                    src_row: 0,
                    dst_row: 0,
                })
                .collect(),
        };

        let (image_resources, image_placements) = self.terminal.kitty_image_state()?;

        Ok(TerminalRenderUpdate {
            cols,
            rows,
            cursor: provider_cursor_from_vt(cursor),
            dirty: effective_dirty,
            ops,
            image_resources: image_resources
                .into_iter()
                .map(|resource| TerminalImageResource {
                    image_id: resource.image_id,
                    generation: resource.generation,
                    width_px: resource.width_px,
                    height_px: resource.height_px,
                    format: resource.format,
                    compression: resource.compression,
                    data_len: resource.data_len,
                })
                .collect(),
            image_placements: image_placements
                .into_iter()
                .map(|placement| TerminalImagePlacement {
                    image_id: placement.image_id,
                    generation: placement.generation,
                    placement_id: placement.placement_id,
                    z: placement.z,
                    viewport_col: placement.viewport_col,
                    viewport_row: placement.viewport_row,
                    grid_cols: placement.grid_cols,
                    grid_rows: placement.grid_rows,
                    pixel_width: placement.pixel_width,
                    pixel_height: placement.pixel_height,
                    source_x: placement.source_x,
                    source_y: placement.source_y,
                    source_width: placement.source_width,
                    source_height: placement.source_height,
                    x_offset_px: placement.x_offset_px,
                    y_offset_px: placement.y_offset_px,
                })
                .collect(),
            ..TerminalRenderUpdate::default()
        })
    }

    fn with_image_resource_data(
        &mut self,
        image_id: u32,
        generation: u64,
        callback: &mut dyn FnMut(&[u8]) -> bool,
    ) -> Result<bool, String> {
        self.terminal.with_kitty_image_data(image_id, generation, callback)
    }

    fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    fn terminal_mode_state(&self) -> Result<TerminalModeState, String> {
        Ok(TerminalModeState {
            active_alternate_screen: self.terminal.active_screen()? == GhosttyTerminalScreen::Alternate,
            application_cursor_keys: self.terminal.mode_enabled(GHOSTTY_MODE_DECCKM)?,
            alternate_scroll: self.terminal.mode_enabled(GHOSTTY_MODE_ALT_SCROLL)?,
            mouse_tracking: self.terminal.mouse_tracking()?,
            mouse_sgr: self.terminal.mode_enabled(GHOSTTY_MODE_SGR_MOUSE)?,
            mouse_sgr_pixels: self.terminal.mode_enabled(GHOSTTY_MODE_SGR_PIXELS_MOUSE)?,
        })
    }

    fn scrollback_extent(&self) -> Result<TerminalScrollbackExtent, String> {
        Ok(TerminalScrollbackExtent {
            normal_scrollback_rows: self.terminal.scrollback_rows()? as u64,
            live_rows: self.rows,
            alternate_screen: self.terminal.active_screen()? == GhosttyTerminalScreen::Alternate,
        })
    }

    fn scrollbar_state(&self) -> Result<TerminalScrollbarState, String> {
        let scrollbar = self.terminal.scrollbar()?;
        let active_screen = self.terminal.active_screen()?;
        let viewport_rows = u16::try_from(scrollbar.len).unwrap_or(u16::MAX);
        let initial_kind = if active_screen == GhosttyTerminalScreen::Alternate {
            TerminalViewportKind::LiveAlternate
        } else {
            TerminalViewportKind::LiveNormal
        };
        let state = TerminalScrollbarState::new(initial_kind, scrollbar.total, viewport_rows, scrollbar.offset);
        let viewport_kind = match active_screen {
            GhosttyTerminalScreen::Alternate => TerminalViewportKind::LiveAlternate,
            GhosttyTerminalScreen::Primary if state.at_bottom => TerminalViewportKind::LiveNormal,
            GhosttyTerminalScreen::Primary => TerminalViewportKind::NormalScrollback,
        };
        Ok(TerminalScrollbarState { viewport_kind, ..state })
    }

    fn scroll_viewport(&mut self, command: ViewportCommand) -> Result<ViewportCommandOutcome, String> {
        if self.terminal.active_screen()? == GhosttyTerminalScreen::Alternate {
            return Ok(ViewportCommandOutcome::Unsupported);
        }
        if matches!(command, ViewportCommand::DeltaRows(0)) {
            return Ok(ViewportCommandOutcome::NoOp);
        }

        let before = self.terminal.scrollbar()?;
        let behavior = match command {
            ViewportCommand::Top => GhosttyTerminalScrollViewport::top(),
            ViewportCommand::Bottom => GhosttyTerminalScrollViewport::bottom(),
            ViewportCommand::DeltaRows(delta) => {
                let delta = isize::try_from(delta).unwrap_or(if delta.is_negative() { isize::MIN } else { isize::MAX });
                GhosttyTerminalScrollViewport::delta(delta)
            }
        };
        self.terminal.scroll_viewport(behavior);
        let after = self.terminal.scrollbar()?;
        if before == after {
            Ok(ViewportCommandOutcome::NoOp)
        } else {
            self.cached_grid = None;
            Ok(ViewportCommandOutcome::Moved)
        }
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

fn effective_render_dirty(requested: DirtyState, render_dirty: GhosttyRenderStateDirty, has_cache: bool) -> DirtyState {
    if !has_cache {
        return DirtyState::Full;
    }
    match (requested, render_dirty) {
        (DirtyState::Full, _) | (_, GhosttyRenderStateDirty::Full) => DirtyState::Full,
        (DirtyState::Clean, GhosttyRenderStateDirty::False) => DirtyState::Clean,
        (_, GhosttyRenderStateDirty::Partial) | (DirtyState::Partial, GhosttyRenderStateDirty::False) => DirtyState::Partial,
    }
}

fn rgb_from_ghostty(rgb: ghostty_ffi::GhosttyColorRgb) -> Rgb {
    Rgb { r: rgb.r, g: rgb.g, b: rgb.b }
}

fn terminal_rgb_from_rgb(rgb: Rgb) -> TerminalRgb {
    TerminalRgb { r: rgb.r, g: rgb.g, b: rgb.b }
}

fn rgb_from_ghostty_style_color(color: GhosttyStyleColor) -> Option<Rgb> {
    if color.tag == GhosttyStyleColorTag::Rgb {
        let rgb = unsafe { color.value.rgb };
        Some(Rgb { r: rgb.r, g: rgb.g, b: rgb.b })
    } else {
        None
    }
}

fn terminal_style_color_from_ghostty(color: GhosttyStyleColor) -> TerminalStyleColor {
    match color.tag {
        GhosttyStyleColorTag::None => TerminalStyleColor::default(),
        GhosttyStyleColorTag::Palette => {
            TerminalStyleColor { tag: TerminalStyleColorTag::Palette, palette_index: unsafe { color.value.palette }, rgb: None }
        }
        GhosttyStyleColorTag::Rgb => {
            let rgb = unsafe { color.value.rgb };
            TerminalStyleColor::rgb(TerminalRgb { r: rgb.r, g: rgb.g, b: rgb.b })
        }
    }
}

fn cell_width_from_ghostty(width: GhosttyCellWide) -> CellWidth {
    match width {
        GhosttyCellWide::Narrow => CellWidth::Narrow,
        GhosttyCellWide::Wide => CellWidth::Wide,
        GhosttyCellWide::SpacerTail => CellWidth::SpacerTail,
        GhosttyCellWide::SpacerHead => CellWidth::SpacerHead,
    }
}

fn terminal_cell_width_from_vt(width: CellWidth) -> TerminalCellWidth {
    match width {
        CellWidth::Narrow => TerminalCellWidth::Narrow,
        CellWidth::Wide => TerminalCellWidth::Wide,
        CellWidth::SpacerTail => TerminalCellWidth::SpacerTail,
        CellWidth::SpacerHead => TerminalCellWidth::SpacerHead,
    }
}

fn terminal_cell_flags_from_vt(flags: CellFlags) -> TerminalCellFlags {
    let mut out = TerminalCellFlags::empty();
    if flags.contains(CellFlags::BOLD) {
        out |= TerminalCellFlags::BOLD;
    }
    if flags.contains(CellFlags::ITALIC) {
        out |= TerminalCellFlags::ITALIC;
    }
    if flags.contains(CellFlags::FAINT) {
        out |= TerminalCellFlags::FAINT;
    }
    if flags.contains(CellFlags::BLINK) {
        out |= TerminalCellFlags::BLINK;
    }
    if flags.contains(CellFlags::INVERSE) {
        out |= TerminalCellFlags::INVERSE;
    }
    if flags.contains(CellFlags::INVISIBLE) {
        out |= TerminalCellFlags::INVISIBLE;
    }
    if flags.contains(CellFlags::STRIKETHROUGH) {
        out |= TerminalCellFlags::STRIKETHROUGH;
    }
    if flags.contains(CellFlags::OVERLINE) {
        out |= TerminalCellFlags::OVERLINE;
    }
    if flags.contains(CellFlags::UNDERLINE) {
        out |= TerminalCellFlags::UNDERLINE;
    }
    out
}

fn semantic_from_ghostty(semantic: GhosttyCellSemanticContent) -> u32 {
    match semantic {
        GhosttyCellSemanticContent::Output => 0,
        GhosttyCellSemanticContent::Input => 1,
        GhosttyCellSemanticContent::Prompt => 2,
    }
}

fn content_tag_from_ghostty(tag: GhosttyCellContentTag) -> u32 {
    match tag {
        GhosttyCellContentTag::Codepoint => 0,
        GhosttyCellContentTag::CodepointGrapheme => 1,
        GhosttyCellContentTag::BgColorPalette => 2,
        GhosttyCellContentTag::BgColorRgb => 3,
    }
}

fn row_semantic_prompt_from_ghostty(semantic: GhosttyRowSemanticPrompt) -> u32 {
    match semantic {
        GhosttyRowSemanticPrompt::None => 0,
        GhosttyRowSemanticPrompt::Prompt => 1,
        GhosttyRowSemanticPrompt::PromptContinuation => 2,
    }
}

fn provider_cursor_from_vt(cursor: CursorState) -> ProviderCursor {
    ProviderCursor {
        col: cursor.col,
        row: cursor.row,
        visible: cursor.visible,
        style: match cursor.style {
            CursorStyle::Bar => ProviderCursorStyle::Bar,
            CursorStyle::Block => ProviderCursorStyle::Block,
            CursorStyle::Underline => ProviderCursorStyle::Underline,
            CursorStyle::BlockHollow => ProviderCursorStyle::BlockHollow,
        },
        blink: cursor.blink,
        wide_tail: cursor.wide_tail,
    }
}
