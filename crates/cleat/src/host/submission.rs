//! Controller activity, conservative echo evidence, and bounded replay admission.
//! These types are independent of a VT engine; the actor owns their PTY transaction.
use std::time::{Duration, Instant};

/// Predicates evaluated by the actor before any marker or PTY write.
#[derive(Default)]
pub(crate) struct SendPreconditions {
    pub(crate) controller_idle: Option<Duration>,
    pub(crate) selector: Option<terminal_screen::Selector>,
}

impl SendPreconditions {
    pub(super) fn guarded(&self) -> bool {
        self.controller_idle.is_some() || self.selector.is_some()
    }

    pub(super) fn evaluate(&self, controller: &ControllerActivity, now: Instant) -> Result<(), String> {
        if self.controller_idle.is_some_and(|idle| controller.at.is_some_and(|at| now.saturating_duration_since(at) < idle)) {
            return Err("controller input is not idle".into());
        }
        Ok(())
    }
}

// Preserve the existing composer delay while the actor holds submission ownership.
pub(super) const SUBMIT_ENTER_DELAY: Duration = Duration::from_millis(100);
pub(super) const CONTROLLER_QUEUE_BYTES: usize = 64 * 1024;
pub(super) const CONTROLLER_QUEUE_EVENTS: usize = 256;

#[derive(Default)]
pub(super) struct ControllerActivity {
    pub(super) generation: u64,
    pub(super) at: Option<Instant>,
    pub(super) unix_ms: Option<u64>,
    /// PTY output is only an observation fence, not an application acknowledgement.
    pub(super) pending_output: bool,
    expected_echo: Option<Vec<u8>>,
    echo_prefix: Vec<usize>,
    echo_matched: usize,
    pub(super) observed_output_at: Option<Instant>,
}

impl ControllerActivity {
    pub(super) fn from_history(history: crate::protocol::ControllerInputHistory) -> Self {
        let now = Instant::now();
        let unix_now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
        let mut activity = Self {
            generation: history.generation,
            unix_ms: history.last_input_at,
            at: history.last_input_at.and_then(|at| now.checked_sub(Duration::from_millis(unix_now.saturating_sub(at)))),
            observed_output_at: (history.generation > 0).then_some(now),
            ..Default::default()
        };
        if history.pending_output {
            activity.written(history.expected_echo);
            activity.echo_matched =
                activity.expected_echo.as_ref().map_or(0, |expected| history.echo_matched.min(expected.len().saturating_sub(1)));
        }
        activity
    }

    #[cfg(any(unix, test))]
    pub(super) fn history(&self) -> crate::protocol::ControllerInputHistory {
        crate::protocol::ControllerInputHistory {
            generation: self.generation,
            last_input_at: self.unix_ms,
            pending_output: self.pending_output,
            expected_echo: self.expected_echo.clone(),
            echo_matched: self.echo_matched,
        }
    }

    pub(super) fn accepted(&mut self, now: Instant) {
        self.generation = self.generation.saturating_add(1);
        self.at = Some(now);
        self.unix_ms = Some(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64);
    }

    pub(super) fn written(&mut self, expected: Option<Vec<u8>>) {
        if !self.pending_output {
            self.expected_echo = Some(Vec::new());
            self.echo_matched = 0;
        }
        self.pending_output = true;
        self.expected_echo = match (self.expected_echo.take(), expected) {
            (Some(mut pending), Some(bytes)) if !bytes.is_empty() && pending.len() + bytes.len() <= CONTROLLER_QUEUE_BYTES => {
                pending.extend(bytes);
                Some(pending)
            }
            _ => None, // No verifiable echo, or too much input: guarded sends remain fail-closed.
        };
        self.echo_prefix.clear();
        if let Some(expected) = &self.expected_echo {
            self.echo_prefix.resize(expected.len(), 0);
            for index in 1..expected.len() {
                let mut prefix = self.echo_prefix[index - 1];
                while prefix > 0 && expected[index] != expected[prefix] {
                    prefix = self.echo_prefix[prefix - 1];
                }
                if expected[index] == expected[prefix] {
                    prefix += 1;
                }
                self.echo_prefix[index] = prefix;
            }
        }
    }

    pub(super) fn observe_output(&mut self, bytes: &[u8], now: Instant) {
        if !self.pending_output {
            return;
        }
        let Some(expected) = &self.expected_echo else {
            return;
        };
        // Streaming substring matching recognizes fragmented echo in linear time and bounded space.
        for byte in bytes {
            while self.echo_matched > 0 && expected[self.echo_matched] != *byte {
                self.echo_matched = self.echo_prefix[self.echo_matched - 1];
            }
            if expected[self.echo_matched] == *byte {
                self.echo_matched += 1;
            }
            if self.echo_matched == expected.len() {
                self.pending_output = false;
                self.expected_echo = None;
                self.echo_prefix.clear();
                self.echo_matched = 0;
                self.observed_output_at = Some(now);
                return;
            }
        }
    }

    pub(super) fn check(&self, preconditions: &SendPreconditions, now: Instant) -> Result<(), String> {
        if preconditions.guarded() {
            // This fence precedes all predicates, including future screen selectors.
            if self.pending_output {
                return Err("controller input has not yet produced a verifiable PTY echo".into());
            }
            if self.generation > 0 && self.observed_output_at.is_some_and(|at| now.saturating_duration_since(at) < SUBMIT_ENTER_DELAY) {
                return Err("controller input output fence is still settling".into());
            }
        }
        preconditions.evaluate(self, now)
    }
}

pub(super) enum QueuedControllerInput {
    Event { source: u128, event: crate::provider::TerminalInputEvent },
    Release(u128),
    Retain(Vec<u128>),
    Focus(bool),
}

impl QueuedControllerInput {
    pub(super) fn bytes(&self) -> usize {
        match self {
            Self::Event { event, .. } => controller_event_size(event),
            Self::Focus(_) => 1,
            Self::Release(_) => std::mem::size_of::<u128>(),
            Self::Retain(sources) => sources.len().saturating_mul(std::mem::size_of::<u128>()),
        }
    }
}

#[derive(Default)]
pub(super) struct ControllerReplayQueue {
    events: std::collections::VecDeque<QueuedControllerInput>,
    bytes: usize,
}

impl ControllerReplayQueue {
    pub(super) fn push(&mut self, input: QueuedControllerInput) -> Result<(), String> {
        let bytes = input.bytes();
        if self.events.len() >= CONTROLLER_QUEUE_EVENTS || bytes > CONTROLLER_QUEUE_BYTES.saturating_sub(self.bytes) {
            return Err("submission busy: controller input replay queue is full (input refused)".into());
        }
        self.bytes += bytes;
        self.events.push_back(input);
        Ok(())
    }

    pub(super) fn pop(&mut self) -> Option<QueuedControllerInput> {
        let input = self.events.pop_front()?;
        self.bytes -= input.bytes();
        Some(input)
    }
}

pub(super) fn controller_event_size(event: &crate::provider::TerminalInputEvent) -> usize {
    use crate::provider::TerminalInputEvent as E;
    match event {
        E::Text(event) => event.text.len(),
        E::Paste(event) => event.text.len(),
        E::RawBytes(bytes) => bytes.len(),
        E::Key(event) => {
            std::mem::size_of_val(event)
                + event.generated_text.as_ref().map_or(0, String::len)
                + event.physical_key.as_ref().map_or(0, String::len)
                + match &event.key {
                    crate::provider::TerminalKey::Code(code) => code.len(),
                    _ => 0,
                }
        }
        // Only keyboard/text/paste/raw events enter the replay queue. Mouse/wheel PTY
        // input is refused while busy; other events use separate maintenance variants.
        _ => 0,
    }
}

pub(super) fn controller_expected_echo(event: &crate::provider::TerminalInputEvent) -> Option<Vec<u8>> {
    use crate::provider::{TerminalInputEvent as E, TerminalKey};
    match event {
        E::Text(event) => Some(event.text.as_bytes().to_vec()),
        E::Paste(event) => Some(event.text.as_bytes().to_vec()),
        E::RawBytes(bytes) => Some(bytes.clone()),
        E::Key(event) => {
            event.generated_text.as_ref().filter(|text| !text.is_empty()).map(|text| text.as_bytes().to_vec()).or_else(|| match event.key {
                TerminalKey::UnicodeScalar(codepoint) if event.modifiers.is_empty() => {
                    char::from_u32(codepoint).map(|ch| ch.to_string().into_bytes())
                }
                _ => None,
            })
        }
        _ => None,
    }
}

pub(super) fn controller_event_is_activity(event: &crate::provider::TerminalInputEvent) -> bool {
    use crate::provider::{TerminalInputEvent as E, TerminalKeyAction};
    match event {
        E::Text(event) => !event.text.is_empty(),
        E::Paste(event) => !event.text.is_empty(),
        E::RawBytes(bytes) => !bytes.is_empty(),
        E::Key(event) => event.action != TerminalKeyAction::Release,
        _ => false,
    }
}

#[cfg(test)]
mod controller_fence_properties {
    use super::*;

    // Echo evidence must survive every read fragmentation and a live host transfer.
    // The generator covers repetitive prefixes, multibyte text and all chunk sizes, plus unrelated output.
    #[test]
    fn fragmented_echo_and_transferred_history() {
        for expected in [b"x".as_slice(), b"aaaaab", b"ababa", "λ draft".as_bytes()] {
            for chunk_size in 1..=expected.len() {
                let now = Instant::now();
                let mut activity = ControllerActivity::default();
                activity.accepted(now);
                activity.written(Some(expected.to_vec()));
                activity.observe_output(b"unrelated output", now);
                assert!(activity.pending_output);
                for (index, chunk) in expected.chunks(chunk_size).enumerate() {
                    // Serialize the real handoff contract between each read, preserving matching progress.
                    let history = serde_json::from_str(&serde_json::to_string(&activity.history()).unwrap()).unwrap();
                    activity = ControllerActivity::from_history(history);
                    activity.observe_output(chunk, now);
                    assert_eq!(activity.pending_output, (index + 1) * chunk_size < expected.len());
                    assert_eq!(activity.generation, 1);
                }
                let guard = SendPreconditions { controller_idle: Some(Duration::ZERO), ..Default::default() };
                assert!(activity.check(&guard, now).is_err()); // matched echo still needs its settling interval
                assert!(activity.check(&guard, now + SUBMIT_ENTER_DELAY).is_ok());
                activity.accepted(now);
                assert_eq!(activity.generation, 2);
            }
        }
    }

    // Unknown or oversized echo remains fail-closed; later unrelated output or input must not authorize a send.
    #[test]
    fn unverifiable_echo_stays_closed() {
        for expected in [None, Some(vec![b'x'; CONTROLLER_QUEUE_BYTES + 1])] {
            let now = Instant::now();
            let mut activity = ControllerActivity::default();
            activity.accepted(now);
            activity.written(expected);
            activity.observe_output(b"anything", now);
            activity.written(Some(b"known".to_vec()));
            activity.observe_output(b"known", now);
            assert!(activity
                .check(&SendPreconditions { controller_idle: Some(Duration::ZERO), ..Default::default() }, now + Duration::from_secs(100))
                .is_err());
        }
    }

    // Generated sequences conserve accepted input and keep both queue bounds inclusive.
    // Operations span empty, one byte, max-1, max, max+1, dequeue, duplication and event-count saturation.
    #[test]
    fn replay_queue_bounds_and_conservation() {
        let mut queue = ControllerReplayQueue::default();
        let mut model = std::collections::VecDeque::new();
        for step in 0..400 {
            if step % 3 == 0 {
                let actual = queue.pop().map(|input| input.bytes());
                assert_eq!(actual, model.pop_front());
            } else {
                let len = [0, 1, CONTROLLER_QUEUE_BYTES - 1, CONTROLLER_QUEUE_BYTES, CONTROLLER_QUEUE_BYTES + 1][step % 5];
                let should_accept = model.len() < CONTROLLER_QUEUE_EVENTS && model.iter().sum::<usize>() + len <= CONTROLLER_QUEUE_BYTES;
                let result = queue.push(QueuedControllerInput::Event {
                    source: 0,
                    event: crate::provider::TerminalInputEvent::RawBytes(vec![b'x'; len]),
                });
                assert_eq!(result.is_ok(), should_accept);
                if should_accept {
                    model.push_back(len);
                }
            }
            assert_eq!(queue.bytes, model.iter().sum::<usize>());
            assert_eq!(queue.events.len(), model.len());
        }
        while queue.pop().is_some() {}
        assert_eq!(queue.bytes, 0);
        for _ in 0..CONTROLLER_QUEUE_EVENTS {
            queue.push(QueuedControllerInput::Event { source: 0, event: crate::provider::TerminalInputEvent::RawBytes(vec![]) }).unwrap();
        }
        assert!(queue.push(QueuedControllerInput::Release(0)).unwrap_err().contains("refused"));
    }
}

/// Stable send failure contract shared by the actor, HTTP transport and CLI.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct SendError {
    #[serde(skip)]
    pub(crate) status: u16,
    pub(crate) reason: String,
    pub(crate) error: String,
}
impl SendError {
    pub(crate) fn new(status: u16, reason: &str, error: impl Into<String>) -> Self {
        Self { status, reason: reason.into(), error: error.into() }
    }
    pub(crate) fn refused(reason: &str, error: impl Into<String>) -> Self {
        Self::new(409, reason, error)
    }
    #[cfg(test)]
    pub(crate) fn contains(&self, text: &str) -> bool {
        self.error.contains(text)
    }
}
impl From<String> for SendError {
    fn from(error: String) -> Self {
        Self::new(500, "error", error)
    }
}
impl From<&str> for SendError {
    fn from(error: &str) -> Self {
        error.to_string().into()
    }
}
impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.reason, self.error)
    }
}

/// Structure needs no recognizer; all other names require a declared role source.
pub(crate) fn selector_roles(selector: &terminal_screen::Selector) -> std::collections::BTreeSet<String> {
    selector
        .referenced_elements()
        .into_iter()
        .filter(|name| {
            !matches!(
                name.as_str(),
                "screen" | "region" | "row" | "span" | "cursor" | "frame" | "enclosure" | "zone" | "group" | "block" | "gap" | "row-slice"
            )
        })
        .collect()
}

pub(crate) fn evaluate_selector(selector: &terminal_screen::Selector, tree: &terminal_screen::ScreenTree) -> Result<(), SendError> {
    if !selector.evaluate(tree).is_empty() {
        return Ok(());
    }
    let absent = selector_roles(selector).iter().any(|role| {
        !tree.document_order().iter().filter_map(|id| tree.node(*id)).any(|node| node.element == *role || node.roles.contains(role))
    });
    Err(if absent {
        SendError::refused("role-absent", "selector role is absent from the current frame")
    } else {
        SendError::refused("not-matched", "selector did not match the current frame")
    })
}

#[cfg(test)]
mod selector_tests {
    use terminal_screen::{Node, Rect, ScreenTree, Selector};

    use super::*;

    // Syntax capability discovery includes nested predicates, unions and nth-child filters,
    // but never mistakes attribute values or regex text for role names.
    #[test]
    fn selector_capabilities_are_static_and_recursive() {
        for (source, expected) in [
            ("row:matches(/composer/)", vec![]),
            ("screen:has(composer[value='dialog'])", vec!["composer"]),
            ("row:not(dialog), span:nth-child(1 of composer)", vec!["composer", "dialog"]),
            ("frame > row-slice", vec![]),
        ] {
            assert_eq!(selector_roles(&Selector::parse(source).unwrap()).into_iter().collect::<Vec<_>>(), expected);
        }
    }

    // Unknown observations are absent; exact value equality preserves literal spaces and suffixes.
    // Generate absent, empty, whitespace, multiline, Unicode and suffix-bearing draft values.
    #[test]
    fn selector_refusal_preserves_observation() {
        let selector = Selector::parse("composer[value='']").unwrap();
        for value in [None, Some(""), Some(" "), Some("draft"), Some("\n"), Some("λ suffix")] {
            let mut tree = ScreenTree::from_root(0, Node::new("screen", Rect { col: 0, row: 0, width: 80, height: 24 }, ""));
            if let Some(value) = value {
                let mut node = Node::new("composer", Rect { col: 0, row: 0, width: 80, height: 1 }, value);
                node.attributes.insert("value".into(), value.into());
                tree.add_node(tree.root(), node).unwrap();
            }
            let before = tree.clone();
            let result = evaluate_selector(&selector, &tree);
            assert_eq!(result.is_ok(), value == Some(""));
            if let Err(error) = result {
                assert_eq!(error.status, 409);
                assert_eq!(error.reason, if value.is_none() { "role-absent" } else { "not-matched" });
            }
            assert_eq!(tree, before);
        }
    }

    // A selector alone activates the same conservative controller fence as a quiet-time guard.
    // Delayed/fragmented echo and the settling boundary must refuse, however old the input is.
    #[test]
    fn selector_guard_fences_controller_echo_without_idle_option() {
        let guard = SendPreconditions { selector: Some(Selector::parse("screen").unwrap()), ..Default::default() };
        let now = Instant::now();
        let mut controller = ControllerActivity::default();
        controller.accepted(now);
        controller.written(Some(b"draft".to_vec()));
        assert!(controller.check(&guard, now + Duration::from_secs(3600)).is_err());
        controller.observe_output(b"dra", now);
        assert!(controller.check(&guard, now + Duration::from_secs(3600)).is_err());
        controller.observe_output(b"ft", now);
        assert!(controller.check(&guard, now + SUBMIT_ENTER_DELAY - Duration::from_nanos(1)).is_err());
        assert!(controller.check(&guard, now + SUBMIT_ENTER_DELAY).is_ok());
    }

    // Replay both real harnesses at both recorded sizes. Every fixture marker is an
    // independent empty/draft/absent oracle, including placeholders, spaces and cursor-left suffixes.
    #[cfg(feature = "ghostty-vt")]
    #[test]
    fn guarded_send_recorded_composers() {
        use crate::{
            asciicast::{decode_event, decode_header, EventCode},
            recognizer::{Engine, Fixture, Rules},
            vt::{ghostty::GhosttyVtEngine, VtEngine},
        };
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let selector = Selector::parse("composer[value='']").unwrap();
        for id in ["codex", "claude-code"] {
            for size in ["80x24", "120x40"] {
                let path = root.join(format!("recognizers/fixtures/{id}/{id}-{size}"));
                let data = std::fs::read_to_string(path.with_extension("cast")).unwrap();
                let fixture: Fixture = toml::from_str(&std::fs::read_to_string(path.with_extension("toml")).unwrap()).unwrap();
                let mut lines = data.lines();
                let header = decode_header(lines.next().unwrap()).unwrap();
                let mut vt = GhosttyVtEngine::new(header.cols, header.rows);
                let mut engine = Engine::new(Rules::load(&root.join(format!("recognizers/{id}.toml"))).unwrap().into_recognizer());
                let mut time = Duration::ZERO;
                for line in lines.filter(|line| !line.trim().is_empty()) {
                    let event = decode_event(line, &mut time).unwrap();
                    match event.code {
                        EventCode::Output => vt.feed(event.data.as_bytes()).unwrap(),
                        EventCode::Resize => {
                            let (cols, rows) = event.data.split_once('x').unwrap();
                            vt.resize(cols.parse().unwrap(), rows.parse().unwrap()).unwrap();
                        }
                        _ => {}
                    }
                    if event.code != EventCode::Marker {
                        continue;
                    }
                    let mut tree = vt.recognition_tree(event.time.as_millis() as u64).unwrap();
                    engine.evaluate(&mut tree, vt.title().as_deref(), event.time.as_millis() as u64);
                    assert!(engine.inspect.error.is_none(), "{id} {size} {}: {:?}", event.data, engine.inspect.error);
                    if event.code == EventCode::Marker {
                        let expected = fixture.marker.iter().find(|m| m.name == event.data).unwrap();
                        let composer = expected.role.iter().find(|r| r.role == "composer");
                        let result = evaluate_selector(&selector, &tree);
                        assert_eq!(result.is_ok(), composer.is_some_and(|r| r.value.is_empty()), "{id} {size} {}", event.data);
                        if let Err(error) = result {
                            assert_eq!(error.reason, if composer.is_none() { "role-absent" } else { "not-matched" });
                        }
                        // Known observation limit in both harnesses: spaces after a cursor
                        // at the start are indistinguishable from terminal padding.
                        if event.data == "draft-space" {
                            let cursor = tree.select("cursor").unwrap()[0].bounds;
                            vt.feed(format!("\x1b7\x1b[{};3H", cursor.row + 1).as_bytes()).unwrap();
                            let mut left = vt.recognition_tree(event.time.as_millis() as u64 + 1).unwrap();
                            engine.evaluate(&mut left, vt.title().as_deref(), event.time.as_millis() as u64 + 1);
                            assert!(evaluate_selector(&selector, &left).is_ok(), "{id} {size} space-only cursor-left limit");
                            vt.feed(b"\x1b8").unwrap();
                        }
                    }
                }
            }
        }
    }
}
