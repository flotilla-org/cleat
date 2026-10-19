//! Attachment-owned probing, source arbitration and OSC 7501 serialization.
use std::{collections::BTreeMap, time::Duration};

use crate::{
    kitty_output::base64,
    program_status::{strip_invisible_formatting, ProgramStatusRecord, ProgramStatusSource},
};

/// Whole status snapshot sent on an opted-in, admitted foreground channel.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct StatusSnapshot {
    pub available: bool,
    pub records: Vec<ProgramStatusRecord>,
    pub program_reported: bool,
}

pub const PROBE_TIMEOUT: Duration = Duration::from_millis(200);
pub const DISCARD_WINDOW: Duration = Duration::from_secs(3);
pub const QUERY: &[u8] = b"\x1b]7501;?\x1b\\";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum ProgramStatusMode {
    #[default]
    Auto,
    On,
    Off,
}

/// Recognize only our reply, buffering at most one candidate prefix. All other
/// bytes retain their order and go to the existing terminal input decoder.
pub(crate) struct Probe {
    pending: Vec<u8>,
    auto: bool,
    paste: bool,
    paste_tail: Vec<u8>,
    pub enabled: bool,
}
impl Probe {
    pub fn new(mode: ProgramStatusMode) -> Self {
        Self {
            pending: Vec::new(),
            auto: matches!(mode, ProgramStatusMode::Auto),
            enabled: matches!(mode, ProgramStatusMode::On),
            paste: false,
            paste_tail: Vec::new(),
        }
    }
    pub fn query(&self) -> &'static [u8] {
        if self.auto {
            QUERY
        } else {
            b""
        }
    }
    pub fn feed(&mut self, bytes: &[u8], elapsed: Duration) -> Vec<u8> {
        if !self.auto || elapsed > DISCARD_WINDOW {
            let mut out = std::mem::take(&mut self.pending);
            out.extend_from_slice(bytes);
            return out;
        }
        let mut out = Vec::new();
        for &byte in bytes {
            // A bracketed paste is user text, even if it contains our reply.
            if self.paste {
                out.push(byte);
                self.paste_tail.push(byte);
                if self.paste_tail.len() > 6 {
                    self.paste_tail.remove(0);
                }
                if self.paste_tail == b"\x1b[201~" {
                    self.paste = false;
                    self.paste_tail.clear();
                }
                continue;
            }
            self.pending.push(byte);
            loop {
                let replies: [&[u8]; 6] =
                    [QUERY, b"\x1b]7501;?\x07", b"\x9d7501;?\x9c", b"\x9d7501;?\x07", b"\x1b]7501;?\x9c", b"\x9d7501;?\x1b\\"];
                if self.pending == b"\x1b[200~" {
                    out.append(&mut self.pending);
                    self.paste = true;
                    break;
                }
                if replies.iter().any(|reply| *reply == self.pending) {
                    self.pending.clear();
                    if elapsed <= PROBE_TIMEOUT {
                        self.enabled = true;
                    }
                    break;
                }
                if replies.iter().any(|reply| reply.starts_with(&self.pending)) || b"\x1b[200~".starts_with(&self.pending) {
                    break;
                }
                out.push(self.pending.remove(0));
                if self.pending.is_empty() {
                    break;
                }
            }
        }
        out
    }
}

#[derive(Default)]
pub(crate) struct StatusEmitter {
    emitted: BTreeMap<String, (Vec<u8>, u64)>,
}
impl StatusEmitter {
    pub fn update(&mut self, records: &[ProgramStatusRecord], program_reported: bool, enabled: bool) -> Vec<u8> {
        let mut next = BTreeMap::new();
        if enabled {
            for record in records {
                if record.source != ProgramStatusSource::Osc7501 && (program_reported || !record.id.is_empty()) {
                    continue;
                }
                if let Some(report) = encode(record) {
                    if record.source == ProgramStatusSource::Osc7501 || !next.contains_key(&record.id) {
                        next.insert(record.id.clone(), (report, record.since));
                    }
                }
            }
        }
        let mut out = Vec::new();
        // Clear first, then replay descendants: root clear also clears all children.
        for id in self.emitted.keys().filter(|id| !next.contains_key(*id)) {
            out.extend(clear(id));
        }
        let removed = self.emitted.keys().any(|id| !next.contains_key(id));
        for (id, report) in &next {
            if removed || self.emitted.get(id) != Some(report) {
                out.extend_from_slice(&report.0);
            }
        }
        self.emitted = next;
        out
    }
    pub fn clear(&mut self) -> Vec<u8> {
        self.update(&[], false, false)
    }
}
fn valid_word(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.+-".contains(&b))
}
fn valid_id(id: &str) -> bool {
    id.is_empty() || id.len() <= 128 && id.split('/').count() <= 8 && id.split('/').all(|p| p.len() <= 32 && valid_word(p))
}
fn clear(id: &str) -> Vec<u8> {
    if id.is_empty() {
        b"\x1b]7501;state=clear\x1b\\".to_vec()
    } else {
        format!("\x1b]7501;state=clear:id={id}\x1b\\").into_bytes()
    }
}
fn encode(record: &ProgramStatusRecord) -> Option<Vec<u8>> {
    if !valid_id(&record.id)
        || record.app.len() > 32
        || !record.app.is_empty() && !valid_word(&record.app)
        || record.progress.is_some_and(|p| p > 100)
    {
        return None;
    }
    let msg = strip_invisible_formatting(&record.msg);
    let title = strip_invisible_formatting(&record.title);
    if msg.len() > 2048 || title.len() > 192 {
        return None;
    }
    let state = match record.state {
        crate::program_status::ProgramState::Idle => "idle",
        crate::program_status::ProgramState::Working => "working",
        crate::program_status::ProgramState::Done => "done",
        crate::program_status::ProgramState::Blocked => "blocked",
        crate::program_status::ProgramState::Error => "error",
    };
    let mut out = format!("\x1b]7501;state={state}");
    if !record.id.is_empty() {
        out.push_str(&format!(":id={}", record.id));
    }
    if !record.app.is_empty() {
        out.push_str(&format!(":app={}", record.app));
    }
    if let Some(kind) = record.kind {
        out.push_str(&format!(":kind={}", match kind {
            crate::program_status::ProgramKind::Permission => "permission",
            crate::program_status::ProgramKind::Question => "question",
            crate::program_status::ProgramKind::Auth => "auth",
        }));
    }
    if let Some(progress) = record.progress {
        out.push_str(&format!(":progress={progress}"));
    }
    if !msg.is_empty() {
        out.push_str(&format!(":msg={}", base64(msg.as_bytes())));
    }
    if !title.is_empty() {
        out.push_str(&format!(":title={}", base64(title.as_bytes())));
    }
    out.push_str("\x1b\\");
    (out.len() <= 4096).then(|| out.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::program_status::{ProgramState, ProgramStatusTracker};
    fn record(id: &str, state: ProgramState, source: ProgramStatusSource) -> ProgramStatusRecord {
        ProgramStatusRecord {
            id: id.into(),
            source,
            state,
            kind: None,
            progress: None,
            app: String::new(),
            msg: String::new(),
            title: String::new(),
            since: 0,
        }
    }
    // The probe enables only at/before 200 ms; replies up to 3 s are consumed
    // without enabling late. Generate every split, both terminators, and boundary times.
    #[test]
    fn probe_deadlines_and_splits() {
        for reply in [QUERY, b"\x1b]7501;?\x07", b"\x9d7501;?\x9c", b"\x9d7501;?\x07"] {
            for elapsed in [0, 199, 200, 201, 2999, 3000] {
                for split in 0..=reply.len() {
                    let mut probe = Probe::new(ProgramStatusMode::Auto);
                    let time = Duration::from_millis(elapsed);
                    let mut bytes = probe.feed(&reply[..split], time);
                    bytes.extend(probe.feed(&reply[split..], time));
                    assert!(bytes.is_empty());
                    assert_eq!(probe.enabled, time <= PROBE_TIMEOUT);
                    assert_eq!(probe.feed(b"user input", time), b"user input");
                }
            }
        }
        let mut silent = Probe::new(ProgramStatusMode::Auto);
        assert!(silent.feed(&[], PROBE_TIMEOUT).is_empty());
        assert!(!silent.enabled);
        assert_eq!(silent.feed(QUERY, DISCARD_WINDOW + Duration::from_millis(1)), QUERY);
        assert!(!silent.enabled);
    }
    // Bracketed paste is user data: embedded replies must neither enable the
    // probe nor disappear, under every transport split.
    #[test]
    fn paste_does_not_answer_the_probe() {
        let paste = [b"\x1b[200~".as_slice(), QUERY, b"\x1b[201~"].concat();
        for split in 0..=paste.len() {
            let mut probe = Probe::new(ProgramStatusMode::Auto);
            let mut bytes = probe.feed(&paste[..split], Duration::ZERO);
            bytes.extend(probe.feed(&paste[split..], Duration::ZERO));
            assert_eq!(bytes, paste);
            assert!(!probe.enabled);
            assert!(probe.feed(QUERY, Duration::ZERO).is_empty());
            assert!(probe.enabled);
        }
    }
    // Overrides never probe, and do not consume unsolicited status-looking input.
    #[test]
    fn overrides_and_mismatched_prefixes() {
        for mode in [ProgramStatusMode::On, ProgramStatusMode::Off] {
            let mut probe = Probe::new(mode);
            assert!(probe.query().is_empty());
            assert_eq!(probe.enabled, mode == ProgramStatusMode::On);
            assert_eq!(probe.feed(QUERY, Duration::ZERO), QUERY);
        }
        for input in [b"\x1b[?1;2c".as_slice(), b"\x1b]0;title\x07", b"\x1b\x1b[A", b"\x1b]7501;state=done\x07"] {
            let mut probe = Probe::new(ProgramStatusMode::Auto);
            let mut actual = Vec::new();
            for byte in input {
                actual.extend(probe.feed(&[*byte], Duration::ZERO));
            }
            actual.extend(probe.feed(&[], DISCARD_WINDOW + Duration::from_millis(1)));
            assert_eq!(actual, input);
            assert!(!probe.enabled);
        }
    }
    // Replay, whole replacement, hierarchical removal and detach clear are id-preserving.
    // Generate root/child ids and every state; a removed parent must not erase surviving children.
    #[test]
    fn replay_change_remove_detach() {
        for id in ["", "build", "build/test"] {
            for state in [ProgramState::Idle, ProgramState::Working, ProgramState::Blocked, ProgramState::Done, ProgramState::Error] {
                let first = record(id, state, ProgramStatusSource::Osc7501);
                let child = record("build/test/unit", state, ProgramStatusSource::Osc7501);
                let mut emitter = StatusEmitter::default();
                assert!(emitter.update(std::slice::from_ref(&first), true, false).is_empty());
                assert_eq!(emitter.update(std::slice::from_ref(&first), true, true), encode(&first).unwrap());
                assert!(emitter.update(std::slice::from_ref(&first), true, true).is_empty());
                let mut refreshed = first.clone();
                refreshed.since += 1;
                assert_eq!(emitter.update(std::slice::from_ref(&refreshed), true, true), encode(&first).unwrap());
                let mut replacement = first.clone();
                replacement.msg = "changed".into();
                assert_eq!(
                    emitter.update(&[replacement.clone(), child.clone()], true, true),
                    [encode(&replacement).unwrap(), encode(&child).unwrap()].concat()
                );
                assert_eq!(emitter.update(std::slice::from_ref(&child), true, true), [clear(id), encode(&child).unwrap()].concat());
                assert_eq!(emitter.clear(), clear(&child.id));
                assert!(emitter.clear().is_empty());
            }
        }
    }
    // Arbitration lives in attach. Both sources remain in the store; program ownership
    // survives record disappearance and acknowledgement and ends at the next prompt/exit.
    #[test]
    fn program_ownership_survives_clear_and_ack() {
        let tracker = ProgramStatusTracker::default();
        let inferred = record("", ProgramState::Working, ProgramStatusSource::Recognizer);
        let own = record("", ProgramState::Done, ProgramStatusSource::Osc7501);
        tracker.replace(inferred.clone());
        let mut emitter = StatusEmitter::default();
        let (_, records, reported) = tracker.snapshot_since(None).unwrap();
        assert_eq!(emitter.update(&records, reported, true), encode(&inferred).unwrap());
        tracker.replace(own.clone());
        assert_eq!(tracker.records().len(), 2);
        let (_, records, reported) = tracker.snapshot_since(None).unwrap();
        assert_eq!(emitter.update(&records, reported, true), encode(&own).unwrap());
        tracker.acknowledge();
        let (_, records, reported) = tracker.snapshot_since(None).unwrap();
        assert_eq!(emitter.update(&records, reported, true), clear(""));
        tracker.clear("");
        tracker.replace(inferred.clone());
        let (_, records, reported) = tracker.snapshot_since(None).unwrap();
        assert!(emitter.update(&records, reported, true).is_empty());
        tracker.program_ended();
        tracker.replace(inferred.clone());
        let (_, records, reported) = tracker.snapshot_since(None).unwrap();
        assert_eq!(emitter.update(&records, reported, true), encode(&inferred).unwrap());
    }
    // Independent live attachments emit the same records; history/off attachments
    // emit none. Returning to live replays, and acknowledgement clears both live views.
    #[test]
    fn live_watchers_and_history() {
        let tracker = ProgramStatusTracker::default();
        tracker.replace(record("task", ProgramState::Done, ProgramStatusSource::Osc7501));
        let mut driver = StatusEmitter::default();
        let mut watcher = StatusEmitter::default();
        let mut history = StatusEmitter::default();
        let records = tracker.records();
        assert_eq!(driver.update(&records, true, true), watcher.update(&records, true, true));
        assert!(history.update(&records, true, false).is_empty());
        assert!(!history.update(&records, true, true).is_empty());
        assert_eq!(history.update(&records, true, false), clear("task"));
        tracker.acknowledge();
        assert_eq!(driver.update(&tracker.records(), true, true), clear("task"));
        assert_eq!(watcher.update(&tracker.records(), true, true), clear("task"));
    }
    // Serialization removes invisible controls and validates all field byte limits.
    // Generate exact boundaries and one byte beyond, including Unicode field lengths.
    #[test]
    fn safe_serialization_and_limits() {
        let mut status = record("original/id", ProgramState::Blocked, ProgramStatusSource::Osc7501);
        status.msg = "a\u{202e}b\u{2066}é\u{200b}".into();
        status.title = "t\u{feff}itle".into();
        let encoded = String::from_utf8(encode(&status).unwrap()).unwrap();
        assert!(encoded.contains(":id=original/id"));
        assert!(encoded.contains(&format!(":msg={}", base64("abé".as_bytes()))));
        assert!(encoded.contains(&format!(":title={}", base64(b"title"))));
        for size in [0, 191, 192, 193, 2047, 2048, 2049] {
            status.msg = "a".repeat(size);
            status.title.clear();
            assert_eq!(encode(&status).is_some(), size <= 2048);
            status.msg.clear();
            status.title = "a".repeat(size);
            assert_eq!(encode(&status).is_some(), size <= 192);
        }
        for id in ["bad//id".to_string(), "bad:id".into(), "x".repeat(33), "x/".repeat(8) + "x"] {
            status.title.clear();
            status.id = id;
            assert!(encode(&status).is_none());
        }
    }
    // Prompt and full-reset callbacks reset first-report ownership, including
    // when prompt start retains a done record and changes no record fields.
    #[cfg(feature = "ghostty-vt")]
    #[test]
    fn vt_lifetime_events_reset_ownership() {
        use crate::vt::VtEngine;
        for boundary in [b"\x1b]133;A\x07".as_slice(), b"\x1bc"] {
            let mut engine = crate::vt::ghostty::GhosttyVtEngine::new(80, 24);
            engine.feed(b"\x1b]7501;state=done\x07").unwrap();
            let tracker = engine.program_status_tracker().unwrap();
            let (generation, _, reported) = tracker.snapshot_since(None).unwrap();
            assert!(reported);
            engine.feed(boundary).unwrap();
            let (_, records, reported) = tracker.snapshot_since(Some(generation)).unwrap();
            assert!(!reported);
            assert_eq!(records.len(), usize::from(boundary != b"\x1bc"));
        }
    }
    // A nested session owns the inner probe; filtering removes that query from outer
    // presentation, so a capable outer terminal cannot generate a second reply.
    #[cfg(feature = "ghostty-vt")]
    #[test]
    fn nested_query_has_one_owner() {
        use crate::vt::VtEngine;
        let mut outer = crate::vt::ghostty::GhosttyVtEngine::new(80, 24);
        let mut filter = crate::status_output::StatusOutputFilter::default();
        outer.feed(QUERY).unwrap();
        let mut inner = Probe::new(ProgramStatusMode::Auto);
        assert!(inner.feed(&outer.drain_session_replies(), Duration::ZERO).is_empty());
        assert!(inner.enabled);
        assert!(filter.push(QUERY).is_empty());
        assert!(outer.drain_session_replies().is_empty());
    }
}
