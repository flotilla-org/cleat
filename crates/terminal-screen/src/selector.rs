use std::{
    collections::{BTreeSet, HashMap},
    fmt,
};

use regex::Regex;

use crate::{Node, NodeId, ScreenTree};

/// A parsed, reusable selector. Regexes are compiled once, at parse time.
#[derive(Clone, Debug)]
pub struct Selector {
    groups: Vec<Vec<Segment>>,
}

#[derive(Clone, Debug)]
struct Segment {
    relation: Relation,
    element: Option<String>,
    filters: Vec<Filter>,
}
#[derive(Clone, Copy, Debug)]
enum Relation {
    Descendant,
    Child,
}
#[derive(Clone, Debug)]
enum Filter {
    Attribute(String, Option<(Operator, String)>),
    Nth(usize),
    Last,
    Not(Selector),
    Has(Selector),
    Text(String),
    Matches(Regex),
}
#[derive(Clone, Copy, Debug)]
enum Operator {
    Equal,
    Prefix,
    Contains,
    Suffix,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectorError {
    /// Byte offset in the original selector.
    pub offset: usize,
    pub message: String,
}
impl fmt::Display for SelectorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at byte {}", self.message, self.offset)
    }
}
impl std::error::Error for SelectorError {}

impl Selector {
    pub fn parse(source: &str) -> Result<Self, SelectorError> {
        if source.len() > 16_384 {
            return Err(SelectorError { offset: 0, message: "selector exceeds 16384 bytes".into() });
        }
        let mut parser = Parser { source, offset: 0 };
        let selector = parser.selector(0, false)?;
        parser.whitespace();
        if parser.peek().is_some() {
            return parser.error("unexpected trailing input");
        }
        Ok(selector)
    }
    /// Matches in preorder document order, with selector groups deduplicated.
    pub fn evaluate(&self, tree: &ScreenTree) -> Vec<NodeId> {
        let mut evaluation = Evaluation::new(tree);
        evaluation.query(self, None);
        // query always caches a result, including an empty match set.
        evaluation.cache.remove(&(self as *const Self, None)).unwrap().ordered
    }
}

struct Matches {
    ordered: Vec<NodeId>,
    members: BTreeSet<NodeId>,
}

// Pointer identity is never dereferenced: the immutable selector tree is
// borrowed for the whole evaluation, so its addresses cannot move during use.
type QueryKey = (*const Selector, Option<NodeId>);

/// One query owns one preorder index and memo table. Selector addresses are
/// stable identities only during this evaluation; nothing survives a frame.
struct Evaluation<'a> {
    tree: &'a ScreenTree,
    order: Vec<NodeId>,
    positions: Vec<usize>,
    ends: Vec<usize>,
    cache: HashMap<QueryKey, Matches>,
}

impl<'a> Evaluation<'a> {
    fn new(tree: &'a ScreenTree) -> Self {
        let order = tree.document_order();
        let mut positions = vec![0; order.len()];
        let mut ends = vec![0; order.len()];
        for (position, id) in order.iter().enumerate() {
            positions[id.0] = position;
            ends[id.0] = position + 1;
        }
        for id in order.iter().rev() {
            if let Some(parent) = tree.node(*id).unwrap().parent() {
                ends[parent.0] = ends[parent.0].max(ends[id.0]);
            }
        }
        Self { tree, order, positions, ends, cache: HashMap::new() }
    }

    fn query(&mut self, selector: &Selector, scope: Option<NodeId>) {
        let key = (selector as *const Selector, scope);
        if self.cache.contains_key(&key) {
            return;
        }
        // Preorder makes every subtree contiguous. :has scans only strict
        // descendants, while its :not arguments retain documented global scope.
        let start = scope.map_or(0, |id| self.positions[id.0] + 1);
        let end = scope.map_or(self.order.len(), |id| self.ends[id.0]);
        let mut union = BTreeSet::new();
        for group in &selector.groups {
            let mut previous = Vec::new();
            for (index, segment) in group.iter().enumerate() {
                let related = if index == 0 {
                    if matches!(segment.relation, Relation::Child) {
                        self.related(&scope.into_iter().collect::<Vec<_>>(), Relation::Child, start, end)
                    } else {
                        vec![true; end - start]
                    }
                } else {
                    self.related(&previous, segment.relation, start, end)
                };
                let mut matches = Vec::new();
                for (offset, in_scope) in related.into_iter().enumerate() {
                    let id = self.order[start + offset];
                    if in_scope && self.matches(segment, id) {
                        matches.push(id);
                    }
                }
                // xa11y indices apply to segment matches, not CSS siblings.
                for filter in &segment.filters {
                    match filter {
                        Filter::Nth(n) => matches = matches.get(n - 1).copied().into_iter().collect(),
                        Filter::Last => matches = matches.last().copied().into_iter().collect(),
                        _ => {}
                    }
                }
                previous = matches;
            }
            union.extend(previous);
        }
        let ordered = self.order[start..end].iter().copied().filter(|id| union.contains(id)).collect();
        self.cache.insert(key, Matches { ordered, members: union });
    }

    fn related(&self, parents: &[NodeId], relation: Relation, start: usize, end: usize) -> Vec<bool> {
        match relation {
            Relation::Child => {
                let mut related = vec![false; end - start];
                for parent in parents {
                    for child in self.tree.node(*parent).unwrap().children() {
                        let position = self.positions[child.0];
                        if (start..end).contains(&position) {
                            related[position - start] = true;
                        }
                    }
                }
                related
            }
            Relation::Descendant => {
                // Mark subtree intervals in a difference array, avoiding the
                // previous-set × candidate × ancestor-depth matching loop.
                let mut changes = vec![0isize; end - start + 1];
                for parent in parents {
                    let lower = (self.positions[parent.0] + 1).max(start);
                    let upper = self.ends[parent.0].min(end);
                    if lower < upper {
                        changes[lower - start] += 1;
                        changes[upper - start] -= 1;
                    }
                }
                let mut active = 0;
                changes[..end - start]
                    .iter()
                    .map(|change| {
                        active += change;
                        active > 0
                    })
                    .collect()
            }
        }
    }

    fn matches(&mut self, segment: &Segment, id: NodeId) -> bool {
        let node = self.tree.node(id).unwrap();
        if segment.element.as_ref().is_some_and(|element| node.element != *element && !node.roles.contains(element)) {
            return false;
        }
        segment.filters.iter().all(|filter| match filter {
            Filter::Attribute(name, comparison) => attribute_matches(node, name, comparison.as_ref()),
            Filter::Not(selector) => {
                self.query(selector, None);
                !self.cache[&(selector as *const Selector, None)].members.contains(&id)
            }
            Filter::Has(selector) => {
                self.query(selector, Some(id));
                !self.cache[&(selector as *const Selector, Some(id))].ordered.is_empty()
            }
            Filter::Text(text) => node.text.contains(text),
            Filter::Matches(regex) => regex.is_match(&node.text),
            Filter::Nth(_) | Filter::Last => true,
        })
    }
}

fn attribute_matches(node: &Node, name: &str, comparison: Option<&(Operator, String)>) -> bool {
    let Some(actual) = node.attribute(name) else {
        return false;
    };
    let Some((op, expected)) = comparison else {
        return true;
    };
    match op {
        Operator::Equal => actual == expected.as_str(),
        Operator::Prefix => actual.to_lowercase().starts_with(expected),
        Operator::Contains => actual.to_lowercase().contains(expected),
        Operator::Suffix => actual.to_lowercase().ends_with(expected),
    }
}

struct Parser<'a> {
    source: &'a str,
    offset: usize,
}
impl Parser<'_> {
    fn error<T>(&self, message: impl Into<String>) -> Result<T, SelectorError> {
        Err(SelectorError { offset: self.offset, message: message.into() })
    }
    fn peek(&self) -> Option<char> {
        self.source[self.offset..].chars().next()
    }
    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.offset += c.len_utf8();
        Some(c)
    }
    fn eat(&mut self, expected: char) -> bool {
        if self.peek() == Some(expected) {
            self.bump();
            true
        } else {
            false
        }
    }
    fn expect(&mut self, expected: char) -> Result<(), SelectorError> {
        if self.eat(expected) {
            Ok(())
        } else {
            self.error(format!("expected '{expected}'"))
        }
    }
    fn whitespace(&mut self) -> bool {
        let start = self.offset;
        while self.peek().is_some_and(char::is_whitespace) {
            self.bump();
        }
        self.offset != start
    }
    fn identifier(&mut self) -> Result<String, SelectorError> {
        let start = self.offset;
        while self.peek().is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-')) {
            self.bump();
        }
        if start == self.offset {
            return self.error("expected name");
        }
        Ok(self.source[start..self.offset].to_string())
    }
    fn quoted(&mut self) -> Result<String, SelectorError> {
        let Some(quote @ ('\'' | '"')) = self.bump() else {
            return self.error("expected quoted string");
        };
        let mut value = String::new();
        while let Some(c) = self.bump() {
            if c == quote {
                return Ok(value);
            }
            if c == '\\' {
                let Some(escaped) = self.bump() else {
                    return self.error("unterminated escape");
                };
                if escaped != quote && escaped != '\\' {
                    value.push('\\');
                }
                value.push(escaped);
            } else {
                value.push(c);
            }
        }
        self.error("unterminated string")
    }
    fn selector(&mut self, depth: usize, relative: bool) -> Result<Selector, SelectorError> {
        if depth >= 32 {
            return self.error("selector nesting exceeds 32");
        }
        self.whitespace();
        let mut groups = Vec::new();
        loop {
            let mut relation = Relation::Descendant;
            if relative && self.eat('>') {
                relation = Relation::Child;
                self.whitespace();
            }
            let mut segments = vec![self.segment(depth, relation)?];
            loop {
                let space = self.whitespace();
                if self.peek().is_none() || matches!(self.peek(), Some(')' | ',')) {
                    break;
                }
                let relation = if self.eat('>') {
                    self.whitespace();
                    Relation::Child
                } else if space {
                    Relation::Descendant
                } else {
                    return self.error("expected combinator");
                };
                segments.push(self.segment(depth, relation)?);
            }
            groups.push(segments);
            if !self.eat(',') {
                break;
            }
            self.whitespace();
        }
        Ok(Selector { groups })
    }
    fn segment(&mut self, depth: usize, relation: Relation) -> Result<Segment, SelectorError> {
        let start = self.offset;
        let element = if self.eat('*') {
            None
        } else if self.peek().is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-')) {
            Some(self.identifier()?)
        } else {
            None
        };
        let mut filters = Vec::new();
        loop {
            if self.eat('[') {
                self.whitespace();
                let name = self.identifier()?;
                self.whitespace();
                let comparison = if self.eat(']') {
                    None
                } else {
                    let op = match self.bump() {
                        Some('=') => Operator::Equal,
                        Some('^') => {
                            self.expect('=')?;
                            Operator::Prefix
                        }
                        Some('*') => {
                            self.expect('=')?;
                            Operator::Contains
                        }
                        Some('$') => {
                            self.expect('=')?;
                            Operator::Suffix
                        }
                        _ => return self.error("expected attribute operator (=, ^=, *=, $=)"),
                    };
                    self.whitespace();
                    let value = if matches!(self.peek(), Some('\'' | '"')) { self.quoted()? } else { self.identifier()? };
                    self.whitespace();
                    self.expect(']')?;
                    let value = if matches!(op, Operator::Equal) { value } else { value.to_lowercase() };
                    Some((op, value))
                };
                filters.push(Filter::Attribute(name, comparison));
            } else if self.eat(':') {
                let name = self.identifier()?;
                if name == "last" {
                    filters.push(Filter::Last);
                    continue;
                }
                self.expect('(')?;
                self.whitespace();
                let filter = match name.as_str() {
                    "nth" => {
                        let start = self.offset;
                        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                            self.bump();
                        }
                        let n = self.source[start..self.offset].parse::<usize>().ok().filter(|n| *n > 0);
                        Filter::Nth(
                            n.ok_or_else(|| SelectorError { offset: start, message: "nth requires a positive one-based integer".into() })?,
                        )
                    }
                    "not" => Filter::Not(self.selector(depth + 1, false)?),
                    "has" => Filter::Has(self.selector(depth + 1, true)?),
                    "has-text" => Filter::Text(self.quoted()?),
                    "matches" => {
                        self.expect('/')?;
                        let mut pattern = String::new();
                        loop {
                            match self.bump() {
                                None => return self.error("unterminated regex"),
                                Some('/') => break,
                                Some('\\') => {
                                    let Some(c) = self.bump() else {
                                        return self.error("unterminated regex escape");
                                    };
                                    if c != '/' {
                                        pattern.push('\\');
                                    }
                                    pattern.push(c);
                                }
                                Some(c) => pattern.push(c),
                            }
                        }
                        let regex = regex::RegexBuilder::new(&pattern)
                            .size_limit(1 << 20)
                            .build()
                            .map_err(|err| SelectorError { offset: self.offset, message: format!("invalid regex: {err}") })?;
                        Filter::Matches(regex)
                    }
                    _ => return self.error(format!("unknown pseudo-class '{name}'")),
                };
                self.whitespace();
                self.expect(')')?;
                filters.push(filter);
            } else {
                break;
            }
        }
        if self.offset == start {
            return self.error("empty selector segment");
        }
        Ok(Segment { relation, element, filters })
    }
}
