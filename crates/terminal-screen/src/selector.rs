use std::fmt;

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
        self.scoped(tree, None)
    }

    fn scoped(&self, tree: &ScreenTree, scope: Option<NodeId>) -> Vec<NodeId> {
        let order = tree.document_order();
        let mut union = std::collections::BTreeSet::new();
        for group in &self.groups {
            let mut previous = Vec::new();
            for (index, segment) in group.iter().enumerate() {
                // Negations are frame-wide predicates: evaluate each once per
                // segment, rather than once again for every candidate node.
                let excluded = segment
                    .filters
                    .iter()
                    .filter_map(|filter| if let Filter::Not(selector) = filter { Some(selector.evaluate(tree)) } else { None })
                    .flatten()
                    .collect::<std::collections::BTreeSet<_>>();
                let mut matches = order
                    .iter()
                    .copied()
                    .filter(|id| {
                        let related = |parent: NodeId| match segment.relation {
                            Relation::Child => tree.node(*id).unwrap().parent() == Some(parent),
                            Relation::Descendant => tree.descendant_of(*id, parent),
                        };
                        let in_scope = if index == 0 { scope.is_none_or(related) } else { previous.iter().copied().any(related) };
                        in_scope && !excluded.contains(id) && segment.matches(tree, *id)
                    })
                    .collect::<Vec<_>>();
                // xa11y indices apply to the segment's matches, not siblings.
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
        order.into_iter().filter(|id| union.contains(id)).collect()
    }
}

impl Segment {
    fn matches(&self, tree: &ScreenTree, id: NodeId) -> bool {
        let node = tree.node(id).unwrap();
        if self.element.as_ref().is_some_and(|element| node.element != *element && !node.roles.contains(element)) {
            return false;
        }
        self.filters.iter().all(|filter| match filter {
            Filter::Attribute(name, comparison) => attribute_matches(node, name, comparison),
            Filter::Not(_) => true,
            Filter::Has(selector) => !selector.scoped(tree, Some(id)).is_empty(),
            Filter::Text(text) => node.text.contains(text),
            Filter::Matches(regex) => regex.is_match(&node.text),
            Filter::Nth(_) | Filter::Last => true,
        })
    }
}

fn attribute_matches(node: &Node, name: &str, comparison: &Option<(Operator, String)>) -> bool {
    let Some(actual) = node.attribute(name) else {
        return false;
    };
    let Some((op, expected)) = comparison else {
        return true;
    };
    match op {
        Operator::Equal => actual == expected,
        Operator::Prefix => actual.to_lowercase().starts_with(&expected.to_lowercase()),
        Operator::Contains => actual.to_lowercase().contains(&expected.to_lowercase()),
        Operator::Suffix => actual.to_lowercase().ends_with(&expected.to_lowercase()),
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
