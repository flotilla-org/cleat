use cleat::{
    provider::DirtyState,
    vt::{ghostty::GhosttyVtEngine, VtEngine},
};
use terminal_screen::ScreenGrid;

// Cast fixtures use the same render adapter as live screen queries.
pub fn snapshot(vt: &mut GhosttyVtEngine, generation: u64) -> ScreenGrid {
    let mut update = vt.render_update(DirtyState::Full).unwrap();
    update.render_generation = generation;
    cleat::screen_query::grid_from_update(update).unwrap()
}
