//! Session-owned OSC 7501 records. Parsing and validation belong to the VT engine.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ProgramState {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProgramStatusSource {
    Osc7501,
    Title,
    Recognizer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProgramKind {
    Permission,
    Question,
    Auth,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramStatusRecord {
    /// Empty for the root record; scoped to the session containing this record.
    pub id: String,
    pub source: ProgramStatusSource,
    pub state: ProgramState,
    pub kind: Option<ProgramKind>,
    pub progress: Option<u8>,
    pub app: String,
    pub msg: String,
    pub title: String,
    /// Unix milliseconds when this whole record was last replaced.
    pub since: u64,
}

#[derive(Default)]
pub struct ProgramStatusStore {
    records: Vec<ProgramStatusRecord>,
    reported: bool,
}

impl ProgramStatusStore {
    pub fn records(&self) -> &[ProgramStatusRecord] {
        &self.records
    }

    pub fn replace(&mut self, mut record: ProgramStatusRecord) {
        record.msg = strip_invisible_formatting(&record.msg);
        record.title = strip_invisible_formatting(&record.title);
        if record.source == ProgramStatusSource::Osc7501 {
            self.reported = true;
        }
        self.records.retain(|old| {
            old.id != record.id
                || (old.source != record.source
                    && (old.source == ProgramStatusSource::Osc7501 || record.source == ProgramStatusSource::Osc7501))
        });
        // Insertion order breaks timestamp ties and remains stable if the wall clock moves.
        if self.records.len() == 256 {
            self.records.remove(0);
        }
        self.records.push(record);
    }

    pub fn clear(&mut self, id: &str) -> bool {
        if id.is_empty() {
            self.reported = false;
        }
        let before = self.records.len();
        self.records
            .retain(|record| !id.is_empty() && record.id != id && !record.id.strip_prefix(id).is_some_and(|rest| rest.starts_with('/')));
        self.records.len() != before
    }

    pub fn program_ended(&mut self) -> bool {
        let reported = std::mem::take(&mut self.reported);
        let before = self.records.len();
        self.records.retain(|record| matches!(record.state, ProgramState::Done | ProgramState::Error));
        self.records.len() != before || (reported && self.records.iter().any(|record| record.source != ProgramStatusSource::Osc7501))
    }

    pub fn acknowledge(&mut self) -> bool {
        let before = self.records.len();
        self.records.retain(|record| !matches!(record.state, ProgramState::Done | ProgramState::Error));
        self.records.len() != before
    }
}

/// Shared observation of the actor-owned store; readers never send actor commands.
#[derive(Clone, Default)]
pub struct ProgramStatusTracker(std::sync::Arc<std::sync::Mutex<TrackedProgramStatus>>);
#[derive(Default)]
struct TrackedProgramStatus {
    store: ProgramStatusStore,
    generation: u64,
    program_reported: bool,
    roles: Vec<crate::recognizer::Role>,
}
impl ProgramStatusTracker {
    fn read<T>(&self, f: impl FnOnce(&TrackedProgramStatus) -> T) -> T {
        f(&self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()))
    }
    fn update(&self, f: impl FnOnce(&mut ProgramStatusStore) -> bool) {
        let mut state = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if f(&mut state.store) {
            state.generation = state.generation.wrapping_add(1);
        }
    }
    pub fn observations_since(&self, generation: Option<u64>) -> Option<(u64, Vec<ProgramStatusRecord>, Vec<crate::recognizer::Role>, bool)> {
        self.read(|state| {
            (generation != Some(state.generation)).then(|| (state.generation, state.store.records().to_vec(), state.roles.clone(), state.program_reported))
        })
    }
    /// Publish the status and roles from one evaluation at one subscription cut point.
    pub fn publish_recognition(&self, record: Option<ProgramStatusRecord>, roles: Vec<crate::recognizer::Role>, clear: bool) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let mut changed = false;
        if let Some(record) = record {
            state.store.replace(record);
            changed = true;
        }
        if clear {
            let before = state.store.records.len();
            state.store.records.retain(|record| record.source == ProgramStatusSource::Osc7501);
            changed |= before != state.store.records.len();
        }
        if state.roles != roles {
            state.roles = roles;
            changed = true;
        }
        if changed {
            state.generation = state.generation.wrapping_add(1);
        }
    }
    pub fn replace_roles(&self, roles: Vec<crate::recognizer::Role>) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if state.roles != roles {
            state.roles = roles;
            state.generation = state.generation.wrapping_add(1);
        }
    }
    /// ADR 0007 rule 6: inferred fallback stops after the first own report, until program end.
    pub fn effective_records(&self) -> Vec<ProgramStatusRecord> {
        self.read(|state| {
            state
                .store
                .records
                .iter()
                .filter(|record| !state.store.reported || record.source == ProgramStatusSource::Osc7501)
                .cloned()
                .collect()
        })
    }
    pub fn mark_reported(&self) {
        self.update(|store| {
            let changed = !store.reported;
            store.reported = true;
            changed
        });
    }
    pub fn clear_inferred(&self) {
        self.update(|store| {
            let before = store.records.len();
            store.records.retain(|r| r.source == ProgramStatusSource::Osc7501);
            before != store.records.len()
        });
    }
    pub fn records(&self) -> Vec<ProgramStatusRecord> {
        self.read(|state| state.store.records().to_vec())
    }
    /// The generation and records share one cut point. Unchanged reads allocate nothing.
    pub fn records_since(&self, generation: Option<u64>) -> Option<(u64, Vec<ProgramStatusRecord>)> {
        self.read(|state| (generation != Some(state.generation)).then(|| (state.generation, state.store.records().to_vec())))
    }
    pub fn is_empty(&self) -> bool {
        self.read(|state| state.store.records().is_empty())
    }
    pub fn matches_states(&self, states: &[ProgramState]) -> bool {
        self.read(|state| {
            state
                .store
                .records
                .iter()
                .any(|record| (!state.store.reported || record.source == ProgramStatusSource::Osc7501) && states.contains(&record.state))
        })
    }
    pub fn reset(&self) {
        let mut state = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.store.clear("");
        state.program_reported = false;
        state.generation = state.generation.wrapping_add(1);
    }
    pub fn report_received(&self) {
        let mut state = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.program_reported {
            state.program_reported = true;
            state.generation = state.generation.wrapping_add(1);
        }
    }
    pub fn program_reported(&self) -> bool {
        self.read(|state| state.program_reported)
    }
    pub fn snapshot_since(&self, generation: Option<u64>) -> Option<(u64, Vec<ProgramStatusRecord>, bool)> {
        self.read(|state| {
            (generation != Some(state.generation)).then(|| (state.generation, state.store.records().to_vec(), state.program_reported))
        })
    }
    pub fn replace(&self, record: ProgramStatusRecord) {
        if record.source == ProgramStatusSource::Osc7501 {
            self.report_received();
        }
        self.update(|store| {
            store.replace(record);
            true
        });
    }
    pub fn clear(&self, id: &str) {
        self.update(|store| store.clear(id));
    }
    pub fn program_ended(&self) {
        let mut state = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let changed = state.store.program_ended() || state.program_reported;
        state.program_reported = false;
        if changed {
            state.generation = state.generation.wrapping_add(1);
        }
    }
    pub fn acknowledge(&self) {
        self.update(ProgramStatusStore::acknowledge);
    }
}

/// Remove Unicode format controls and default-ignorable characters from untrusted labels.
/// Preserve ordinary combining marks and visible text.
pub fn strip_invisible_formatting(text: &str) -> String {
    text.chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(*c as u32,
                    0x00ad | 0x034f | 0x061c | 0x115f..=0x1160 | 0x17b4..=0x17b5 |
                    0x180b..=0x180f | 0x200b..=0x200f | 0x202a..=0x202e | 0x2060..=0x206f |
                    0x3164 | 0xfe00..=0xfe0f | 0xfeff | 0xffa0 | 0xfff0..=0xfff8 |
                    0x1bca0..=0x1bca3 | 0x1d173..=0x1d17a | 0xe0000..=0xe0fff
                )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(id: &str, state: ProgramState, since: u64) -> ProgramStatusRecord {
        ProgramStatusRecord {
            id: id.into(),
            state,
            since,
            source: ProgramStatusSource::Osc7501,
            kind: None,
            progress: None,
            app: String::new(),
            title: String::new(),
            msg: String::new(),
        }
    }

    // Every report completely replaces the previous record, and eviction follows update order.
    // Generate counts crossing the 256 boundary, replacements, timestamp ties and backward wall-clock changes.
    #[test]
    fn replacement_and_capacity() {
        let mut store = ProgramStatusStore::default();
        for n in 0..258 {
            store.replace(record(&n.to_string(), ProgramState::Working, 258 - n / 2));
        }
        assert_eq!(store.records().len(), 256);
        assert_eq!(store.records()[0].id, "2");
        let mut original = record("2", ProgramState::Blocked, 260);
        original.msg = "old".into();
        original.progress = Some(42);
        original.kind = Some(ProgramKind::Auth);
        store.replace(original);
        store.replace(record("2", ProgramState::Done, 261));
        assert_eq!(store.records().last().unwrap(), &record("2", ProgramState::Done, 261));
        store.replace(record("new", ProgramState::Idle, 262));
        assert!(!store.records().iter().any(|r| r.id == "3"));
    }

    // A clear removes descendants even if their parent has no record; empty id clears all.
    #[test]
    fn hierarchical_clear() {
        let mut store = ProgramStatusStore::default();
        for id in ["", "build/test", "build/test/unit", "builder", "build2/test"] {
            store.replace(record(id, ProgramState::Done, 0));
        }
        store.clear("build");
        assert_eq!(store.records().iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["", "builder", "build2/test"]);
        store.clear("");
        assert!(store.records().is_empty());
    }

    // Process exit and prompt start discard all live states; acknowledgement discards only terminal states.
    // Generate each state under each lifecycle operation, including repeated operations (idempotence).
    #[test]
    fn lifetime_and_acknowledgement() {
        for state in [ProgramState::Idle, ProgramState::Working, ProgramState::Blocked, ProgramState::Done, ProgramState::Error] {
            let mut store = ProgramStatusStore::default();
            store.replace(record("", state, 1));
            store.program_ended();
            store.program_ended();
            assert_eq!(store.records().len(), usize::from(matches!(state, ProgramState::Done | ProgramState::Error)));
            store.replace(record("", state, 2));
            store.acknowledge();
            store.acknowledge();
            assert_eq!(store.records().len(), usize::from(!matches!(state, ProgramState::Done | ProgramState::Error)));
        }
    }

    // Unchanged observations are omitted; record or ownership changes advance the atomic generation.
    // Prompt/exit resets ownership even when a retained done/error record is unchanged.
    // Generate replacement, clear, prompt/exit and acknowledgement sequences over every state.
    #[test]
    fn tracker_generations_and_read_predicates() {
        let tracker = ProgramStatusTracker::default();
        let (mut generation, records) = tracker.records_since(None).unwrap();
        assert!(records.is_empty());
        assert!(tracker.records_since(Some(generation)).is_none());
        for (n, state) in [ProgramState::Idle, ProgramState::Working, ProgramState::Blocked, ProgramState::Done, ProgramState::Error]
            .into_iter()
            .enumerate()
        {
            for operation in 0..4 {
                tracker.replace(record("task", state, n as u64));
                let (updated, records) = tracker.records_since(Some(generation)).unwrap();
                assert_ne!(updated, generation);
                generation = updated;
                assert_eq!(records, [record("task", state, n as u64)]);
                assert!(!tracker.is_empty());
                assert!(tracker.matches_states(&[state]));
                assert!(tracker.records_since(Some(generation)).is_none());
                tracker.clear("missing");
                assert!(tracker.records_since(Some(generation)).is_none());
                match operation {
                    0 => tracker.clear("task"),
                    1 => tracker.program_ended(),
                    2 => tracker.acknowledge(),
                    _ => tracker.clear(""),
                }
                let terminal = matches!(state, ProgramState::Done | ProgramState::Error);
                let changed = match operation {
                    1 => true,
                    2 => terminal,
                    _ => true,
                };
                let records = if changed {
                    let (updated, records) = tracker.records_since(Some(generation)).unwrap();
                    assert_ne!(updated, generation);
                    generation = updated;
                    records
                } else {
                    assert!(tracker.records_since(Some(generation)).is_none());
                    tracker.records()
                };
                assert_eq!(tracker.is_empty(), records.is_empty());
                assert_eq!(tracker.matches_states(&[state]), !records.is_empty());
                assert!(tracker.records_since(Some(generation)).is_none());
            }
        }
    }

    // Untrusted text loses bidi and invisible formatting while retaining visible Unicode.
    #[test]
    fn strips_formatting() {
        assert_eq!(strip_invisible_formatting("a\u{202e}b\u{2066}é\u{200b}\u{feff}\u{e0061}"), "abé");
    }
}

#[cfg(test)]
mod recognizer_tests {
    use super::*;
    fn record(source: ProgramStatusSource, state: ProgramState) -> ProgramStatusRecord {
        ProgramStatusRecord {
            id: String::new(),
            source,
            state,
            kind: None,
            progress: None,
            app: "test".into(),
            msg: String::new(),
            title: String::new(),
            since: 0,
        }
    }
    // Generated source/state pairs cover all vocabulary variants. Own reports
    // survive inferred updates and suppress fallback until a program boundary.
    #[test]
    fn own_reports_win_while_both_sources_remain_observable() {
        for source in [ProgramStatusSource::Recognizer, ProgramStatusSource::Title] {
            for state in [ProgramState::Idle, ProgramState::Working, ProgramState::Done, ProgramState::Blocked, ProgramState::Error] {
                let tracker = ProgramStatusTracker::default();
                tracker.replace(record(source, state));
                assert_eq!(tracker.effective_records(), [record(source, state)]);
                tracker.replace(record(ProgramStatusSource::Osc7501, ProgramState::Working));
                tracker.replace(record(source, state));
                assert_eq!(tracker.records().len(), 2);
                assert_eq!(tracker.effective_records(), [record(ProgramStatusSource::Osc7501, ProgramState::Working)]);
                tracker.clear("");
                tracker.mark_reported(); // An explicit OSC clear remains authoritative.
                tracker.replace(record(source, ProgramState::Working));
                assert!(tracker.effective_records().is_empty());
                tracker.program_ended();
                tracker.replace(record(source, state));
                assert_eq!(tracker.effective_records(), [record(source, state)]);
            }
        }
    }
    // Switching between title and screen updates one inferred root, and unchanged
    // role snapshots do not create a new subscription generation.
    #[test]
    fn inferred_root_and_role_generation_are_stable() {
        let tracker = ProgramStatusTracker::default();
        tracker.replace(record(ProgramStatusSource::Title, ProgramState::Working));
        tracker.replace(record(ProgramStatusSource::Recognizer, ProgramState::Idle));
        assert_eq!(tracker.records(), [record(ProgramStatusSource::Recognizer, ProgramState::Idle)]);
        let (generation, _, roles) = tracker.observations_since(None).unwrap();
        assert!(roles.is_empty());
        tracker.replace_roles(Vec::new());
        assert!(tracker.observations_since(Some(generation)).is_none());
        tracker.clear_inferred();
        assert!(tracker.records().is_empty());
        assert!(tracker.observations_since(Some(generation)).is_some());
    }
}
