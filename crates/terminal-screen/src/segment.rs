//! graph-based segmentation as a switchable region producer.
//!
//! The cell grid is a 4-connected graph. Each edge between neighbouring cells
//! gets a dissimilarity built from explainable terms (background change,
//! whitespace gutter, vertical adjacency, foreground/attribute change, semantic
//! change). Border glyphs are walls: no edge joins a wall to a non-wall cell,
//! so wall cells form their own regions ("frames") and the non-wall cells they
//! enclose form "enclosures". Inside an enclosure a Kruskal-ordered merge
//! (single linkage) is swept over increasing thresholds: block -> group ->
//! zone -> enclosure. The
//! nested partitions form the region tree; its leaves are row slices, so every
//! cell belongs to exactly one leaf.
//!
//! Weights are heuristic and uncalibrated; see the README for known limitations.

use std::collections::{BTreeSet, HashMap};

use crate::{tree::span_node, Cell, CellWidth, CursorStyle, Node, NodeId, Rect, Rgb, ScreenGrid, ScreenTree, SemanticPrompt};

// ---------------------------------------------------------------------------
// Producer interface
// ---------------------------------------------------------------------------

/// Where a region came from. Recognizers prefer declared over inferred.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum Provenance {
    Declared,
    Structural,
    Graph,
    Temporal,
    Learned,
}

impl Provenance {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Declared => "declared",
            Self::Structural => "structural",
            Self::Graph => "graph",
            Self::Temporal => "temporal",
            Self::Learned => "learned",
        }
    }
}

/// Ordered coarse to fine; a collapsed chain keeps every kind it stands for.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum RegionKind {
    Screen,
    /// A connected set of border glyphs (box frame, rule, divider).
    Frame,
    /// Non-wall cells connected without crossing a wall.
    Enclosure,
    /// Cells of one background, joined across whitespace gutters.
    Zone,
    /// Blocks joined across thin (1-row) gaps: proximity grouping.
    Group,
    /// Content joined by short gaps and vertical adjacency.
    Block,
    /// A whitespace component at block level.
    Gap,
    /// Leaf: one contiguous row slice of one block/gap/frame.
    RowSlice,
}

impl RegionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Screen => "screen",
            Self::Frame => "frame",
            Self::Enclosure => "enclosure",
            Self::Zone => "zone",
            Self::Group => "group",
            Self::Block => "block",
            Self::Gap => "gap",
            Self::RowSlice => "row-slice",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Region {
    /// Every kind this node stands for after collapsing identical cell sets.
    pub kinds: BTreeSet<RegionKind>,
    pub bounds: Rect,
    pub cell_count: u32,
    pub provenance: Provenance,
    /// 0..1: how far the cheapest boundary edge exceeds the costliest internal one.
    pub confidence: f32,
    /// Human-readable reason the region stops where it does.
    pub evidence: String,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
}

impl Region {
    pub fn is(&self, kind: RegionKind) -> bool {
        self.kinds.contains(&kind)
    }
    /// Coarsest kind.
    pub fn kind(&self) -> RegionKind {
        *self.kinds.first().expect("regions have at least one kind")
    }
}

/// Output of any producer: a strict hierarchy whose leaves partition the grid.
#[derive(Clone, Debug)]
pub struct RegionTree {
    pub cols: u16,
    pub rows: u16,
    pub generation: u64,
    /// Preorder; index 0 is the screen.
    pub regions: Vec<Region>,
    leaf_of: Vec<u32>,
}

/// Optional history for temporal producers (not used by the graph producer).
pub struct FrameHistory<'a> {
    pub frames: &'a [ScreenGrid],
}

/// The switchable seam. Producers differ only in how they partition cells.
pub trait RegionProducer {
    fn provenance(&self) -> Provenance;
    fn produce(&self, grid: &ScreenGrid, history: Option<&FrameHistory<'_>>) -> RegionTree;
}

impl RegionTree {
    /// Construct another producer's partition. Entries must be preorder with
    /// mutually consistent links, bounded geometry and complete row-slice leaves.
    /// Region IDs are local to this frame; confidence is producer-defined.
    pub fn new(grid: &ScreenGrid, regions: Vec<Region>) -> Result<Self, crate::TreeError> {
        let fail = |message: &str| crate::TreeError(message.to_string());
        let Some(root) = regions.first() else {
            return Err(fail("region tree needs a screen root"));
        };
        if root.parent.is_some() || !root.is(RegionKind::Screen) || root.bounds != grid.bounds() {
            return Err(fail("invalid region root"));
        }
        let mut leaf_of = vec![u32::MAX; grid.cells().len()];
        let mut counts = vec![0u32; regions.len()];
        for (id, region) in regions.iter().enumerate() {
            if region.kinds.is_empty() || !region.confidence.is_finite() || !(0.0..=1.0).contains(&region.confidence) {
                return Err(fail("region needs a kind and finite confidence in 0..1"));
            }
            if id != 0 {
                let Some(parent) = region.parent.filter(|p| *p < id) else {
                    return Err(fail("parent must precede child"));
                };
                if !regions[parent].bounds.contains(region.bounds) || regions[parent].children.iter().filter(|c| **c == id).count() != 1 {
                    return Err(fail("region must nest under its linked parent"));
                }
            }
            let mut unique = BTreeSet::new();
            for &child in &region.children {
                if child <= id || child >= regions.len() || regions[child].parent != Some(id) || !unique.insert(child) {
                    return Err(fail("invalid or duplicate child link"));
                }
            }
            if region.children.is_empty() && id != 0 {
                let b = region.bounds;
                if !region.is(RegionKind::RowSlice) || b.height != 1 || b.width == 0 || !grid.bounds().contains(b) {
                    return Err(fail("leaf must be a nonempty bounded row slice"));
                }
                for col in b.col..b.col + b.width {
                    let slot = &mut leaf_of[usize::from(b.row) * usize::from(grid.cols()) + usize::from(col)];
                    if *slot != u32::MAX {
                        return Err(fail("leaf cells overlap"));
                    }
                    *slot = id as u32;
                }
                counts[id] = u32::from(b.width);
            }
        }
        if leaf_of.contains(&u32::MAX) {
            return Err(fail("leaves do not cover every cell"));
        }
        for id in (0..regions.len()).rev() {
            if !regions[id].children.is_empty() {
                counts[id] = regions[id].children.iter().map(|c| counts[*c]).sum();
            }
            if regions[id].cell_count != counts[id] {
                return Err(fail("region cell count must equal its descendant leaves"));
            }
        }
        // Strict preorder is required by the screen-tree adapter.
        let mut order = Vec::new();
        let mut stack = vec![0];
        while let Some(id) = stack.pop() {
            order.push(id);
            stack.extend(regions[id].children.iter().rev());
        }
        if order != (0..regions.len()).collect::<Vec<_>>() {
            return Err(fail("regions must be in preorder"));
        }
        Ok(Self { cols: grid.cols(), rows: grid.rows(), generation: grid.generation, regions, leaf_of })
    }
    pub fn root(&self) -> usize {
        0
    }
    pub fn leaf_at(&self, col: u16, row: u16) -> usize {
        self.leaf_of[usize::from(row) * usize::from(self.cols) + usize::from(col)] as usize
    }
    pub fn ancestor(&self, mut id: usize, kind: RegionKind) -> Option<usize> {
        loop {
            if self.regions[id].is(kind) {
                return Some(id);
            }
            id = self.regions[id].parent?;
        }
    }
    pub fn region_at(&self, col: u16, row: u16, kind: RegionKind) -> Option<usize> {
        self.ancestor(self.leaf_at(col, row), kind)
    }

    /// One character per cell: `#` frame cells, `.` gap (at block level),
    /// otherwise a letter per distinct region of `kind` in reading order.
    pub fn render_map(&self, kind: RegionKind) -> Vec<String> {
        const LETTERS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        let mut letters: HashMap<usize, char> = HashMap::new();
        let mut out = Vec::new();
        for row in 0..self.rows {
            let mut line = String::new();
            for col in 0..self.cols {
                let leaf = self.leaf_at(col, row);
                let ch = if self.ancestor(leaf, RegionKind::Frame).is_some_and(|f| self.regions[leaf].parent == Some(f)) {
                    '#'
                } else if kind == RegionKind::Block && self.ancestor(leaf, RegionKind::Gap).is_some() {
                    '.'
                } else {
                    match self.ancestor(leaf, kind) {
                        Some(id) => {
                            let n = letters.len();
                            *letters.entry(id).or_insert_with(|| if n < LETTERS.len() { LETTERS[n] as char } else { '?' })
                        }
                        None => ' ',
                    }
                };
                line.push(ch);
            }
            out.push(line);
        }
        out
    }

    fn leaves_under(&self, id: usize, out: &mut Vec<usize>) {
        if self.regions[id].children.is_empty() {
            out.push(id);
        }
        for &c in &self.regions[id].children {
            self.leaves_under(c, out);
        }
    }

    /// Text of exactly the region's cells: row slices joined by a space within
    /// a row and a newline between rows.
    pub fn text(&self, grid: &ScreenGrid, id: usize) -> String {
        let mut leaves = Vec::new();
        self.leaves_under(id, &mut leaves);
        leaves.sort_by_key(|&l| (self.regions[l].bounds.row, self.regions[l].bounds.col));
        let mut text = String::new();
        let mut last: Option<(u16, u16)> = None;
        for l in leaves {
            let b = self.regions[l].bounds;
            match last {
                Some((r, end)) if r == b.row && end == b.col => {}
                Some((r, _)) if r == b.row => text.push(' '),
                Some(_) => text.push('\n'),
                None => {}
            }
            last = Some((b.row, b.col + b.width));
            text.push_str(&grid.text(b));
        }
        text
    }

    /// Selectors walk `screen > region… > row > span`. Kinds are roles;
    /// rows are slices, with their physical row index available as an attribute.
    /// Cursor nodes are observational overlays and do not own cells.
    pub fn to_screen_tree(&self, grid: &ScreenGrid) -> ScreenTree {
        assert_eq!((self.cols, self.rows, self.generation), (grid.cols(), grid.rows(), grid.generation), "region/grid frame must agree");
        let mut ids: Vec<NodeId> = Vec::with_capacity(self.regions.len());
        let mut root = Node::new("screen", grid.bounds(), grid.text(grid.bounds()));
        root.attributes.insert("generation".into(), grid.generation.to_string());
        root.attributes.insert("cols".into(), grid.cols().to_string());
        root.attributes.insert("rows".into(), grid.rows().to_string());
        let mut tree = ScreenTree::from_root(grid.generation, root);
        for (i, region) in self.regions.iter().enumerate() {
            if i == 0 {
                ids.push(tree.root());
                continue;
            }
            let leaf = region.children.is_empty();
            let mut node = Node::new(if leaf { "row" } else { "region" }, region.bounds, self.text(grid, i));
            for k in &region.kinds {
                node.roles.insert(k.as_str().into());
            }
            node.confidence = Some(region.confidence);
            node.attributes.insert("kind".into(), region.kind().as_str().into());
            node.attributes.insert("provenance".into(), region.provenance.as_str().into());
            node.attributes.insert("confidence".into(), format!("{:.2}", region.confidence));
            node.attributes.insert("evidence".into(), region.evidence.clone());
            let parent = ids[region.parent.expect("non-root regions have parents")];
            let id = tree.add_node(parent, node).expect("region bounds nest");
            ids.push(id);
            if leaf {
                let b = region.bounds;
                let metadata = grid.row_metadata()[usize::from(b.row)];
                let annotations = tree.annotations_mut(id).expect("new row is attached");
                annotations.attributes.insert("index".into(), b.row.to_string());
                annotations.attributes.insert("index-from-bottom".into(), (grid.rows() - b.row - 1).to_string());
                annotations.attributes.insert("slice-col".into(), b.col.to_string());
                annotations.attributes.insert(
                    "semantic-prompt".into(),
                    match metadata.semantic_prompt {
                        SemanticPrompt::None => "none",
                        SemanticPrompt::Prompt => "prompt",
                        SemanticPrompt::Continuation => "continuation",
                    }
                    .into(),
                );
                for (name, enabled) in [
                    ("soft-wrap", metadata.soft_wrap),
                    ("wrap-continuation", metadata.wrap_continuation),
                    ("prompt", metadata.semantic_prompt != SemanticPrompt::None),
                ] {
                    if enabled {
                        annotations.attributes.insert(name.into(), "true".into());
                    }
                }
                let cells = &grid.row(b.row).expect("leaf row in grid")[usize::from(b.col)..usize::from(b.col + b.width)];
                let mut start = 0;
                for end in 1..=cells.len() {
                    if end == cells.len() || cells[end].style != cells[start].style || cells[end].semantic != cells[start].semantic {
                        tree.add_node(id, span_node(&cells[start..end], b.col + start as u16, b.row)).expect("span inside leaf");
                        start = end;
                    }
                }
                let c = grid.cursor;
                if c.visible && c.row == b.row && c.col >= b.col && c.col < b.col + b.width {
                    let mut node = Node::new("cursor", Rect { col: c.col, row: c.row, width: 1, height: 1 }, "");
                    node.attributes.insert("visible".into(), "true".into());
                    for (name, enabled) in [("blinking", c.blinking), ("password-input", c.password_input), ("wide-tail", c.wide_tail)] {
                        if enabled {
                            node.attributes.insert(name.into(), "true".into());
                        }
                    }
                    node.attributes.insert(
                        "style".into(),
                        match c.style {
                            CursorStyle::Bar => "bar",
                            CursorStyle::Block => "block",
                            CursorStyle::Underline => "underline",
                            CursorStyle::Hollow => "hollow",
                        }
                        .into(),
                    );
                    tree.add_node(id, node).expect("cursor inside leaf");
                }
            }
        }
        tree
    }
}

// ---------------------------------------------------------------------------
// Graph producer
// ---------------------------------------------------------------------------

/// Edge-reason bits, kept so a boundary can say why it is a boundary.
pub mod reason {
    pub const BG: u8 = 1;
    pub const GUTTER: u8 = 2;
    pub const VERTICAL: u8 = 4;
    pub const FG: u8 = 8;
    pub const ATTR: u8 = 16;
    pub const SEMANTIC: u8 = 32;
    pub const PROMPT: u8 = 64;
    pub const WALL: u8 = 128;
}

fn reason_text(bits: u8) -> String {
    let names = [
        (reason::BG, "bg"),
        (reason::GUTTER, "gutter"),
        (reason::VERTICAL, "vertical"),
        (reason::FG, "fg"),
        (reason::ATTR, "attr"),
        (reason::SEMANTIC, "semantic"),
        (reason::PROMPT, "prompt-row"),
        (reason::WALL, "wall"),
    ];
    let v: Vec<&str> = names.iter().filter(|(b, _)| bits & b != 0).map(|(_, n)| *n).collect();
    if v.is_empty() {
        "none".into()
    } else {
        v.join("+")
    }
}

#[derive(Clone, Debug)]
pub struct GraphParams {
    /// A horizontal blank run at least this long is a gutter, not a word gap.
    pub gutter_min: u16,
    pub w_bg: f32,
    pub w_gutter: f32,
    pub w_vertical: f32,
    pub w_fg: f32,
    pub w_attr: f32,
    pub w_semantic: f32,
    pub w_prompt: f32,
    /// Block level: fixed single-linkage threshold.
    pub block_tau: f32,
    /// A blank run of at most this many rows between content above and below
    /// is a thin gap; content joins across it at the group level.
    pub thin_rows: u16,
    pub w_thin: f32,
    pub group_tau: f32,
    /// Zone level threshold (joins content to gutters, stops at bg changes).
    pub zone_tau: f32,
}

impl Default for GraphParams {
    fn default() -> Self {
        Self {
            gutter_min: 3,
            w_bg: 0.7,
            w_gutter: 0.5,
            w_vertical: 0.3,
            w_fg: 0.1,
            w_attr: 0.05,
            w_semantic: 0.4,
            w_prompt: 0.2,
            block_tau: 0.35,
            thin_rows: 1,
            w_thin: 0.4,
            group_tau: 0.45,
            zone_tau: 0.65,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct GraphSegmenter {
    params: GraphParams,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Wall,
    Gutter,
    /// Gutter cell in a short vertical blank run between content rows.
    Thin,
    Content,
}

#[derive(Clone, Copy)]
struct Feat {
    class: Class,
    blank: bool,
    fg: Rgb,
    bg: Rgb,
    attrs: u8,
    semantic: u8,
    /// 1-cell bg island (fake cursor): its bg change is not a boundary.
    mark: bool,
}

fn single_char(cell: &Cell) -> Option<char> {
    let mut it = cell.grapheme.chars();
    match (it.next(), it.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    }
}

fn is_box_glyph(c: char) -> bool {
    ('\u{2500}'..='\u{257f}').contains(&c)
}

fn is_blank(cell: &Cell) -> bool {
    match cell.width {
        CellWidth::SpacerHead => true,
        CellWidth::SpacerTail => false,
        _ => cell.grapheme.is_empty() || cell.grapheme == " " || cell.grapheme == "\u{a0}",
    }
}

fn classify(grid: &ScreenGrid, gutter_min: u16, thin_rows: u16) -> Vec<Feat> {
    let (cols, rows) = (grid.cols(), grid.rows());
    let ch = |c: i32, r: i32| -> Option<char> {
        if c < 0 || r < 0 || c >= i32::from(cols) || r >= i32::from(rows) {
            None
        } else {
            single_char(grid.cell(c as u16, r as u16).expect("bounds checked"))
        }
    };
    // Horizontal run lengths of ASCII rule characters (-, =, +).
    let mut rule_run = vec![0u16; usize::from(cols) * usize::from(rows)];
    for r in 0..rows {
        let mut c = 0;
        while c < cols {
            if matches!(ch(i32::from(c), i32::from(r)), Some('-' | '=' | '+')) {
                let start = c;
                while c < cols && matches!(ch(i32::from(c), i32::from(r)), Some('-' | '=' | '+')) {
                    c += 1;
                }
                for x in start..c {
                    rule_run[usize::from(r) * usize::from(cols) + usize::from(x)] = c - start;
                }
            } else {
                c += 1;
            }
        }
    }
    let vert = |c: Option<char>| matches!(c, Some('|' | '+')) || c.is_some_and(is_box_glyph);
    let mut wall = vec![false; usize::from(cols) * usize::from(rows)];
    for r in 0..rows {
        for c in 0..cols {
            let i = usize::from(r) * usize::from(cols) + usize::from(c);
            let (ci, ri) = (i32::from(c), i32::from(r));
            wall[i] = match ch(ci, ri) {
                Some(g) if is_box_glyph(g) => true,
                Some('-' | '=') => rule_run[i] >= 3,
                Some('|') => vert(ch(ci, ri - 1)) || vert(ch(ci, ri + 1)),
                Some('+') => {
                    rule_run[i] >= 3 && (matches!(ch(ci - 1, ri), Some('-' | '=')) || matches!(ch(ci + 1, ri), Some('-' | '=')))
                        || matches!(ch(ci, ri - 1), Some('|'))
                        || matches!(ch(ci, ri + 1), Some('|'))
                }
                _ => false,
            };
        }
    }
    // Titles embedded in a horizontal border ("╭─ Files ──╮") belong to the frame.
    let horiz = |c: Option<char>| matches!(c, Some('─' | '━' | '═' | '╌' | '╍' | '┄' | '┅' | '┈' | '┉' | '-' | '='));
    for r in 0..rows {
        let mut c = 0u16;
        while c < cols {
            let i = usize::from(r) * usize::from(cols) + usize::from(c);
            if wall[i] {
                c += 1;
                continue;
            }
            let start = c;
            while c < cols && !wall[usize::from(r) * usize::from(cols) + usize::from(c)] {
                c += 1;
            }
            if start > 0
                && c < cols
                && horiz(ch(i32::from(start) - 1, i32::from(r)))
                && horiz(ch(i32::from(c), i32::from(r)))
                && c - start <= 48
            {
                for x in start..c {
                    wall[usize::from(r) * usize::from(cols) + usize::from(x)] = true;
                }
            }
        }
    }
    let mut feats = Vec::with_capacity(wall.len());
    for (i, cell) in grid.cells().iter().enumerate() {
        let s = &cell.style;
        let (fg, bg) = if s.inverse { (s.bg, s.fg) } else { (s.fg, s.bg) };
        let attrs = u8::from(s.bold)
            | u8::from(s.faint) << 1
            | u8::from(s.italic) << 2
            | u8::from(s.underline_style != 0) << 3
            | u8::from(s.strikethrough) << 4;
        feats.push(Feat {
            class: if wall[i] { Class::Wall } else { Class::Content },
            blank: !wall[i] && is_blank(cell),
            fg,
            bg,
            attrs,
            semantic: cell.semantic as u8,
            mark: false,
        });
    }
    // The hardware cursor cell, and 1-cell background islands next to content
    // (an app-painted cursor), count as content.
    let cur = grid.cursor;
    if cur.visible {
        let i = usize::from(cur.row) * usize::from(cols) + usize::from(cur.col);
        if feats[i].class != Class::Wall {
            feats[i].blank = false;
        }
    }
    for r in 0..usize::from(rows) {
        for c in 0..usize::from(cols) {
            let i = r * usize::from(cols) + c;
            let left = (c > 0).then(|| feats[i - 1]);
            let right = (c + 1 < usize::from(cols)).then(|| feats[i + 1]);
            let island = left.is_none_or(|f| f.bg != feats[i].bg) && right.is_none_or(|f| f.bg != feats[i].bg);
            if island
                && feats[i].class != Class::Wall
                && (c >= 1 && !feats[i - 1].blank && feats[i - 1].class != Class::Wall
                    || c >= 2 && !feats[i - 2].blank && feats[i - 2].class != Class::Wall)
            {
                feats[i].blank = false;
                feats[i].mark = true;
            }
        }
    }
    if cur.visible {
        let i = usize::from(cur.row) * usize::from(cols) + usize::from(cur.col);
        if cur.col > 0 && feats[i].class != Class::Wall && is_blank(&grid.cells()[i]) {
            feats[i].semantic = feats[i - 1].semantic;
        }
    }
    // Long blank runs are gutters.
    for r in 0..usize::from(rows) {
        let base = r * usize::from(cols);
        let mut c = 0;
        while c < usize::from(cols) {
            if feats[base + c].blank {
                let start = c;
                while c < usize::from(cols) && feats[base + c].blank {
                    c += 1;
                }
                if c - start >= usize::from(gutter_min) {
                    for f in &mut feats[base + start..base + c] {
                        f.class = Class::Gutter;
                    }
                }
            } else {
                c += 1;
            }
        }
    }
    // Thin gaps: short vertical gutter runs with content directly above and below.
    let w = usize::from(cols);
    for c in 0..w {
        let mut r = 0usize;
        while r < usize::from(rows) {
            if feats[r * w + c].class == Class::Gutter {
                let start = r;
                while r < usize::from(rows) && feats[r * w + c].class == Class::Gutter {
                    r += 1;
                }
                let bounded = start > 0
                    && r < usize::from(rows)
                    && feats[(start - 1) * w + c].class == Class::Content
                    && feats[r * w + c].class == Class::Content;
                if bounded && r - start <= usize::from(thin_rows) {
                    for y in start..r {
                        feats[y * w + c].class = Class::Thin;
                    }
                }
            } else {
                r += 1;
            }
        }
    }
    feats
}

struct Dsu {
    parent: Vec<u32>,
    size: Vec<u32>,
    int: Vec<f32>,
}

impl Dsu {
    fn new(n: usize) -> Self {
        Self { parent: (0..n as u32).collect(), size: vec![1; n], int: vec![0.0; n] }
    }
    fn find(&mut self, mut x: u32) -> u32 {
        while self.parent[x as usize] != x {
            let p = self.parent[x as usize];
            self.parent[x as usize] = self.parent[p as usize];
            x = p;
        }
        x
    }
    fn union(&mut self, a: u32, b: u32, w: f32) {
        let (a, b) = if self.size[a as usize] >= self.size[b as usize] { (a, b) } else { (b, a) };
        self.parent[b as usize] = a;
        self.size[a as usize] += self.size[b as usize];
        self.int[a as usize] = self.int[a as usize].max(self.int[b as usize]).max(w);
    }
    fn labels(&mut self) -> Vec<u32> {
        (0..self.parent.len() as u32).map(|i| self.find(i)).collect()
    }
}

#[derive(Clone, Copy)]
struct Edge {
    a: u32,
    b: u32,
    w: f32,
    why: u8,
}

struct Proto {
    kinds: BTreeSet<RegionKind>,
    bounds: Rect,
    cells: u32,
    confidence: f32,
    evidence: String,
    parent: Option<usize>,
}

#[derive(Clone, Copy)]
struct CompInfo {
    min_c: u16,
    min_r: u16,
    max_c: u16,
    max_r: u16,
    cells: u32,
    all_gutter: bool,
}

impl CompInfo {
    fn rect(&self) -> Rect {
        Rect { col: self.min_c, row: self.min_r, width: self.max_c - self.min_c + 1, height: self.max_r - self.min_r + 1 }
    }
}

/// Indexed by label (labels are root cell indices).
fn comp_infos(labels: &[u32], feats: &[Feat], cols: u16) -> Vec<CompInfo> {
    let mut m = vec![CompInfo { min_c: u16::MAX, min_r: u16::MAX, max_c: 0, max_r: 0, cells: 0, all_gutter: true }; labels.len()];
    for (i, &l) in labels.iter().enumerate() {
        let (c, r) = ((i % usize::from(cols)) as u16, (i / usize::from(cols)) as u16);
        let g = matches!(feats[i].class, Class::Gutter | Class::Thin);
        let e = &mut m[l as usize];
        e.min_c = e.min_c.min(c);
        e.max_c = e.max_c.max(c);
        e.min_r = e.min_r.min(r);
        e.max_r = e.max_r.max(r);
        e.cells += 1;
        e.all_gutter &= g;
    }
    m
}

/// Per component: (cheapest boundary weight, its reason bits).
fn boundaries(labels: &[u32], edges: &[Edge]) -> Vec<(f32, u8)> {
    let mut m = vec![(f32::INFINITY, reason::WALL); labels.len()];
    for e in edges {
        let (la, lb) = (labels[e.a as usize], labels[e.b as usize]);
        if la != lb {
            for l in [la, lb] {
                let entry = &mut m[l as usize];
                if e.w < entry.0 {
                    *entry = (e.w, e.why);
                }
            }
        }
    }
    m
}

fn confidence(level: &str, int: f32, boundary: Option<&(f32, u8)>) -> (f32, String) {
    let (bw, why) = boundary.copied().unwrap_or((f32::INFINITY, reason::WALL));
    let conf = if bw.is_infinite() { 1.0 } else { ((bw - int) / 0.5).clamp(0.0, 1.0) };
    let bw_s = if bw.is_infinite() { "inf".to_string() } else { format!("{bw:.2}") };
    (conf, format!("{level}: internal {int:.2}, boundary {bw_s} ({})", reason_text(why)))
}

impl GraphSegmenter {
    /// Override heuristic weights while preserving nested merge thresholds.
    pub fn new(params: GraphParams) -> Result<Self, crate::TreeError> {
        let values = [
            params.w_bg,
            params.w_gutter,
            params.w_vertical,
            params.w_fg,
            params.w_attr,
            params.w_semantic,
            params.w_prompt,
            params.w_thin,
            params.block_tau,
            params.group_tau,
            params.zone_tau,
        ];
        if params.gutter_min == 0
            || values.iter().any(|v| !v.is_finite() || *v < 0.0)
            || params.block_tau > params.group_tau
            || params.group_tau > params.zone_tau
        {
            return Err(crate::TreeError("graph weights must be finite/nonnegative, gutter_min positive, thresholds ordered".into()));
        }
        Ok(Self { params })
    }

    fn edges(&self, grid: &ScreenGrid, feats: &[Feat]) -> (Vec<Edge>, Vec<(u32, u32)>) {
        let p = &self.params;
        let cols = usize::from(grid.cols());
        let rows = usize::from(grid.rows());
        let meta = grid.row_metadata();
        let mut edges = Vec::with_capacity(cols * rows * 2);
        let mut wall_adj = Vec::new();
        let mut weigh = |a: usize, b: usize, vertical: bool| {
            let (fa, fb) = (feats[a], feats[b]);
            let (wa, wb) = (fa.class == Class::Wall, fb.class == Class::Wall);
            if wa || wb {
                if wa && wb {
                    edges.push(Edge { a: a as u32, b: b as u32, w: 0.0, why: reason::WALL });
                } else {
                    wall_adj.push((a as u32, b as u32));
                }
                return;
            }
            let (mut w, mut why) = (0.0f32, 0u8);
            if fa.bg != fb.bg && !(!vertical && (fa.mark || fb.mark)) {
                w += p.w_bg;
                why |= reason::BG;
            }
            match (fa.class, fb.class) {
                (Class::Gutter, Class::Gutter) | (Class::Thin, Class::Thin) => {}
                (Class::Thin, Class::Content) | (Class::Content, Class::Thin) => {
                    w += p.w_thin;
                    why |= reason::GUTTER;
                }
                (Class::Gutter | Class::Thin, _) | (_, Class::Gutter | Class::Thin) => {
                    w += p.w_gutter;
                    why |= reason::GUTTER;
                }
                _ => {
                    if vertical {
                        w += p.w_vertical;
                        why |= reason::VERTICAL;
                    }
                    if !fa.blank && !fb.blank {
                        if fa.fg != fb.fg {
                            w += p.w_fg;
                            why |= reason::FG;
                        }
                        if fa.attrs != fb.attrs {
                            w += p.w_attr;
                            why |= reason::ATTR;
                        }
                    }
                }
            }
            if fa.semantic != fb.semantic {
                w += p.w_semantic;
                why |= reason::SEMANTIC;
            }
            if vertical && meta[a / cols].semantic_prompt != meta[b / cols].semantic_prompt {
                w += p.w_prompt;
                why |= reason::PROMPT;
            }
            edges.push(Edge { a: a as u32, b: b as u32, w, why });
        };
        for r in 0..rows {
            for c in 0..cols {
                let i = r * cols + c;
                if c + 1 < cols {
                    weigh(i, i + 1, false);
                }
                if r + 1 < rows {
                    weigh(i, i + cols, true);
                }
            }
        }
        edges.sort_unstable_by(|x, y| x.w.total_cmp(&y.w));
        (edges, wall_adj)
    }

    pub fn segment(&self, grid: &ScreenGrid) -> RegionTree {
        let p = &self.params;
        let cols = grid.cols();
        let n = usize::from(cols) * usize::from(grid.rows());
        let feats = classify(grid, p.gutter_min, p.thin_rows);
        let (edges, wall_adj) = self.edges(grid, &feats);
        let mut dsu = Dsu::new(n);

        // Level 1: block (single linkage <= tau).
        for e in &edges {
            let (ra, rb) = (dsu.find(e.a), dsu.find(e.b));
            if ra == rb {
                continue;
            }
            if e.w <= p.block_tau {
                dsu.union(ra, rb, e.w);
            }
        }
        let block = dsu.labels();
        let block_int = dsu.int.clone();
        // Level 2: group (across thin gaps); level 3: zone (across gutters, not bg).
        let sweep = |tau: f32, dsu: &mut Dsu| {
            for e in &edges {
                let (ra, rb) = (dsu.find(e.a), dsu.find(e.b));
                if ra != rb && e.w <= tau {
                    dsu.union(ra, rb, e.w);
                }
            }
            (dsu.labels(), dsu.int.clone())
        };
        let (group, group_int) = sweep(p.group_tau, &mut dsu);
        let (zone, zone_int) = sweep(p.zone_tau, &mut dsu);
        // Level 4: enclosure (everything not separated by a wall).
        for e in &edges {
            let (ra, rb) = (dsu.find(e.a), dsu.find(e.b));
            if ra != rb {
                dsu.union(ra, rb, e.w);
            }
        }
        let enc = dsu.labels();
        let enc_int = dsu.int.clone();

        let block_info = comp_infos(&block, &feats, cols);
        let zone_info = comp_infos(&zone, &feats, cols);
        let group_info = comp_infos(&group, &feats, cols);
        let group_b = boundaries(&group, &edges);
        let enc_info = comp_infos(&enc, &feats, cols);
        let block_b = boundaries(&block, &edges);
        let zone_b = boundaries(&zone, &edges);

        let mut protos: Vec<Proto> = vec![Proto {
            kinds: [RegionKind::Screen].into(),
            bounds: grid.bounds(),
            cells: n as u32,
            confidence: 1.0,
            evidence: "screen".into(),
            parent: None,
        }];

        // Enclosures and frames, in reading order of first cell.
        let mut enc_proto: HashMap<u32, usize> = HashMap::new();
        let mut order: Vec<u32> = Vec::new();
        for &l in &enc {
            if let std::collections::hash_map::Entry::Vacant(slot) = enc_proto.entry(l) {
                let info = enc_info[l as usize];
                let is_wall = feats[l as usize].class == Class::Wall;
                let (confidence, evidence) = confidence(if is_wall { "frame" } else { "enclosure" }, enc_int[l as usize], None);
                slot.insert(protos.len());
                order.push(l);
                protos.push(Proto {
                    kinds: [if is_wall { RegionKind::Frame } else { RegionKind::Enclosure }].into(),
                    bounds: info.rect(),
                    cells: info.cells,
                    confidence,
                    evidence,
                    parent: Some(0),
                });
            }
        }
        // Nesting by adjacency + bbox containment.
        // Frame <-> enclosure adjacency lists, indexed by proto.
        let mut adj: Vec<Vec<usize>> = vec![Vec::new(); protos.len()];
        for &(a, b) in &wall_adj {
            let (pa, pb) = (enc_proto[&enc[a as usize]], enc_proto[&enc[b as usize]]);
            adj[pa].push(pb);
            adj[pb].push(pa);
        }
        for a in &mut adj {
            a.sort_unstable();
            a.dedup();
        }
        let area = |r: Rect| u32::from(r.width) * u32::from(r.height);
        // Enclosure inside enclosure: cast rays from the bbox centre lines past
        // walls; the first non-wall cell hit in >= 3 of 4 directions, from an
        // enclosure whose bbox strictly contains ours, surrounds us.
        let (gc, gr) = (i32::from(cols), i32::from(grid.rows()));
        let ray = |r: Rect, dc: i32, dr: i32| -> Option<u32> {
            let (mut c, mut rr) = if dc != 0 {
                (if dc < 0 { i32::from(r.col) - 1 } else { i32::from(r.col + r.width) }, i32::from(r.row) + i32::from(r.height) / 2)
            } else {
                (i32::from(r.col) + i32::from(r.width) / 2, if dr < 0 { i32::from(r.row) - 1 } else { i32::from(r.row + r.height) })
            };
            while c >= 0 && rr >= 0 && c < gc && rr < gr {
                let i = (rr * gc + c) as usize;
                if feats[i].class != Class::Wall {
                    return Some(enc[i]);
                }
                c += dc;
                rr += dr;
            }
            None
        };
        let mut parents: Vec<(usize, usize)> = Vec::new();
        for &l in &order {
            let me = enc_proto[&l];
            let mb = protos[me].bounds;
            let is_frame = protos[me].kinds.contains(&RegionKind::Frame);
            // A frame hangs under an adjacent enclosure containing its bbox; an
            // enclosure under an adjacent frame strictly containing its bbox.
            let mut best = adj[me]
                .iter()
                .copied()
                .filter(|&o| {
                    let ob = protos[o].bounds;
                    ob.contains(mb) && (is_frame || ob != mb)
                })
                .min_by_key(|&o| (area(protos[o].bounds), o));
            if !is_frame {
                let mut hits: HashMap<u32, u8> = HashMap::new();
                for (dc, dr) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
                    if let Some(h) = ray(mb, dc, dr) {
                        if h != l {
                            *hits.entry(h).or_default() += 1;
                        }
                    }
                }
                for (h, count) in hits {
                    let hp = enc_proto[&h];
                    let hb = protos[hp].bounds;
                    if count >= 3 && hb.contains(mb) && hb != mb && best.is_none_or(|b| (area(hb), hp) < (area(protos[b].bounds), b)) {
                        best = Some(hp);
                    }
                }
            }
            if let Some(b) = best {
                parents.push((me, b));
            }
        }
        for (me, b) in parents {
            protos[me].parent = Some(b);
        }
        // Break any cycle defensively.
        for i in 1..protos.len() {
            let (mut cur, mut steps) = (i, 0);
            while let Some(p) = protos[cur].parent {
                steps += 1;
                if steps > protos.len() {
                    protos[i].parent = Some(0);
                    break;
                }
                cur = p;
            }
        }

        // Zones and blocks under enclosures, collapsing identical cell sets.
        let mut zone_proto: HashMap<u32, usize> = HashMap::new();
        let mut block_proto: HashMap<u32, usize> = HashMap::new();
        let mut group_proto: HashMap<u32, usize> = HashMap::new();
        for i in 0..n {
            let (bl, zl, el) = (block[i], zone[i], enc[i]);
            let ep = enc_proto[&el];
            if feats[i].class == Class::Wall {
                block_proto.entry(bl).or_insert(ep);
                continue;
            }
            let zp = *zone_proto.entry(zl).or_insert_with(|| {
                let info = zone_info[zl as usize];
                let (c, ev) = confidence("zone", zone_int[zl as usize], Some(&zone_b[zl as usize]));
                if info.cells == protos[ep].cells {
                    protos[ep].kinds.insert(RegionKind::Zone);
                    ep
                } else {
                    protos.push(Proto {
                        kinds: [RegionKind::Zone].into(),
                        bounds: info.rect(),
                        cells: info.cells,
                        confidence: c,
                        evidence: ev,
                        parent: Some(ep),
                    });
                    protos.len() - 1
                }
            });
            let gl = group[i];
            let gp = *group_proto.entry(gl).or_insert_with(|| {
                let info = group_info[gl as usize];
                let (c, ev) = confidence("group", group_int[gl as usize], Some(&group_b[gl as usize]));
                if info.cells == protos[zp].cells {
                    protos[zp].kinds.insert(RegionKind::Group);
                    zp
                } else {
                    protos.push(Proto {
                        kinds: [RegionKind::Group].into(),
                        bounds: info.rect(),
                        cells: info.cells,
                        confidence: c,
                        evidence: ev,
                        parent: Some(zp),
                    });
                    protos.len() - 1
                }
            });
            let zp = gp;
            block_proto.entry(bl).or_insert_with(|| {
                let info = block_info[bl as usize];
                let kind = if info.all_gutter { RegionKind::Gap } else { RegionKind::Block };
                let (c, ev) = confidence("block", block_int[bl as usize], Some(&block_b[bl as usize]));
                if info.cells == protos[zp].cells {
                    protos[zp].kinds.insert(kind);
                    zp
                } else {
                    protos.push(Proto {
                        kinds: [kind].into(),
                        bounds: info.rect(),
                        cells: info.cells,
                        confidence: c,
                        evidence: ev,
                        parent: Some(zp),
                    });
                    protos.len() - 1
                }
            });
        }
        // Leaves: maximal row runs of one block label.
        let mut leaf_proto = vec![0u32; n];
        for r in 0..grid.rows() {
            let base = usize::from(r) * usize::from(cols);
            let mut c = 0usize;
            while c < usize::from(cols) {
                let l = block[base + c];
                let start = c;
                while c < usize::from(cols) && block[base + c] == l {
                    c += 1;
                }
                let id = protos.len();
                protos.push(Proto {
                    kinds: [RegionKind::RowSlice].into(),
                    bounds: Rect { col: start as u16, row: r, width: (c - start) as u16, height: 1 },
                    cells: (c - start) as u32,
                    confidence: 1.0,
                    evidence: "row slice".into(),
                    parent: Some(block_proto[&l]),
                });
                for x in start..c {
                    leaf_proto[base + x] = id as u32;
                }
            }
        }

        // Preorder with spatially sorted children.
        let mut kids: Vec<Vec<usize>> = vec![Vec::new(); protos.len()];
        for (i, pr) in protos.iter().enumerate() {
            if let Some(p) = pr.parent {
                kids[p].push(i);
            }
        }
        for k in &mut kids {
            k.sort_by_key(|&i| (protos[i].bounds.row, protos[i].bounds.col, std::cmp::Reverse(protos[i].cells)));
        }
        let mut remap = vec![usize::MAX; protos.len()];
        let mut regions: Vec<Region> = Vec::with_capacity(protos.len());
        let mut stack = vec![(0usize, None::<usize>)];
        while let Some((pid, parent)) = stack.pop() {
            let id = regions.len();
            remap[pid] = id;
            let pr = &protos[pid];
            regions.push(Region {
                kinds: pr.kinds.clone(),
                bounds: pr.bounds,
                cell_count: pr.cells,
                provenance: Provenance::Graph,
                confidence: pr.confidence,
                evidence: pr.evidence.clone(),
                parent,
                children: Vec::new(),
            });
            if let Some(p) = parent {
                regions[p].children.push(id);
            }
            for &k in kids[pid].iter().rev() {
                stack.push((k, Some(id)));
            }
        }
        for id in (0..regions.len()).rev() {
            if !regions[id].children.is_empty() {
                regions[id].cell_count = regions[id].children.iter().map(|&c| regions[c].cell_count).sum();
            }
        }
        let leaf_of = leaf_proto.into_iter().map(|p| remap[p as usize] as u32).collect();
        RegionTree { cols, rows: grid.rows(), generation: grid.generation, regions, leaf_of }
    }
}

impl RegionProducer for GraphSegmenter {
    fn provenance(&self) -> Provenance {
        Provenance::Graph
    }
    fn produce(&self, grid: &ScreenGrid, _history: Option<&FrameHistory<'_>>) -> RegionTree {
        self.segment(grid)
    }
}
