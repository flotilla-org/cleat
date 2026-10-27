//! Controller activity, conservative echo evidence, and bounded replay admission.
//! These types are independent of a VT engine; the actor owns their PTY transaction.
use std::time::{Duration, Instant};

use crate::protocol::{
    ControllerFence, ControllerFenceCause, ControllerFenceClearance, ControllerFenceClearanceCause, ControllerFenceState,
};

fn unix_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

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

    /// A selector over recognizer roles, checked after a quiet interval, is screen
    /// evidence that can replace a pending or unverifiable echo fence (#393).
    /// Structural selectors say nothing about the application's input state.
    fn selector_evidence(&self) -> bool {
        self.controller_idle.is_some() && self.selector.as_ref().is_some_and(|selector| !selector_roles(selector).is_empty())
    }

    pub(super) fn evaluate(&self, controller: &ControllerActivity, now: Instant) -> Result<(), String> {
        if self.controller_idle.is_some_and(|idle| controller.at.is_some_and(|at| now.saturating_duration_since(at) < idle)) {
            return Err("controller input is not idle".into());
        }
        Ok(())
    }
}

/// A guarded write, in transaction order: the echo fence, quiet time, the
/// selector on the current frame, then `write`. A matched recognizer-role
/// selector replaces a closed echo fence (#393). As evidence it must also find
/// every role it references on the frame, so a negation cannot match an absent
/// composer. The fence is recorded as cleared only after `write` succeeds.
pub(super) fn guarded_write<C, T>(
    context: &mut C,
    controller: &mut ControllerActivity,
    preconditions: &SendPreconditions,
    now: Instant,
    check_selector: impl FnOnce(&mut C, &terminal_screen::Selector, bool) -> Result<(), SendError>,
    write: impl FnOnce(&mut C) -> Result<T, SendError>,
) -> Result<T, SendError> {
    let evidence = controller.check(preconditions, now)?;
    if let Some(selector) = &preconditions.selector {
        check_selector(context, selector, evidence == FenceEvidence::Selector)?;
    }
    let written = write(context)?;
    if evidence == FenceEvidence::Selector {
        controller.clear_by_selector();
    }
    Ok(written)
}

/// What [`guarded_write`] must still prove after [`ControllerActivity::check`] succeeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FenceEvidence {
    /// Echo (or no controller input) already cleared the fence.
    Echo,
    /// The fence is open only if the selector matches in this transaction;
    /// the actor then records the match with [`ControllerActivity::clear_by_selector`].
    Selector,
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
    fence_since: Option<u64>,
    observed_unix_ms: Option<u64>,
    fence_cause: Option<ControllerFenceCause>,
    cleared_by: Option<ControllerFenceClearance>,
}

impl ControllerActivity {
    pub(super) fn from_history(history: crate::protocol::ControllerInputHistory) -> Self {
        Self::from_history_at(history, Instant::now(), unix_ms())
    }

    fn from_history_at(history: crate::protocol::ControllerInputHistory, now: Instant, unix_now: u64) -> Self {
        let mut activity = Self {
            generation: history.generation,
            unix_ms: history.last_input_at,
            at: history.last_input_at.and_then(|at| now.checked_sub(Duration::from_millis(unix_now.saturating_sub(at)))),
            observed_output_at: history
                .observed_output_at
                .and_then(|at| now.checked_sub(Duration::from_millis(unix_now.saturating_sub(at))))
                .or_else(|| (history.generation > 0).then_some(now)),
            observed_unix_ms: history.observed_output_at.or_else(|| (history.generation > 0).then_some(unix_now)),
            fence_since: history.fence_since,
            fence_cause: history.fence_cause,
            cleared_by: history.fence_cleared_by,
            ..Default::default()
        };
        if history.pending_output {
            activity.written(history.expected_echo);
            activity.fence_since = history.fence_since.or(history.last_input_at);
            activity.fence_cause = history.fence_cause.or(activity.fence_cause);
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
            fence_since: self.fence_since,
            observed_output_at: self.observed_unix_ms,
            fence_cause: self.fence_cause,
            fence_cleared_by: self.cleared_by,
        }
    }

    pub(super) fn accepted(&mut self, now: Instant) {
        self.generation = self.generation.saturating_add(1);
        self.at = Some(now);
        self.unix_ms = Some(unix_ms());
    }

    pub(super) fn written(&mut self, expected: Option<Vec<u8>>) {
        if !self.pending_output {
            self.expected_echo = Some(Vec::new());
            self.echo_matched = 0;
            self.cleared_by = None;
        }
        let was_pending = self.pending_output;
        let was_unverifiable = was_pending && self.expected_echo.is_none();
        self.pending_output = true;
        let expected_is_oversized = match (&self.expected_echo, &expected) {
            (Some(pending), Some(bytes)) => !bytes.is_empty() && pending.len() + bytes.len() > CONTROLLER_QUEUE_BYTES,
            _ => false,
        };
        self.expected_echo = match (self.expected_echo.take(), expected) {
            (Some(mut pending), Some(bytes)) if !bytes.is_empty() && pending.len() + bytes.len() <= CONTROLLER_QUEUE_BYTES => {
                pending.extend(bytes);
                Some(pending)
            }
            _ => None, // No verifiable echo, or too much input: guarded sends remain fail-closed.
        };
        if !was_pending || (!was_unverifiable && self.expected_echo.is_none()) {
            self.fence_since = Some(unix_ms());
        }
        if !was_unverifiable && self.expected_echo.is_none() {
            self.fence_cause =
                Some(if expected_is_oversized { ControllerFenceCause::Oversized } else { ControllerFenceCause::NoKnownText });
        }
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
                self.observed_unix_ms = Some(unix_ms());
                self.fence_since = self.observed_unix_ms;
                self.fence_cause = None;
                return;
            }
        }
    }

    /// Record that a matched selector replaced echo evidence for all input so far.
    /// No settling interval follows: the screen already showed the input's effect.
    fn clear_by_selector(&mut self) {
        if !self.pending_output {
            return;
        }
        self.pending_output = false;
        self.expected_echo = None;
        self.echo_prefix.clear();
        self.echo_matched = 0;
        self.fence_cause = None;
        self.fence_since = Some(unix_ms());
        self.cleared_by = Some(ControllerFenceClearance { cause: ControllerFenceClearanceCause::Selector, generation: self.generation });
    }

    fn settling(&self, now: Instant) -> bool {
        self.cleared_by.is_none()
            && self.generation > 0
            && self.observed_output_at.is_some_and(|at| now.saturating_duration_since(at) < SUBMIT_ENTER_DELAY)
    }

    pub(super) fn fence(&self, now: Instant) -> ControllerFence {
        let (state, since, settles_at, cause) = if self.pending_output {
            (
                if self.expected_echo.is_some() { ControllerFenceState::Pending } else { ControllerFenceState::Unverifiable },
                self.fence_since,
                None,
                self.fence_cause,
            )
        } else if self.cleared_by.is_some() {
            (ControllerFenceState::Clear, self.fence_since, None, None)
        } else if self.settling(now) {
            (
                ControllerFenceState::Settling,
                self.observed_unix_ms,
                self.observed_unix_ms.map(|at| at.saturating_add(SUBMIT_ENTER_DELAY.as_millis() as u64)),
                None,
            )
        } else {
            (
                ControllerFenceState::Clear,
                self.observed_unix_ms.map(|at| at.saturating_add(SUBMIT_ENTER_DELAY.as_millis() as u64)),
                None,
                None,
            )
        };
        let cleared_by = self.cleared_by.filter(|_| state == ControllerFenceState::Clear);
        ControllerFence { state, since, settles_at, cause, cleared_by }
    }

    pub(super) fn check(&self, preconditions: &SendPreconditions, now: Instant) -> Result<FenceEvidence, SendError> {
        let mut evidence = FenceEvidence::Echo;
        if preconditions.guarded() {
            if self.pending_output {
                if !preconditions.selector_evidence() {
                    return Err(SendError::controller(
                        if self.expected_echo.is_some() {
                            ControllerRefusalCause::FencePending
                        } else {
                            ControllerRefusalCause::FenceUnverifiable
                        },
                        "controller input has not yet produced a verifiable PTY echo",
                    ));
                }
                evidence = FenceEvidence::Selector;
            } else if self.settling(now) {
                return Err(SendError::controller(
                    ControllerRefusalCause::FenceSettling,
                    "controller input output fence is still settling",
                ));
            }
        }
        // evaluate currently checks only controller_idle; add a distinct cause here if another predicate is introduced.
        preconditions.evaluate(self, now).map_err(|error| SendError::controller(ControllerRefusalCause::QuietTime, error))?;
        Ok(evidence)
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

    // #393: a guarded write happens only on a frame where its selector matches,
    // so a composer guard never writes over a draft or without a composer. A
    // recognizer-role selector with a quiet interval replaces a pending or
    // unverifiable echo fence, but only on a frame that has every role it names
    // (a negation cannot match an absent composer), and the fence records the
    // clearance for inspect and transfer only once the write succeeded.
    // Structural selectors and quiet time alone keep the conservative fence,
    // and a settling fence still refuses. The generator spans every fence state
    // (none, pending, both unverifiable causes, settling), absent, empty and
    // draft composers (spaces, multiline, cursor-left suffix), positive,
    // negated, structural and no selector, with and without a quiet floor
    // crossing that floor, and a write that succeeds or fails.
    #[hegel::test(test_cases = 500)]
    fn guarded_write_requires_match_and_selector_replaces_closed_fence(tc: hegel::TestCase) {
        use hegel::generators as gs;
        use terminal_screen::{Node, Rect, ScreenTree, Selector};
        const EMPTY: &str = "composer[value='']";
        const NEGATED: &str = "screen:not(:has(composer[value='x']))";
        let floor = Duration::from_secs(2);
        let fence = tc.draw(gs::integers::<u8>().min_value(0).max_value(4));
        let ms = Duration::from_millis(1);
        let elapsed = tc.draw(gs::sampled_from(vec![Duration::ZERO, floor - ms, floor, floor + ms, Duration::from_secs(3600)]));
        let composer =
            tc.draw(gs::sampled_from(vec![None, Some(""), Some(" "), Some("draft"), Some("line one\nline two"), Some("abc def")]));
        let selector = tc.draw(gs::sampled_from(vec![None, Some(EMPTY), Some(NEGATED), Some("screen")]));
        let idle = tc.draw(gs::booleans()).then_some(floor);
        let write_fails = tc.draw(gs::booleans());
        let start = Instant::now();
        let now = start + elapsed;
        let mut activity = ControllerActivity::default();
        if fence > 0 {
            activity.accepted(start);
            activity.written(match fence {
                1 | 4 => Some(b"typed".to_vec()),
                2 => None,
                _ => Some(vec![b'x'; CONTROLLER_QUEUE_BYTES + 1]),
            });
        }
        if fence == 4 {
            activity.observe_output(b"typed", now);
        }
        let mut tree = ScreenTree::from_root(0, Node::new("screen", Rect { col: 0, row: 0, width: 80, height: 24 }, ""));
        if let Some(value) = composer {
            let mut node = Node::new("composer", Rect { col: 0, row: 0, width: 80, height: 2 }, value);
            node.attributes.insert("value".into(), value.into());
            tree.add_node(tree.root(), node).unwrap();
        }
        let preconditions = SendPreconditions { controller_idle: idle, selector: selector.map(|source| Selector::parse(source).unwrap()) };
        let before = activity.fence(now);
        let mut writes = 0;
        let result = guarded_write(
            &mut writes,
            &mut activity,
            &preconditions,
            now,
            |_, selector, require_roles| evaluate_selector(selector, &tree, require_roles),
            |writes| {
                *writes += 1;
                if write_fails {
                    Err(SendError::from("PTY write failed"))
                } else {
                    Ok(())
                }
            },
        );

        let closed = matches!(fence, 1..=3);
        let role = matches!(selector, Some(EMPTY | NEGATED));
        let evidence = preconditions.guarded() && closed && role && idle.is_some();
        let quiet = idle.is_none() || fence == 0 || elapsed >= floor;
        let matched = (selector != Some(EMPTY) || composer == Some("")) && (!evidence || composer.is_some());
        let authorized = !preconditions.guarded() || ((!closed || evidence) && fence != 4 && quiet && matched);
        let context = format!("fence={fence} elapsed={elapsed:?} composer={composer:?} selector={selector:?} idle={idle:?}");
        assert_eq!(writes, usize::from(authorized), "{context}");
        if writes > 0 && selector == Some(EMPTY) {
            assert_eq!(composer, Some(""), "a composer guard wrote over a draft or without a composer");
        }
        if writes > 0 && evidence {
            assert!(composer.is_some(), "selector evidence without its role on the frame: {context}");
        }
        match result {
            Ok(()) if evidence => {
                let after = activity.fence(now);
                assert_eq!(after.state, ControllerFenceState::Clear);
                let cleared = ControllerFenceClearance { cause: ControllerFenceClearanceCause::Selector, generation: 1 };
                assert_eq!(after.cleared_by, Some(cleared));
                assert_eq!(ControllerActivity::from_history(activity.history()).fence(now), after);
                // The next controller write needs new evidence.
                activity.written(None);
                assert_eq!((activity.fence(now).state, activity.fence(now).cleared_by), (ControllerFenceState::Unverifiable, None));
            }
            Ok(()) => {
                assert!(!write_fails);
                assert_eq!(activity.fence(now), before);
            }
            Err(error) => {
                if authorized {
                    assert!(write_fails, "{context}");
                } else {
                    assert_eq!(error.status, 409);
                    assert!(["controller-active", "not-matched", "role-absent"].contains(&error.reason.as_str()));
                }
                assert_eq!(activity.fence(now), before, "a refusal or failed write must not change the fence");
            }
        }
    }

    // The public contract exposes state, timestamps and causes but never text or matching progress.
    // Generate every echo chunk size, masked/transformed output, both unverifiable causes and transfer in every state.
    #[test]
    fn diagnostic_states_causes_and_privacy() {
        let secret = "DISTINCTIVE-private-controller-λ";
        let now = Instant::now();
        let guard = SendPreconditions { controller_idle: Some(Duration::ZERO), ..Default::default() };
        let assert_public = |activity: &ControllerActivity, time, state, cause: Option<&str>| {
            let fence = activity.fence(time);
            assert_eq!(fence.state, state);
            let json = serde_json::to_value(&fence).unwrap();
            assert_eq!(json.get("settles_at").is_some(), state == ControllerFenceState::Settling);
            assert_eq!(json.get("cause").is_some(), state == ControllerFenceState::Unverifiable);
            assert_eq!(
                json.as_object().unwrap().len(),
                2 + usize::from(state == ControllerFenceState::Settling || state == ControllerFenceState::Unverifiable)
            );
            if let Some(cause) = cause {
                let error = activity.check(&guard, time).unwrap_err();
                assert_eq!(error.status, 409);
                assert_eq!(error.reason, "controller-active");
                assert_eq!(error.controller.as_ref().unwrap().cause.as_str(), cause);
                for public in [serde_json::to_string(&fence).unwrap(), serde_json::to_string(&error).unwrap(), error.to_string()] {
                    assert!(!public.contains(secret));
                    assert!(!public.contains("echo_matched"));
                    assert!(!public.contains("expected_echo"));
                }
            } else {
                assert!(activity.check(&guard, time).is_ok());
            }
        };
        let fresh = ControllerActivity::default();
        assert_public(&fresh, now, ControllerFenceState::Clear, None);
        assert_eq!(fresh.fence(now).since, None);
        for chunk in 1..=secret.len() {
            let mut activity = ControllerActivity::default();
            activity.accepted(now);
            activity.written(Some(secret.as_bytes().to_vec()));
            let pending_since = activity.fence(now).since;
            assert!(pending_since.is_some());
            for output in [b"".as_slice(), b"********", b"DISTINCTIVE-PRIVATE-CONTROLLER"] {
                activity.observe_output(output, now);
                assert_public(&activity, now, ControllerFenceState::Pending, Some("fence-pending"));
            }
            activity = ControllerActivity::from_history(activity.history());
            assert_eq!(activity.fence(now).since, pending_since);
            assert_public(&activity, now, ControllerFenceState::Pending, Some("fence-pending"));
            for bytes in secret.as_bytes().chunks(chunk) {
                activity.observe_output(bytes, now);
            }
            assert_public(&activity, now, ControllerFenceState::Settling, Some("fence-settling"));
            let settling = activity.fence(now);
            assert_eq!(settling.settles_at, settling.since.map(|at| at + 100));
            // Inject both clock readings so loaded runners cannot expire this settling assertion.
            let transferred = ControllerActivity::from_history_at(activity.history(), now, settling.since.unwrap());
            assert_eq!(transferred.fence(now), settling);
            assert_public(&activity, now + SUBMIT_ENTER_DELAY, ControllerFenceState::Clear, None);
            assert_eq!(activity.fence(now + SUBMIT_ENTER_DELAY).since, settling.settles_at);
            let mut history = activity.history();
            history.observed_output_at = Some(unix_ms().saturating_sub(1000));
            let transferred = ControllerActivity::from_history_at(history, now, unix_ms());
            assert_public(&transferred, now, ControllerFenceState::Clear, None);
        }
        for (expected, cause) in
            [(None, ControllerFenceCause::NoKnownText), (Some(vec![b'x'; CONTROLLER_QUEUE_BYTES + 1]), ControllerFenceCause::Oversized)]
        {
            let mut activity = ControllerActivity::default();
            activity.accepted(now);
            activity.written(expected);
            assert_eq!(activity.fence(now).cause, Some(cause));
            let before = activity.fence(now);
            activity = ControllerActivity::from_history(activity.history());
            activity.written(Some(b"later".to_vec()));
            activity.observe_output(b"later", now);
            assert_eq!(activity.fence(now), before);
            assert_public(&activity, now, ControllerFenceState::Unverifiable, Some("fence-unverifiable"));
            // New child activations use empty history, resetting even an unverifiable fence.
            let fresh = ControllerActivity::from_history(Default::default());
            assert_eq!(fresh.fence(now), ControllerFence::default());
        }
        let mut activity = ControllerActivity::default();
        activity.accepted(now);
        let quiet = SendPreconditions { controller_idle: Some(Duration::from_secs(1)), ..Default::default() };
        assert_eq!(activity.check(&quiet, now).unwrap_err().controller.unwrap().cause, ControllerRefusalCause::QuietTime);
        assert_eq!(SendError::controller(ControllerRefusalCause::Busy, "submission busy").controller.unwrap().cause.as_str(), "busy");
        // The inclusive evidence limit is still verifiable; the next byte closes it beyond echo evidence.
        activity.written(Some(vec![b'x'; CONTROLLER_QUEUE_BYTES]));
        assert_eq!(activity.fence(now).state, ControllerFenceState::Pending);
        activity.written(Some(vec![b'x']));
        assert_eq!(activity.fence(now).cause, Some(ControllerFenceCause::Oversized));
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
    // HTTP is authoritative for status; the JSON body carries only reason and
    // diagnostic. Clients restore status from the response, including legacy bodies.
    #[serde(skip)]
    pub(crate) status: u16,
    pub(crate) reason: String,
    pub(crate) error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) controller: Option<ControllerRefusal>,
}
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct ControllerRefusal {
    pub(crate) cause: ControllerRefusalCause,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ControllerRefusalCause {
    FencePending,
    FenceUnverifiable,
    FenceSettling,
    QuietTime,
    Busy,
}

impl ControllerRefusalCause {
    fn as_str(self) -> &'static str {
        match self {
            Self::FencePending => "fence-pending",
            Self::FenceUnverifiable => "fence-unverifiable",
            Self::FenceSettling => "fence-settling",
            Self::QuietTime => "quiet-time",
            Self::Busy => "busy",
        }
    }
}
impl SendError {
    pub(crate) fn controller(cause: ControllerRefusalCause, error: impl Into<String>) -> Self {
        let mut result = Self::refused("controller-active", error);
        result.controller = Some(ControllerRefusal { cause });
        result
    }
    pub(crate) fn new(status: u16, reason: &str, error: impl Into<String>) -> Self {
        Self { status, reason: reason.into(), error: error.into(), controller: None }
    }
    pub(crate) fn refused(reason: &str, error: impl Into<String>) -> Self {
        Self::new(409, reason, error)
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
        if self.reason == "error" {
            f.write_str(&self.error)
        } else {
            write!(f, "{}: {}", self.reason, self.error)?;
            if let Some(controller) = &self.controller {
                write!(f, " (controller.cause={})", controller.cause.as_str())?;
            }
            Ok(())
        }
    }
}

/// The guard and key bytes that delete exactly `draft` from a composer (#393).
///
/// The selector requires the composer's observed value to equal `draft`. For a
/// draft of N characters the keys are N forward deletes then N backspaces, at
/// least one key per character (a grapheme-deleting editor needs fewer). They
/// empty the buffer from any cursor position provided the application, as
/// Claude Code and Codex do, ignores Delete and Backspace at a buffer edge.
/// Nothing outside the composer buffer is addressed, and no Escape or submit
/// key is sent.
pub(crate) fn composer_clear(draft: &str) -> Result<(terminal_screen::Selector, Vec<u8>), SendError> {
    use crate::keys::{encode_named_key, Modifiers, NamedKey};
    if draft.is_empty() {
        return Err(SendError::new(400, "error", "guarded clear requires the nonempty draft that was read"));
    }
    let quoted = draft.replace('\\', "\\\\").replace('\'', "\\'");
    let selector = terminal_screen::Selector::parse(&format!("composer[value='{quoted}']"))
        .map_err(|error| SendError::new(400, "error", error.to_string()))?;
    let count = draft.chars().count();
    let delete = encode_named_key(NamedKey::Delete, Modifiers::default()).ok_or("delete key has no encoding")?;
    let backspace = encode_named_key(NamedKey::Backspace, Modifiers::default()).ok_or("backspace key has no encoding")?;
    let mut keys = delete.repeat(count);
    keys.extend(backspace.repeat(count));
    Ok((selector, keys))
}

/// Structure needs no recognizer; all other names require a declared role source.
pub(crate) fn selector_roles(selector: &terminal_screen::Selector) -> std::collections::BTreeSet<String> {
    selector.referenced_elements().into_iter().filter(|name| !terminal_screen::STRUCTURAL_ELEMENTS.contains(&name.as_str())).collect()
}

pub(crate) fn evaluate_selector(
    selector: &terminal_screen::Selector,
    tree: &terminal_screen::ScreenTree,
    require_roles: bool,
) -> Result<(), SendError> {
    let absent = || {
        selector_roles(selector).iter().any(|role| {
            !tree.document_order().iter().filter_map(|id| tree.node(*id)).any(|node| node.element == *role || node.roles.contains(role))
        })
    };
    if require_roles && absent() {
        return Err(SendError::refused("role-absent", "selector role is absent from the current frame"));
    }
    if !selector.evaluate(tree).is_empty() {
        return Ok(());
    }
    let absent = absent();
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
            let result = evaluate_selector(&selector, &tree, false);
            assert_eq!(result.is_ok(), value == Some(""));
            if let Err(error) = result {
                assert_eq!(error.status, 409);
                assert_eq!(error.reason, if value.is_none() { "role-absent" } else { "not-matched" });
            }
            assert_eq!(tree, before);
        }
    }

    // #393 guarded clear: the guard matches exactly the draft that was read
    // (quotes, backslashes, newlines and spaces included, never a longer or
    // shorter value), and from any cursor position its keys empty a buffer that
    // holds that draft. The generator draws drafts over an alphabet of selector
    // metacharacters, whitespace and multibyte text, and every cursor position.
    #[hegel::test(test_cases = 200)]
    fn guarded_clear_matches_exact_draft_and_deletes_it(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let alphabet = vec!['a', ' ', '\'', '"', '\\', '\n', ']', '=', 'é', '日'];
        let draft: String = tc.draw(gs::vecs(gs::sampled_from(alphabet)).max_size(12)).into_iter().collect();
        let Ok((selector, keys)) = composer_clear(&draft) else {
            assert!(draft.is_empty(), "only an empty draft is refused");
            return;
        };
        let frame = |value: &str| {
            let mut tree = ScreenTree::from_root(0, Node::new("screen", Rect { col: 0, row: 0, width: 80, height: 24 }, ""));
            let mut node = Node::new("composer", Rect { col: 0, row: 0, width: 80, height: 2 }, value);
            node.attributes.insert("value".into(), value.into());
            tree.add_node(tree.root(), node).unwrap();
            tree
        };
        assert!(evaluate_selector(&selector, &frame(&draft), false).is_ok());
        let shorter: String = draft.chars().skip(1).collect();
        for other in [format!("{draft}x"), format!(" {draft}"), shorter, String::new()] {
            assert_eq!(evaluate_selector(&selector, &frame(&other), false).unwrap_err().reason, "not-matched", "{other:?}");
        }
        // Line-editor model of the composer buffer: Delete removes the character
        // at the cursor, Backspace the one before it, each a no-op at an edge.
        let mut buffer: Vec<char> = draft.chars().collect();
        let mut cursor = tc.draw(gs::integers::<usize>().min_value(0).max_value(buffer.len()));
        let mut rest = keys.as_slice();
        while !rest.is_empty() {
            if let Some(after) = rest.strip_prefix(b"\x1b[3~") {
                if cursor < buffer.len() {
                    buffer.remove(cursor);
                }
                rest = after;
            } else {
                assert_eq!(rest[0], 0x7f, "only Delete and Backspace are sent");
                if cursor > 0 {
                    cursor -= 1;
                    buffer.remove(cursor);
                }
                rest = &rest[1..];
            }
        }
        assert!(buffer.is_empty());
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
                        let result = evaluate_selector(&selector, &tree, false);
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
                            assert!(evaluate_selector(&selector, &left, false).is_ok(), "{id} {size} space-only cursor-left limit");
                            vt.feed(b"\x1b8").unwrap();
                        }
                    }
                }
            }
        }
    }
}
