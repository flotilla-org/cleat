use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use crate::{Cell, Rect, ScreenGrid, Selector, SelectorError, SemanticContent};

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
    /// A tree with only a root; producers other than `analyze` attach nodes with `add_node`.
    pub fn from_root(generation: u64, root: Node) -> Self {
        Self { generation, nodes: vec![root] }
    }
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
}

pub(crate) fn span_node(cells: &[Cell], col: u16, row: u16) -> Node {
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

/// Analyze one frame with the default graph region producer.
pub fn analyze(grid: &ScreenGrid) -> ScreenTree {
    crate::segment::GraphSegmenter::default().segment(grid).to_screen_tree(grid)
}
