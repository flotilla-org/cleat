use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
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
    Adjacent,
    Sibling,
}
#[derive(Clone, Debug)]
enum Filter {
    Attribute(String, Option<(Operator, String, bool)>),
    Nth(usize),
    Last,
    FirstChild,
    LastChild,
    NthChild(usize, Option<Selector>),
    Not(Selector),
    Has(Selector),
    Text(String, bool),
    Matches(Regex),
}
#[derive(Clone, Copy, Debug)]
enum Operator {
    Equal,
    NotEqual,
    Prefix,
    Contains,
    Suffix,
}

/// One matched node and named groups from positive :matches predicates on it.
/// Groups in :has/:not are predicates only; absent optional captures are omitted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectorMatch {
    pub node: NodeId,
    pub captures: BTreeMap<String, String>,
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
    /// Match results include named regex groups on the selected node.
    pub fn evaluate_with_captures(&self, tree: &ScreenTree) -> Vec<SelectorMatch> {
        let mut evaluation = Evaluation::new(tree);
        evaluation.query(self, None);
        let mut result = evaluation.cache.remove(&(self as *const Self, None)).expect("query caches its result");
        result.ordered.into_iter().map(|node| SelectorMatch { node, captures: result.captures.remove(&node).unwrap_or_default() }).collect()
    }
    fn has_positions(&self) -> bool {
        self.groups.iter().flatten().flat_map(|s| &s.filters).any(|f| match f {
            Filter::Nth(_) | Filter::Last | Filter::FirstChild | Filter::LastChild | Filter::NthChild(..) => true,
            Filter::Has(s) | Filter::Not(s) => s.has_positions(),
            _ => false,
        })
    }
    // Negation of a comparison is unknown if its subject attribute is absent.
    // Any unknown alternative prevents a nonmatch from becoming true.
    fn known(&self, node: &Node) -> bool {
        self.groups.iter().all(|g| {
            g.last().expect("nonempty group").filters.iter().all(|f| match f {
                Filter::Attribute(name, Some(_)) => node.attribute(name).is_some(),
                Filter::Not(s) => s.known(node),
                _ => true,
            })
        })
    }
    /// Matches in preorder document order, with selector groups deduplicated.
    pub fn evaluate(&self, tree: &ScreenTree) -> Vec<NodeId> {
        let mut evaluation = Evaluation::new(tree);
        evaluation.query(self, None);
        // query always caches a result, including an empty match set.
        evaluation.cache.remove(&(self as *const Self, None)).expect("query caches its result, even for an empty match set").ordered
    }
}

struct Matches {
    ordered: Vec<NodeId>,
    members: BTreeSet<NodeId>,
    captures: BTreeMap<NodeId, BTreeMap<String, String>>,
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
            if let Some(parent) = tree.node(*id).expect("preorder contains only attached nodes").parent() {
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
        let sibling_relative = selector.groups.iter().any(|g| matches!(g[0].relation, Relation::Adjacent | Relation::Sibling));
        let range_scope = if sibling_relative { scope.and_then(|id| self.tree.node(id).expect("scope node").parent()) } else { scope };
        let start = range_scope.map_or(0, |id| self.positions[id.0] + 1);
        let end = range_scope.map_or(self.order.len(), |id| self.ends[id.0]);
        let mut union = BTreeSet::new();
        let mut captures = BTreeMap::new();
        for group in &selector.groups {
            let mut previous = Vec::new();
            for (index, segment) in group.iter().enumerate() {
                let related = if index == 0 {
                    if let Some(scope) = scope {
                        self.related(&[scope], segment.relation, start, end)
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
            for &id in &previous {
                let values = captures.entry(id).or_insert_with(BTreeMap::new);
                for filter in &group.last().expect("nonempty group").filters {
                    if let Filter::Matches(regex) = filter {
                        if let Some(matched) = regex.captures(&self.tree.node(id).expect("matched node").text) {
                            for name in regex.capture_names().flatten() {
                                if let Some(value) = matched.name(name) {
                                    values.insert(name.to_string(), value.as_str().to_string());
                                }
                            }
                        }
                    }
                }
            }
            union.extend(previous);
        }
        let ordered = self.order[start..end].iter().copied().filter(|id| union.contains(id)).collect();
        self.cache.insert(key, Matches { ordered, members: union, captures });
    }

    fn related(&self, parents: &[NodeId], relation: Relation, start: usize, end: usize) -> Vec<bool> {
        match relation {
            Relation::Child => {
                let mut related = vec![false; end - start];
                for parent in parents {
                    for child in self.tree.node(*parent).expect("matched parents are attached nodes").children() {
                        let position = self.positions[child.0];
                        if (start..end).contains(&position) {
                            related[position - start] = true;
                        }
                    }
                }
                related
            }
            Relation::Adjacent | Relation::Sibling => {
                let mut related = vec![false; end - start];
                for id in parents {
                    if let Some(parent) = self.tree.node(*id).expect("matched node").parent() {
                        let siblings = self.tree.node(parent).expect("attached parent").children();
                        let index = siblings.iter().position(|s| s == id).expect("parent contains child");
                        for sibling in &siblings[index + 1..] {
                            let position = self.positions[sibling.0];
                            if (start..end).contains(&position) {
                                related[position - start] = true;
                            }
                            if matches!(relation, Relation::Adjacent) {
                                break;
                            }
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
        let node = self.tree.node(id).expect("candidate IDs come from tree preorder");
        if segment.element.as_ref().is_some_and(|element| node.element != *element && !node.roles.contains(element)) {
            return false;
        }
        segment.filters.iter().all(|filter| match filter {
            Filter::Attribute(name, comparison) => attribute_matches(node, name, comparison.as_ref()),
            Filter::Not(selector) => {
                self.query(selector, None);
                selector.known(node) && !self.cache[&(selector as *const Selector, None)].members.contains(&id)
            }
            Filter::Has(selector) => {
                self.query(selector, Some(id));
                !self.cache[&(selector as *const Selector, Some(id))].ordered.is_empty()
            }
            Filter::Text(text, insensitive) => {
                if *insensitive {
                    node.text.to_lowercase().contains(text)
                } else {
                    node.text.contains(text)
                }
            }
            Filter::FirstChild => node.parent().is_some_and(|p| self.tree.node(p).expect("parent").children().first() == Some(&id)),
            Filter::LastChild => node.parent().is_some_and(|p| self.tree.node(p).expect("parent").children().last() == Some(&id)),
            Filter::NthChild(n, selector) => {
                if let Some(p) = node.parent() {
                    if let Some(s) = selector {
                        self.query(s, None);
                    }
                    let siblings = self.tree.node(p).expect("parent").children();
                    siblings
                        .iter()
                        .copied()
                        .filter(|s| selector.as_ref().is_none_or(|sel| self.cache[&(sel as *const Selector, None)].members.contains(s)))
                        .nth(n - 1)
                        == Some(id)
                } else {
                    false
                }
            }
            Filter::Matches(regex) => regex.is_match(&node.text),
            Filter::Nth(_) | Filter::Last => true,
        })
    }
}

fn attribute_matches(node: &Node, name: &str, comparison: Option<&(Operator, String, bool)>) -> bool {
    let Some(actual) = node.attribute(name) else {
        return false;
    };
    let Some((op, expected, insensitive)) = comparison else {
        return true;
    };
    let actual = if *insensitive { actual.to_lowercase() } else { actual.to_string() };
    match op {
        Operator::Equal => actual == *expected,
        Operator::NotEqual => actual != *expected,
        Operator::Prefix => actual.starts_with(expected),
        Operator::Contains => actual.contains(expected),
        Operator::Suffix => actual.ends_with(expected),
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
            if relative {
                if let Some(r) = self.combinator() {
                    relation = r;
                    self.whitespace();
                }
            }
            let mut segments = vec![self.segment(depth, relation)?];
            loop {
                let space = self.whitespace();
                if self.peek().is_none() || matches!(self.peek(), Some(')' | ',')) {
                    break;
                }
                let relation = if let Some(relation) = self.combinator() {
                    self.whitespace();
                    relation
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
    fn combinator(&mut self) -> Option<Relation> {
        match self.peek()? {
            '>' => {
                self.bump();
                Some(Relation::Child)
            }
            '+' => {
                self.bump();
                Some(Relation::Adjacent)
            }
            '~' => {
                self.bump();
                Some(Relation::Sibling)
            }
            _ => None,
        }
    }
    fn positive_index(&mut self) -> Result<usize, SelectorError> {
        let start = self.offset;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.bump();
        }
        self.source[start..self.offset]
            .parse::<usize>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| SelectorError { offset: start, message: "index requires a positive one-based integer".into() })
    }
    fn case_flag(&mut self) -> Result<bool, SelectorError> {
        let space = self.whitespace();
        let insensitive = space && self.eat('i');
        if insensitive {
            self.whitespace();
        }
        Ok(insensitive)
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
                        Some('!') => {
                            self.expect('=')?;
                            Operator::NotEqual
                        }
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
                        _ => return self.error("expected attribute operator (=, !=, ^=, *=, $=)"),
                    };
                    self.whitespace();
                    let value = if matches!(self.peek(), Some('\'' | '"')) { self.quoted()? } else { self.identifier()? };
                    let insensitive = self.case_flag()?;
                    self.expect(']')?;
                    let value = if insensitive { value.to_lowercase() } else { value };
                    Some((op, value, insensitive))
                };
                filters.push(Filter::Attribute(name, comparison));
            } else if self.eat(':') {
                let name = self.identifier()?;
                match name.as_str() {
                    "last-match" => {
                        filters.push(Filter::Last);
                        continue;
                    }
                    "first-child" => {
                        filters.push(Filter::FirstChild);
                        continue;
                    }
                    "last-child" => {
                        filters.push(Filter::LastChild);
                        continue;
                    }
                    _ => {}
                }
                self.expect('(')?;
                self.whitespace();
                let filter = match name.as_str() {
                    "nth-match" => Filter::Nth(self.positive_index()?),
                    "nth-child" => {
                        let n = self.positive_index()?;
                        let space = self.whitespace();
                        let selector = if space && self.peek() == Some('o') {
                            if self.identifier()? != "of" || !self.whitespace() {
                                return self.error("expected 'of' and a selector");
                            }
                            let selector = self.selector(depth + 1, false)?;
                            if selector.has_positions() {
                                return self.error("positional filters are not allowed in nth-child's of selector");
                            }
                            Some(selector)
                        } else {
                            None
                        };
                        Filter::NthChild(n, selector)
                    }
                    "not" => {
                        let selector = self.selector(depth + 1, false)?;
                        if selector.has_positions() {
                            return self.error("positional filters are not allowed inside :not");
                        }
                        Filter::Not(selector)
                    }
                    "has" => Filter::Has(self.selector(depth + 1, true)?),
                    "has-text" => {
                        let text = self.quoted()?;
                        let insensitive = self.case_flag()?;
                        Filter::Text(if insensitive { text.to_lowercase() } else { text }, insensitive)
                    }
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
                        let insensitive = self.eat('i');
                        let regex = regex::RegexBuilder::new(&pattern)
                            .case_insensitive(insensitive)
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
