use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use crate::{
    detect_bands, detect_boxes, BandKind, Cell, CursorStyle, Rect, ScreenGrid, Selector, SelectorError, SemanticContent, SemanticPrompt,
};

/// Identity within one tree only; never an identity across generations.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub struct NodeId(pub usize);

/// Data-driven element, roles and attributes. True boolean flags are present
/// with value `true`; false flags are absent. `text` is an implicit attribute.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub element: String,
    pub roles: BTreeSet<String>,
    pub attributes: BTreeMap<String, String>,
    pub bounds: Rect,
    pub text: String,
    pub confidence: Option<f32>,
    parent: Option<NodeId>,
    children: Vec<NodeId>,
}

impl Node {
    pub fn new(element: impl Into<String>, bounds: Rect, text: impl Into<String>) -> Self {
        Self {
            element: element.into(),
            roles: BTreeSet::new(),
            attributes: BTreeMap::new(),
            bounds,
            text: text.into(),
            confidence: None,
            parent: None,
            children: Vec::new(),
        }
    }
    pub fn parent(&self) -> Option<NodeId> {
        self.parent
    }
    pub fn children(&self) -> &[NodeId] {
        &self.children
    }
    pub fn attribute(&self, name: &str) -> Option<&str> {
        if name == "text" {
            Some(&self.text)
        } else {
            self.attributes.get(name).map(String::as_str)
        }
    }
    fn flag(&mut self, name: &str, enabled: bool) {
        if enabled {
            self.attributes.insert(name.into(), "true".into());
        }
    }
    fn attr(&mut self, name: &str, value: impl ToString) {
        self.attributes.insert(name.into(), value.to_string());
    }
}

/// Recognizer-editable metadata; attached geometry and links stay immutable.
pub struct NodeAnnotations<'a> {
    pub roles: &'a mut BTreeSet<String>,
    pub attributes: &'a mut BTreeMap<String, String>,
    pub confidence: &'a mut Option<f32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScreenTree {
    pub generation: u64,
    nodes: Vec<Node>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeError(pub String);
impl fmt::Display for TreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for TreeError {}

impl ScreenTree {
    pub fn root(&self) -> NodeId {
        NodeId(0)
    }
    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id.0)
    }
    /// Recognizers can add roles, state/value attributes and evidence confidence.
    /// Links remain private, so annotations cannot introduce tree cycles.
    pub fn annotations_mut(&mut self, id: NodeId) -> Option<NodeAnnotations<'_>> {
        let node = self.nodes.get_mut(id.0)?;
        Some(NodeAnnotations { roles: &mut node.roles, attributes: &mut node.attributes, confidence: &mut node.confidence })
    }
    /// Add a recognizer-produced node. Bounds must lie within the parent.
    pub fn add_node(&mut self, parent: NodeId, mut node: Node) -> Result<NodeId, TreeError> {
        let Some(parent_node) = self.node(parent) else {
            return Err(TreeError("unknown parent".into()));
        };
        if !parent_node.bounds.contains(node.bounds) {
            return Err(TreeError("child lies outside parent".into()));
        }
        if node.parent.is_some() || !node.children.is_empty() {
            return Err(TreeError("node is already attached".into()));
        }
        let id = NodeId(self.nodes.len());
        node.parent = Some(parent);
        self.nodes.push(node);
        self.nodes[parent.0].children.push(id);
        Ok(id)
    }
    /// Preorder document order; children are spatially ordered after analysis.
    pub fn document_order(&self) -> Vec<NodeId> {
        let mut result = Vec::new();
        let mut pending = vec![self.root()];
        while let Some(id) = pending.pop() {
            result.push(id);
            pending.extend(self.nodes[id.0].children.iter().rev().copied());
        }
        result
    }
    pub fn select(&self, source: &str) -> Result<Vec<&Node>, SelectorError> {
        Ok(Selector::parse(source)?.evaluate(self).into_iter().map(|id| &self.nodes[id.0]).collect())
    }
    fn push(&mut self, parent: NodeId, node: Node) -> NodeId {
        self.add_node(parent, node).expect("analysis bounds are contained")
    }
}

fn span_node(cells: &[Cell], col: u16, row: u16) -> Node {
    let first = &cells[0];
    let mut node =
        Node::new("span", Rect { col, row, width: cells.len() as u16, height: 1 }, cells.iter().map(Cell::text).collect::<String>());
    let s = &first.style;
    for (name, value) in [
        ("bold", s.bold),
        ("faint", s.faint),
        ("italic", s.italic),
        ("blink", s.blink),
        ("inverse", s.inverse),
        ("invisible", s.invisible),
        ("strikethrough", s.strikethrough),
        ("overline", s.overline),
        ("underline", s.underline_style != 0),
        ("protected", s.protected),
    ] {
        node.flag(name, value);
    }
    node.attr("fg", s.fg);
    node.attr("bg", s.bg);
    node.attr("underline-style", s.underline_style);
    if let Some(color) = s.underline_color {
        node.attr("underline-color", color);
    }
    if let Some(uri) = &s.hyperlink {
        node.attr("hyperlink", uri);
    }
    let semantic = match first.semantic {
        SemanticContent::Input => "input",
        SemanticContent::Prompt => "prompt",
        SemanticContent::Output => "output",
    };
    node.attr("semantic", semantic);
    node.flag(semantic, true);
    node
}

/// Build observations. Each physical row occurs exactly once. A full-width row
/// nests only under a box that contains its entire bounds; partial-width boxes
/// are observations with their own bounded text, not duplicated physical rows.
pub fn analyze(grid: &ScreenGrid) -> ScreenTree {
    let mut root = Node::new("screen", grid.bounds(), grid.text(grid.bounds()));
    root.attr("generation", grid.generation);
    root.attr("cols", grid.cols());
    root.attr("rows", grid.rows());
    let mut tree = ScreenTree { generation: grid.generation, nodes: vec![root] };
    let mut boxes = detect_boxes(grid);
    boxes.sort_by_key(|b| std::cmp::Reverse(b.rect.area()));
    let mut box_ids: Vec<NodeId> = Vec::new();
    for b in boxes {
        let parent = box_ids
            .iter()
            .copied()
            .filter(|id| tree.nodes[id.0].bounds != b.rect && tree.nodes[id.0].bounds.contains(b.rect))
            .min_by_key(|id| tree.nodes[id.0].bounds.area())
            .unwrap_or(tree.root());
        let mut node = Node::new("box", b.rect, grid.text(b.rect));
        node.confidence = Some(b.confidence);
        node.attr("confidence", b.confidence);
        node.attr("border-style", match b.border_style {
            crate::BorderStyle::Unicode => "unicode",
            crate::BorderStyle::Ascii => "ascii",
        });
        if let Some(title) = b.title {
            node.attr("title", title);
        }
        box_ids.push(tree.push(parent, node));
    }
    let bands = detect_bands(grid);
    for row in 0..grid.rows() {
        let bounds = Rect { col: 0, row, width: grid.cols(), height: 1 };
        let mut parent = box_ids
            .iter()
            .copied()
            .filter(|id| tree.nodes[id.0].bounds.contains(bounds))
            .min_by_key(|id| tree.nodes[id.0].bounds.area())
            .unwrap_or(tree.root());
        if let Some(band) = bands.iter().find(|b| b.rect.row == row) {
            let mut node = Node::new("band", band.rect, band.text.clone());
            node.confidence = Some(band.confidence);
            node.attr("confidence", band.confidence);
            node.attr("kind", match band.kind {
                BandKind::Styled => "styled",
                BandKind::Separator => "separator",
                BandKind::Blank => "blank",
            });
            parent = tree.push(parent, node);
        }
        let metadata = grid.row_metadata()[usize::from(row)];
        let mut node = Node::new("row", bounds, grid.text(bounds));
        node.attr("index", row);
        node.attr("index-from-bottom", grid.rows() - row - 1);
        node.flag("soft-wrap", metadata.soft_wrap);
        node.flag("wrap-continuation", metadata.wrap_continuation);
        node.flag("prompt", metadata.semantic_prompt != SemanticPrompt::None);
        node.attr("semantic-prompt", match metadata.semantic_prompt {
            SemanticPrompt::None => "none",
            SemanticPrompt::Prompt => "prompt",
            SemanticPrompt::Continuation => "continuation",
        });
        let row_id = tree.push(parent, node);
        let cells = grid.row(row).expect("row iteration stays inside validated grid dimensions");
        let mut start = 0;
        for end in 1..=cells.len() {
            if end == cells.len() || cells[end].style != cells[start].style || cells[end].semantic != cells[start].semantic {
                tree.push(row_id, span_node(&cells[start..end], start as u16, row));
                start = end;
            }
        }
        if grid.cursor.visible && grid.cursor.row == row {
            let cursor = grid.cursor;
            let mut node = Node::new("cursor", Rect { col: cursor.col, row, width: 1, height: 1 }, "");
            node.flag("visible", true);
            node.flag("blinking", cursor.blinking);
            node.flag("password-input", cursor.password_input);
            node.flag("wide-tail", cursor.wide_tail);
            node.attr("style", match cursor.style {
                CursorStyle::Bar => "bar",
                CursorStyle::Block => "block",
                CursorStyle::Underline => "underline",
                CursorStyle::Hollow => "hollow",
            });
            tree.push(row_id, node);
        }
    }
    let keys = tree.nodes.iter().map(|n| (n.bounds.row, n.bounds.col)).collect::<Vec<_>>();
    for node in &mut tree.nodes {
        node.children.sort_by_key(|id| keys[id.0]);
    }
    tree
}
