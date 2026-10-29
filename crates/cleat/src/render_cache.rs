//! The latest render of one live stream, kept as whole rows so a consumer
//! that is behind can catch up with one coalesced update. The daemon keeps one
//! per session for its channels; a views client keeps one per attachment for
//! its live views.
use crate::{image_delivery::RenderBundle, provider::DirtyState};

#[derive(Default)]
pub(crate) struct PacketRenderCache {
    latest: Option<RenderBundle>,
    /// Daemon: the last view given a history capture, for round-robin fairness.
    pub history_cursor: u128,
    rows: Vec<crate::provider::TerminalRenderRow>,
    row_generations: Vec<u64>,
    reset_generation: u64,
}

impl PacketRenderCache {
    pub fn store(&mut self, mut bundle: RenderBundle) {
        use crate::provider::TerminalRenderUpdateOpKind as Kind;
        let update = &mut bundle.packet.update;
        let generation = update.render_generation;
        if self.latest.as_ref().is_none_or(|previous| {
            let previous = &previous.packet.update;
            previous.cols != update.cols
                || previous.rows != update.rows
                || previous.terminal_modes.active_alternate_screen != update.terminal_modes.active_alternate_screen
        }) {
            self.rows = (0..update.rows)
                .map(|row| crate::provider::TerminalRenderRow {
                    row,
                    col_count: update.cols,
                    cells: vec![Default::default(); update.cols as usize],
                    ..Default::default()
                })
                .collect();
            self.row_generations = vec![generation; update.rows as usize];
            self.reset_generation = generation;
        }
        for op in std::mem::take(&mut update.ops) {
            if op.kind == Kind::FullVisibleReplace {
                self.reset_generation = generation;
            }
            if op.kind == Kind::ScrollCopy {
                // Snapshot before copying so overlapping scrolls are lossless.
                // Decoded ops are untrusted: rows past the grid are skipped.
                let copied: Vec<_> =
                    (0..op.row_count).filter_map(|offset| self.rows.get(usize::from(op.src_row) + usize::from(offset)).cloned()).collect();
                for (offset, mut row) in copied.into_iter().enumerate() {
                    let index = usize::from(op.dst_row) + offset;
                    if let Some(target) = self.rows.get_mut(index) {
                        row.row = index as u16;
                        *target = row;
                        self.row_generations[index] = generation;
                    }
                }
            } else {
                for row in op.rows {
                    let index = usize::from(row.row);
                    if let Some(target) = self.rows.get_mut(index) {
                        *target = row;
                        self.row_generations[index] = generation;
                    }
                }
            }
        }
        self.latest = Some(bundle);
    }

    pub fn since(&self, generation: u64) -> Option<RenderBundle> {
        use crate::provider::{TerminalRenderUpdateOp, TerminalRenderUpdateOpKind as Kind};
        let mut bundle = self.latest.clone()?;
        let update = &mut bundle.packet.update;
        if generation < self.reset_generation {
            update.dirty = DirtyState::Full;
            update.ops.push(TerminalRenderUpdateOp {
                kind: Kind::FullVisibleReplace,
                row_count: update.rows,
                col_count: update.cols,
                rows: self.rows.clone(),
                ..Default::default()
            });
        } else {
            update.ops = self
                .rows
                .iter()
                .zip(&self.row_generations)
                .filter(|(_, changed)| **changed > generation)
                .map(|(row, _)| TerminalRenderUpdateOp {
                    kind: Kind::RowReplace,
                    first_row: row.row,
                    row_count: 1,
                    col_count: update.cols,
                    rows: vec![row.clone()],
                    ..Default::default()
                })
                .collect();
            update.dirty = if update.ops.is_empty() { DirtyState::Clean } else { DirtyState::Partial };
        }
        Some(bundle)
    }

    pub fn latest_generation(&self) -> Option<u64> {
        self.latest.as_ref().map(|update| update.packet.update.render_generation)
    }

    pub fn latest(&self) -> Option<&RenderBundle> {
        self.latest.as_ref()
    }

    /// Whether catching up from `generation` replaces the whole visible grid.
    pub fn full_since(&self, generation: u64) -> bool {
        generation < self.reset_generation
    }
}
