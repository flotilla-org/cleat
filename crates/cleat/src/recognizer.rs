//! Opt-in application observations. Rules never participate in input admission.
use std::{
    collections::BTreeMap,
    panic::{catch_unwind, AssertUnwindSafe},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use terminal_screen::{Node, NodeId, ScreenTree, Selector};

use crate::program_status::{ProgramKind, ProgramState, ProgramStatusRecord, ProgramStatusSource};

pub const ENGINE_VERSION: &str = "0.1.0";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    id: String,
    revision: u64,
    min_engine: String,
    verified: Vec<String>,
    #[serde(default)]
    rule: Vec<toml::Spanned<Declaration>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Declaration {
    select: String,
    role: Option<String>,
    state: Option<ProgramState>,
    kind: Option<ProgramKind>,
    value: Option<String>,
}
struct Rule {
    selector: Selector,
    declaration: Declaration,
}

/// A whole file is validated and compiled before it can be installed.
pub struct Rules {
    pub id: String,
    pub revision: u64,
    rules: Vec<Rule>,
}
impl Rules {
    pub fn parse(path: &Path, source: &str) -> Result<Self, String> {
        let error = |offset: usize, rule: Option<usize>, message: String| {
            let line = source[..offset.min(source.len())].bytes().filter(|b| *b == b'\n').count() + 1;
            format!("{}:{line}: rule {}: {message}", path.display(), rule.map(|r| (r + 1).to_string()).unwrap_or_else(|| "file".into()))
        };
        let file: File = toml::from_str(source).map_err(|e: toml::de::Error| {
            let offset = e.span().map_or(0, |s| s.start);
            let count = source[..offset.min(source.len())].matches("[[rule]]").count();
            error(offset, count.checked_sub(1), e.message().into())
        })?;
        if !valid_id(&file.id) {
            return Err(error(0, None, "id must contain only ASCII letters, digits, - or _".into()));
        }
        let minimum = semver::Version::parse(&file.min_engine).map_err(|e| error(0, None, e.to_string()))?;
        if minimum > semver::Version::parse(ENGINE_VERSION).unwrap() {
            return Err(error(0, None, "requires a newer recognizer engine".into()));
        }
        for verified in &file.verified {
            let Some((harness, range)) = verified.split_once(' ') else {
                return Err(error(0, None, "verified requires a harness and semver range".into()));
            };
            if harness.is_empty() {
                return Err(error(0, None, "empty harness".into()));
            }
            semver::VersionReq::parse(range).map_err(|e| error(0, None, e.to_string()))?;
        }
        let mut rules = Vec::new();
        for (index, declaration) in file.rule.into_iter().enumerate() {
            let offset = declaration.span().start;
            let declaration = declaration.into_inner();
            let fail = |message| error(offset, Some(index), message);
            if declaration.role.is_none() && declaration.state.is_none() {
                return Err(fail("a rule needs role or state".into()));
            }
            if declaration.role.as_ref().is_some_and(|r| !valid_id(r)) {
                return Err(fail("invalid role".into()));
            }
            if declaration.kind.is_some() && declaration.state != Some(ProgramState::Blocked) {
                return Err(fail("kind requires blocked state".into()));
            }
            if let Some(value) = &declaration.value {
                template(value, &BTreeMap::new(), false).map_err(&fail)?;
            }
            let selector = Selector::parse(&declaration.select).map_err(|e| fail(e.to_string()))?;
            if let Some(value) = &declaration.value {
                let names = selector.capture_names().into_iter().map(|name| (name, String::new())).collect();
                template(value, &names, true).map_err(&fail)?;
            }
            rules.push(Rule { selector, declaration });
        }
        Ok(Self { id: file.id, revision: file.revision, rules })
    }
    pub fn load(path: &Path) -> Result<Self, String> {
        let source = std::fs::read_to_string(path).map_err(|e| format!("{}:1: rule file: {e}", path.display()))?;
        Self::parse(path, &source)
    }
}
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
fn template(value: &str, captures: &BTreeMap<String, String>, strict: bool) -> Result<String, String> {
    let mut result = String::new();
    let mut rest = value;
    while let Some(start) = rest.find('{') {
        result.push_str(&rest[..start]);
        let Some(end) = rest[start + 1..].find('}') else {
            return Err("unclosed capture reference".into());
        };
        let name = &rest[start + 1..start + 1 + end];
        if !valid_id(name) {
            return Err("invalid capture reference".into());
        }
        match captures.get(name) {
            Some(text) => result.push_str(text),
            None if strict => return Err(format!("capture {name} is absent")),
            None => {}
        }
        rest = &rest[start + end + 2..];
    }
    if rest.contains('}') {
        return Err("unexpected closing brace".into());
    }
    result.push_str(rest);
    Ok(result)
}

/// Add-on files override bundled files by id. No command-name inference occurs.
pub fn addon_directory() -> PathBuf {
    if let Some(path) = std::env::var_os("CLEAT_RECOGNIZERS_DIR") {
        return PathBuf::from(path);
    }
    if cfg!(windows) {
        if let Some(base) = std::env::var_os("APPDATA") {
            return PathBuf::from(base).join("cleat/recognizers");
        }
    }
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| {
        std::env::var_os(if cfg!(windows) { "APPDATA" } else { "HOME" }).map(PathBuf::from).unwrap_or_default().join(".config")
    });
    base.join("cleat/recognizers")
}
pub fn load_selected(tags: &[String]) -> Result<Option<Rules>, String> {
    let ids: Vec<_> = tags.iter().filter_map(|t| t.strip_prefix("recognizer:")).collect();
    if ids.len() > 1 {
        return Err("select exactly one recognizer".into());
    }
    let Some(id) = ids.first() else {
        return Ok(None);
    };
    if !valid_id(id) {
        return Err("invalid recognizer id".into());
    }
    let addon = addon_directory().join(format!("{id}.toml"));
    let rules = if addon.exists() {
        Rules::load(&addon)?
    } else {
        match *id {
            "synthetic" => Rules::parse(Path::new("recognizers/synthetic.toml"), include_str!("../../../recognizers/synthetic.toml"))?,
            _ => return Err(format!("recognizer {id} not found in {}", addon_directory().display())),
        }
    };
    if rules.id != *id {
        return Err("recognizer filename and id differ".into());
    }
    Ok(Some(rules))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Role {
    pub role: String,
    pub value: String,
    pub col: u16,
    pub row: u16,
    pub width: u16,
    pub height: u16,
    pub provenance: String,
    pub state: Option<ProgramState>,
    pub kind: Option<ProgramKind>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Explanation {
    pub rule: usize,
    pub matched: bool,
    pub captures: Vec<BTreeMap<String, String>>,
    pub provenance: Vec<String>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub roles: Vec<Role>,
    pub state: Option<ProgramState>,
    pub kind: Option<ProgramKind>,
    pub source: Option<ProgramStatusSource>,
    pub explanations: Vec<Explanation>,
    /// A matched screen rule (rather than an ambiguous title) establishes idle.
    pub visible: bool,
}
/// Escape hatch for applications whose observations cannot be expressed by selectors.
/// Implementations return data; the engine attaches nodes only after successful evaluation.
pub trait Recognizer: Send {
    fn id(&self) -> &str;
    fn revision(&self) -> u64;
    fn evaluate(&mut self, tree: &ScreenTree, title: Option<&str>) -> Result<Observation, String>;
}
fn provenance(tree: &ScreenTree, mut id: NodeId) -> String {
    loop {
        let node = tree.node(id).unwrap();
        if let Some(source) = node.attribute("provenance") {
            return source.into();
        }
        match node.parent() {
            Some(parent) => id = parent,
            None => return "screen".into(),
        }
    }
}
impl Recognizer for Rules {
    fn id(&self) -> &str {
        &self.id
    }
    fn revision(&self) -> u64 {
        self.revision
    }
    fn evaluate(&mut self, tree: &ScreenTree, title: Option<&str>) -> Result<Observation, String> {
        let mut input = tree.clone();
        if let Some(title) = title {
            let bounds = input.node(input.root()).unwrap().bounds;
            let mut node = Node::new("title", bounds, title);
            node.attributes.insert("provenance".into(), "title".into());
            input.add_node(input.root(), node).map_err(|e| e.to_string())?;
        }
        let mut observation = Observation::default();
        for (index, rule) in self.rules.iter().enumerate() {
            let mut matches = rule.selector.evaluate_with_captures(&input);
            if matches.iter().any(|m| provenance(&input, m.node) == "declared") {
                matches.retain(|m| provenance(&input, m.node) == "declared");
            }
            let mut explanation =
                Explanation { rule: index + 1, matched: !matches.is_empty(), captures: Vec::new(), provenance: Vec::new() };
            for matched in matches {
                let node = input.node(matched.node).unwrap();
                let source = provenance(&input, matched.node);
                let value = match &rule.declaration.value {
                    Some(value) => template(value, &matched.captures, true)?,
                    None => node.text.clone(),
                };
                if let Some(role) = &rule.declaration.role {
                    let b = node.bounds;
                    observation.roles.push(Role {
                        role: role.clone(),
                        value,
                        col: b.col,
                        row: b.row,
                        width: b.width,
                        height: b.height,
                        provenance: source.clone(),
                        state: rule.declaration.state,
                        kind: rule.declaration.kind,
                    });
                }
                if let Some(state) = rule.declaration.state {
                    observation.state = Some(state);
                    observation.kind = rule.declaration.kind;
                    observation.source =
                        Some(if node.element == "title" { ProgramStatusSource::Title } else { ProgramStatusSource::Recognizer });
                    observation.visible = node.element != "title";
                }
                explanation.captures.push(matched.captures);
                explanation.provenance.push(source);
            }
            observation.explanations.push(explanation);
        }
        Ok(observation)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inspect {
    pub id: String,
    pub revision: u64,
    pub error: Option<String>,
    pub observation: Observation,
}
/// One instance per session. Time is injected as monotonic milliseconds.
pub struct Engine {
    recognizer: Box<dyn Recognizer>,
    key: Option<(u64, Option<String>)>,
    pub inspect: Inspect,
    pending: Option<(u64, u64, u8)>,
    state: Option<ProgramState>,
    last_status: Option<(ProgramState, Option<ProgramKind>, ProgramStatusSource)>,
    candidate: Observation,
}
impl Engine {
    pub fn new(recognizer: Box<dyn Recognizer>) -> Self {
        let inspect =
            Inspect { id: recognizer.id().into(), revision: recognizer.revision(), error: None, observation: Observation::default() };
        Self { recognizer, key: None, inspect, pending: None, state: None, last_status: None, candidate: Observation::default() }
    }
    pub fn stop_confirmations(&mut self) {
        self.pending = None;
    }
    pub fn needs_update(&self, generation: u64, title: Option<&str>, now: u64) -> bool {
        self.key.as_ref().is_none_or(|(old, old_title)| *old != generation || old_title.as_deref() != title)
            || self.deadline().is_some_and(|deadline| now >= deadline)
    }
    pub fn deadline(&self) -> Option<u64> {
        self.pending.map(|(start, last, _)| (last + 100).min(start + 700))
    }
    /// Returns a status only on a confirmed change. Repeated frames reuse the cached observation.
    pub fn evaluate(&mut self, tree: &mut ScreenTree, title: Option<&str>, now: u64) -> Option<ProgramStatusRecord> {
        let key = (tree.generation, title.map(str::to_owned));
        if self.key.as_ref() != Some(&key) {
            self.key = Some(key);
            match catch_unwind(AssertUnwindSafe(|| self.recognizer.evaluate(tree, title))) {
                Ok(Ok(observation)) => {
                    self.candidate = observation;
                    self.inspect.error = None;
                }
                result => {
                    self.inspect.error = Some(match result {
                        Ok(Err(error)) => error,
                        _ => "recognizer panicked".into(),
                    });
                    self.candidate = Observation::default();
                    self.pending = None;
                }
            }
        }
        let observation = &self.candidate;
        for role in &observation.roles {
            let mut node = Node::new(
                &role.role,
                terminal_screen::Rect { col: role.col, row: role.row, width: role.width, height: role.height },
                &role.value,
            );
            node.roles.insert(role.role.clone());
            node.attributes.insert("value".into(), role.value.clone());
            node.attributes.insert("provenance".into(), role.provenance.clone());
            if let Some(state) = role.state {
                node.attributes.insert("state".into(), format!("{state:?}").to_lowercase());
            }
            if let Some(kind) = role.kind {
                node.attributes.insert("kind".into(), format!("{kind:?}").to_lowercase());
            }
            if let Err(error) = tree.add_node(tree.root(), node) {
                self.inspect.error = Some(error.to_string());
            }
        }
        self.inspect.observation = observation.clone();
        let next = observation.state;
        if next != Some(ProgramState::Idle) || self.state != Some(ProgramState::Working) || observation.visible {
            self.pending = None;
        } else {
            let pending = self.pending.get_or_insert((now, now, 1));
            if now >= pending.1 + 100 {
                pending.1 = now;
                pending.2 += 1;
            }
            if pending.2 < 3 && now < pending.0 + 700 {
                return None;
            }
            self.pending = None;
        }
        self.state = next;
        let status = next.map(|state| (state, observation.kind, observation.source.unwrap_or(ProgramStatusSource::Recognizer)));
        if status == self.last_status {
            return None;
        }
        self.last_status = status;
        next.map(|state| ProgramStatusRecord {
            id: String::new(),
            source: observation.source.unwrap_or(ProgramStatusSource::Recognizer),
            state,
            kind: observation.kind,
            progress: None,
            app: self.inspect.id.clone(),
            msg: String::new(),
            title: String::new(),
            since: now,
        })
    }
}

/// Convert the shared render contract without importing a VT backend.
pub fn screen_grid(update: &crate::provider::TerminalRenderUpdate, generation: u64) -> terminal_screen::ScreenGrid {
    use terminal_screen::*;

    use crate::provider::{TerminalCellFlags as Flags, TerminalCellWidth, TerminalCursorStyle};
    let mut cells = vec![Cell::default(); usize::from(update.cols) * usize::from(update.rows)];
    let mut metadata = vec![RowMetadata::default(); usize::from(update.rows)];
    for row in update.ops.iter().flat_map(|op| &op.rows) {
        metadata[usize::from(row.row)] = RowMetadata {
            soft_wrap: row.wrap,
            wrap_continuation: row.wrap_continuation,
            semantic_prompt: match row.semantic_prompt {
                1 => SemanticPrompt::Prompt,
                2 => SemanticPrompt::Continuation,
                _ => SemanticPrompt::None,
            },
        };
        for (col, cell) in row.cells.iter().enumerate() {
            let style = &cell.style;
            let flags = style.flags;
            cells[usize::from(row.row) * usize::from(update.cols) + col] = Cell {
                grapheme: cell.graphemes.iter().filter_map(|cp| char::from_u32(*cp)).collect(),
                width: match style.width {
                    TerminalCellWidth::Narrow => CellWidth::Narrow,
                    TerminalCellWidth::Wide => CellWidth::Wide,
                    TerminalCellWidth::SpacerTail => CellWidth::SpacerTail,
                    TerminalCellWidth::SpacerHead => CellWidth::SpacerHead,
                },
                semantic: match style.semantic {
                    1 => SemanticContent::Input,
                    2 => SemanticContent::Prompt,
                    _ => SemanticContent::Output,
                },
                style: Style {
                    fg: Rgb(style.resolved_fg.r, style.resolved_fg.g, style.resolved_fg.b),
                    bg: Rgb(style.resolved_bg.r, style.resolved_bg.g, style.resolved_bg.b),
                    bold: flags.contains(Flags::BOLD),
                    faint: flags.contains(Flags::FAINT),
                    inverse: flags.contains(Flags::INVERSE),
                    invisible: flags.contains(Flags::INVISIBLE),
                    italic: flags.contains(Flags::ITALIC),
                    blink: flags.contains(Flags::BLINK),
                    strikethrough: flags.contains(Flags::STRIKETHROUGH),
                    overline: flags.contains(Flags::OVERLINE),
                    underline_style: style.underline_style,
                    protected: style.protected,
                    hyperlink: (!style.hyperlink_uri.is_empty()).then(|| String::from_utf8_lossy(&style.hyperlink_uri).into_owned()),
                    ..Style::default()
                },
            };
        }
    }
    ScreenGrid::new(
        update.cols,
        update.rows,
        cells,
        metadata,
        Cursor {
            col: update.cursor.col,
            row: update.cursor.row,
            visible: update.cursor.visible,
            blinking: update.cursor.blink,
            wide_tail: update.cursor.wide_tail,
            style: match update.cursor.style {
                TerminalCursorStyle::Bar => CursorStyle::Bar,
                TerminalCursorStyle::Block => CursorStyle::Block,
                TerminalCursorStyle::Underline => CursorStyle::Underline,
                TerminalCursorStyle::BlockHollow => CursorStyle::Hollow,
            },
            // Current cleat render snapshots do not expose password-input state.
            password_input: false,
        },
        generation,
    )
    .expect("VT snapshot has rectangular geometry")
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fixture {
    pub marker: Vec<MarkerExpectation>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarkerExpectation {
    pub name: String,
    /// Exact role/value list, in rule and document order. Empty means absent.
    #[serde(default)]
    pub role: Vec<ExpectedRole>,
    pub state: Option<ProgramState>,
    pub source: Option<ProgramStatusSource>,
}
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExpectedRole {
    pub role: String,
    pub value: String,
}

/// Library equivalent of replay: use the real VT parser with no pacing or side effects.
/// Expectations next to the cast are checked at named asciicast marker events.
pub fn check_fixture(
    cast: &Path,
    expectations: &Path,
    engine: &mut dyn crate::vt::VtEngine,
    recognizer: Box<dyn Recognizer>,
) -> Result<(), String> {
    use crate::asciicast::{decode_event, decode_header, EventCode};
    let data = std::fs::read_to_string(cast).map_err(|e| e.to_string())?;
    let mut lines = data.lines();
    let header = decode_header(lines.next().ok_or("empty cast")?)?;
    engine.resize(header.cols, header.rows)?;
    let source = std::fs::read_to_string(expectations).map_err(|e| e.to_string())?;
    let fixture: Fixture = toml::from_str(&source).map_err(|e| format!("{}: {e}", expectations.display()))?;
    let mut pending = BTreeMap::new();
    for marker in fixture.marker {
        let name = marker.name.clone();
        if pending.insert(name.clone(), marker).is_some() {
            return Err(format!("duplicate expectation {name}"));
        }
    }
    let mut recognizer = Engine::new(recognizer);
    let mut time = std::time::Duration::ZERO;
    let mut generation = 0;
    let mut tree = None;
    for line in lines.filter(|line| !line.trim().is_empty()) {
        let event = decode_event(line, &mut time)?;
        match event.code {
            EventCode::Output => {
                engine.feed(event.data.as_bytes())?;
                if engine.screen_activity_changed()?.unwrap_or(false) {
                    generation += 1;
                    tree = None;
                }
            }
            EventCode::Resize => {
                let (cols, rows) = event.data.split_once('x').ok_or("invalid cast resize")?;
                engine.resize(cols.parse().map_err(|_| "invalid cast columns")?, rows.parse().map_err(|_| "invalid cast rows")?)?;
                generation += 1;
                tree = None;
            }
            _ => {}
        }
        if tree.is_none() {
            tree = Some(engine.recognition_tree(generation)?);
        }
        let mut annotated = tree.as_ref().unwrap().clone();
        recognizer.evaluate(&mut annotated, engine.title().as_deref(), event.time.as_millis() as u64);
        if let Some(error) = &recognizer.inspect.error {
            return Err(format!("{}: {error}", cast.display()));
        }
        if event.code == EventCode::Marker {
            if let Some(expected) = pending.remove(&event.data) {
                let actual = recognizer
                    .inspect
                    .observation
                    .roles
                    .iter()
                    .map(|r| ExpectedRole { role: r.role.clone(), value: r.value.clone() })
                    .collect::<Vec<_>>();
                if actual != expected.role
                    || expected.state.is_some_and(|state| Some(state) != recognizer.state)
                    || expected.source.is_some_and(|source| Some(source) != recognizer.inspect.observation.source)
                {
                    return Err(format!(
                        "{} marker {}: expected {expected:?}, got {actual:?}, state {:?}, source {:?}",
                        cast.display(),
                        event.data,
                        recognizer.state,
                        recognizer.inspect.observation.source
                    ));
                }
            }
        }
    }
    if !pending.is_empty() {
        return Err(format!("unseen fixture markers: {:?}", pending.keys().collect::<Vec<_>>()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use terminal_screen::Rect;

    use super::*;
    fn rules(body: &str) -> Rules {
        Rules::parse(Path::new("test.toml"), &format!("id='test'\nrevision=1\nmin_engine='0.1.0'\nverified=['test ^1']\n{body}")).unwrap()
    }
    fn tree(generation: u64) -> ScreenTree {
        ScreenTree::from_root(generation, Node::new("screen", Rect { col: 0, row: 0, width: 80, height: 24 }, ""))
    }
    fn line(tree: &mut ScreenTree, text: &str, source: &str) {
        let mut node = Node::new("row", Rect { col: 0, row: 0, width: 80, height: 1 }, text);
        node.attributes.insert("provenance".into(), source.into());
        tree.add_node(tree.root(), node).unwrap();
    }
    // Whole-file validation rejects malformed declarations before any rule is available.
    // The generated cases cross syntax, version, selector, schema, template and state boundaries.
    #[test]
    fn invalid_files_are_atomic_and_located() {
        let valid = "id='test'\nrevision=1\nmin_engine='0.1.0'\nverified=[]\n";
        for body in [
            "[[rule]]\nselect='['\nrole='composer'",
            "[[rule]]\nselect='row'\nkind='auth'",
            "[[rule]]\nselect='row'\nrole='bad role'",
            "[[rule]]\nselect='row'\nrole='composer'\nvalue='{unclosed'",
            "[[rule]]\nselect='row'\nrole='composer'\nunknown=true",
        ] {
            let error = Rules::parse(Path::new("broken.toml"), &format!("{valid}{body}")).err().unwrap();
            assert!(error.contains("broken.toml:") && error.contains("rule"), "{error}");
        }
        for source in [
            valid.replace("0.1.0", "99.0.0"),
            valid.replace("verified=[]", "verified=['missing-range']"),
            valid.replace("id='test'", "id='../escape'"),
        ] {
            assert!(Rules::parse(Path::new("broken.toml"), &source).is_err());
        }
    }
    // Named captures become values; declared regions win and unmatched roles remain absent.
    #[test]
    fn captures_roles_provenance_and_explain_share_the_tree() {
        let rules = rules("[[rule]]\nselect='row:matches(/Draft: (?P<draft>.*)/)'\nrole='composer'\nvalue='{draft}'\n[[rule]]\nselect='row:has-text(\"Absent\")'\nrole='dialog'");
        let mut engine = Engine::new(Box::new(rules));
        let mut screen = tree(1);
        line(&mut screen, "Draft: inferred", "graph");
        line(&mut screen, "Draft: chosen", "declared");
        engine.evaluate(&mut screen, None, 0);
        assert_eq!(screen.select("composer").unwrap().iter().map(|n| n.attribute("value").unwrap()).collect::<Vec<_>>(), ["chosen"]);
        assert!(screen.select("dialog").unwrap().is_empty());
        let explain = &engine.inspect.observation.explanations;
        assert_eq!(explain[0].captures[0]["draft"], "chosen");
        assert_eq!(explain[0].provenance, ["declared"]);
        assert!(!explain[1].matched);
    }
    // Only a changed screen generation or title invokes recognizers; timer confirmations reuse observations.
    #[test]
    fn generation_cache_and_title_debounce() {
        let rules = rules(
            "[[rule]]\nselect='title:has-text(\"Working\")'\nstate='working'\n[[rule]]\nselect='title:has-text(\"Ready\")'\nstate='idle'",
        );
        let mut engine = Engine::new(Box::new(rules));
        assert_eq!(engine.evaluate(&mut tree(1), Some("Working"), 0).unwrap().source, ProgramStatusSource::Title);
        assert!(engine.evaluate(&mut tree(1), Some("Ready"), 10).is_none());
        assert!(!engine.needs_update(1, Some("Ready"), 109));
        assert!(engine.evaluate(&mut tree(1), Some("Ready"), 109).is_none());
        assert!(engine.evaluate(&mut tree(1), Some("Ready"), 110).is_none());
        assert!(engine.evaluate(&mut tree(1), Some("Ready"), 209).is_none());
        assert_eq!(engine.evaluate(&mut tree(1), Some("Ready"), 210).unwrap().state, ProgramState::Idle);
        assert!(!engine.needs_update(1, Some("Ready"), 1000));
        assert!(engine.needs_update(2, Some("Ready"), 1000));
    }
    // A blocked or visibly idle screen bypasses title debounce; interrupted idle cancels pending work.
    #[test]
    fn visible_states_and_interrupted_idle() {
        for state in ["idle", "blocked"] {
            let mut engine = Engine::new(Box::new(rules(&format!(
                "[[rule]]\nselect='title'\nstate='working'\n[[rule]]\nselect='row'\nstate='{state}'"
            ))));
            engine.evaluate(&mut tree(1), Some("busy"), 0);
            let mut screen = tree(2);
            line(&mut screen, "Visible", "graph");
            assert!(engine.evaluate(&mut screen, Some("busy"), 1).is_some());
            assert!(engine.deadline().is_none());
        }
        let mut engine = Engine::new(Box::new(rules(
            "[[rule]]\nselect='title:has-text(\"busy\")'\nstate='working'\n[[rule]]\nselect='title:has-text(\"idle\")'\nstate='idle'",
        )));
        engine.evaluate(&mut tree(0), Some("busy"), 0);
        engine.evaluate(&mut tree(0), Some("idle"), 1);
        engine.evaluate(&mut tree(0), Some("busy"), 50);
        assert!(engine.deadline().is_none());
        engine.evaluate(&mut tree(0), Some("idle"), 100);
        assert_eq!(engine.evaluate(&mut tree(0), Some("idle"), 800).unwrap().state, ProgramState::Idle);
    }
    struct Fault {
        panic: bool,
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Recognizer for Fault {
        fn id(&self) -> &str {
            "fault"
        }
        fn revision(&self) -> u64 {
            1
        }
        fn evaluate(&mut self, _: &ScreenTree, _: Option<&str>) -> Result<Observation, String> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.panic {
                panic!("fixture fault");
            }
            Err("fixture error".into())
        }
    }
    // Both Rust escape-hatch errors and unwinding panics become inspect errors, once per generation.
    #[test]
    fn faults_are_isolated_and_cached() {
        for panic in [false, true] {
            let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let mut engine = Engine::new(Box::new(Fault { panic, calls: calls.clone() }));
            for _ in 0..3 {
                assert!(engine.evaluate(&mut tree(0), None, 0).is_none());
            }
            assert!(engine.inspect.error.is_some());
            assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
            engine.evaluate(&mut tree(1), None, 0);
            assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        }
    }
    // Replay uses the real VT parser, checking presence and absence at recording markers.
    #[cfg(feature = "ghostty-vt")]
    #[test]
    fn synthetic_cast_fixture() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../recognizers");
        let mut vt = crate::vt::ghostty::GhosttyVtEngine::new(40, 5);
        check_fixture(
            &root.join("fixtures/synthetic/basic.cast"),
            &root.join("fixtures/synthetic/basic.toml"),
            &mut vt,
            Box::new(Rules::load(&root.join("synthetic.toml")).unwrap()),
        )
        .unwrap();
        // Each generated invalid expectation must fail through the public harness.
        let source = std::fs::read_to_string(root.join("fixtures/synthetic/basic.toml")).unwrap();
        let temp = tempfile::tempdir().unwrap();
        for invalid in [
            source.replace("hello", "wrong"),
            source.replace("name = \"draft\"", "name = \"unseen\""),
            format!("{source}\n[[marker]]\nname='draft'\n"),
        ] {
            let path = temp.path().join("expectations.toml");
            std::fs::write(&path, invalid).unwrap();
            let mut vt = crate::vt::ghostty::GhosttyVtEngine::new(40, 5);
            assert!(check_fixture(
                &root.join("fixtures/synthetic/basic.cast"),
                &path,
                &mut vt,
                Box::new(Rules::load(&root.join("synthetic.toml")).unwrap())
            )
            .is_err());
        }
    }
}

// Release acceptance uses #316's adversarial 200x50 plus screen. Timing remains
// opt-in so shared debug CI does not become a wall-clock gate.
#[cfg(test)]
#[test]
#[ignore = "run with --release --ignored --nocapture"]
fn recognizer_adversarial_plus_budget() {
    use terminal_screen::{Cell, Cursor, RowMetadata, ScreenGrid};
    if cfg!(debug_assertions) {
        panic!("release measurement required");
    }
    let grid = ScreenGrid::new(
        200,
        50,
        vec![Cell { grapheme: "+".into(), ..Cell::default() }; 10_000],
        vec![RowMetadata::default(); 50],
        Cursor::default(),
        0,
    )
    .unwrap();
    let base = terminal_screen::analyze(&grid);
    let mut engine = Engine::new(Box::new(Rules::parse(Path::new("budget.toml"), "id='budget'\nrevision=1\nmin_engine='0.1.0'\nverified=[]\n[[rule]]\nselect='row:matches(/(?P<draft>[+]+)/)'\nrole='composer'\nvalue='{draft}'").unwrap()));
    let start = std::time::Instant::now();
    for generation in 0..100 {
        let mut tree = base.clone();
        tree.generation = generation;
        engine.evaluate(std::hint::black_box(&mut tree), None, generation);
    }
    let per_frame = start.elapsed().as_secs_f64() / 100.;
    eprintln!("200x50 '+' recognition (including frame clone): {:.3}ms/frame", per_frame * 1000.);
    assert!(per_frame < 0.005, "{per_frame}s exceeds 5ms");
}
