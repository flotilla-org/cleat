//! Geometry of one attachment's local window into a shared terminal grid.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AttachmentView {
    pub x: u16,
    pub y: u16,
    grid: (u16, u16),
    window: (u16, u16),
}
impl AttachmentView {
    pub fn new(grid: (u16, u16), window: (u16, u16)) -> Self {
        Self { x: 0, y: 0, grid, window }
    }
    pub fn resize(&mut self, grid: (u16, u16), window: (u16, u16)) {
        self.grid = grid;
        self.window = window;
        self.x = self.x.min(grid.0.saturating_sub(window.0));
        self.y = self.y.min(grid.1.saturating_sub(window.1));
    }
    pub fn pan(&mut self, x: i16, y: i16) {
        self.x = self.x.saturating_add_signed(x);
        self.y = self.y.saturating_add_signed(y);
        self.resize(self.grid, self.window);
    }
    pub fn reveal(&mut self, col: u16, row: u16) {
        self.x = self.x.min(col).max(col.saturating_sub(self.window.0.saturating_sub(1)));
        self.y = self.y.min(row).max(row.saturating_sub(self.window.1.saturating_sub(1)));
        self.resize(self.grid, self.window);
    }
    pub fn contains(&self, col: u16, row: u16) -> bool {
        col >= self.x
            && row >= self.y
            && col < self.grid.0
            && row < self.grid.1
            && col - self.x < self.window.0
            && row - self.y < self.window.1
    }
    pub fn to_grid(self, col: u16, row: u16) -> Option<(u16, u16)> {
        let x = self.x.checked_add(col)?;
        let y = self.y.checked_add(row)?;
        self.contains(x, y).then_some((x, y))
    }
    pub fn description(&self) -> String {
        if self.grid.0 <= self.window.0 && self.grid.1 <= self.window.1 {
            return String::new();
        }
        let mut edges = String::new();
        if self.x > 0 {
            edges.push('<');
        }
        if self.x.saturating_add(self.window.0) < self.grid.0 {
            edges.push('>');
        }
        if self.y > 0 {
            edges.push('^');
        }
        if self.y.saturating_add(self.window.1) < self.grid.1 {
            edges.push('v');
        }
        format!(
            "{edges} @{},{} | view {}x{} of {}x{} | ",
            self.x,
            self.y,
            self.window.0.min(self.grid.0),
            self.window.1.min(self.grid.1),
            self.grid.0,
            self.grid.1
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pan_reaches_edges_and_clamps_after_resize() {
        let mut view = AttachmentView::new((120, 40), (80, 24));
        view.pan(200, 200);
        assert_eq!((view.x, view.y), (40, 16));
        assert_eq!(view.to_grid(79, 23), Some((119, 39)));
        assert_eq!(view.to_grid(80, 23), None);
        assert_eq!(view.to_grid(79, 24), None);
        view.resize((90, 30), (80, 24));
        assert_eq!((view.x, view.y), (10, 6));
        view.resize((90, 30), (100, 40));
        assert_eq!((view.x, view.y), (0, 0));
        assert_eq!(view.to_grid(90, 0), None);
        assert_eq!(view.to_grid(0, 30), None);
    }
    #[test]
    fn reveal_moves_only_as_far_as_needed() {
        let mut view = AttachmentView::new((120, 40), (80, 24));
        view.reveal(100, 30);
        assert_eq!((view.x, view.y), (21, 7));
        assert!(view.contains(100, 30));
        view.reveal(50, 20);
        assert_eq!((view.x, view.y), (21, 7));
        view.reveal(2, 3);
        assert_eq!((view.x, view.y), (2, 3));
    }
}
