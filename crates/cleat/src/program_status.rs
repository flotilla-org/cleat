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
}

impl ProgramStatusStore {
    pub fn records(&self) -> &[ProgramStatusRecord] {
        &self.records
    }

    pub fn replace(&mut self, mut record: ProgramStatusRecord) {
        record.msg = strip_invisible_formatting(&record.msg);
        record.title = strip_invisible_formatting(&record.title);
        self.records.retain(|old| old.id != record.id);
        // Insertion order breaks timestamp ties and remains stable if the wall clock moves.
        if self.records.len() == 256 {
            self.records.remove(0);
        }
        self.records.push(record);
    }

    pub fn clear(&mut self, id: &str) {
        self.records
            .retain(|record| !id.is_empty() && record.id != id && !record.id.strip_prefix(id).is_some_and(|rest| rest.starts_with('/')));
    }

    pub fn program_ended(&mut self) {
        self.records.retain(|record| matches!(record.state, ProgramState::Done | ProgramState::Error));
    }

    pub fn acknowledge(&mut self) {
        self.records.retain(|record| !matches!(record.state, ProgramState::Done | ProgramState::Error));
    }
}

/// Shared observation of the actor-owned store; readers never send actor commands.
#[derive(Clone, Default)]
pub struct ProgramStatusTracker(std::sync::Arc<std::sync::Mutex<ProgramStatusStore>>);
impl ProgramStatusTracker {
    fn with_store<T>(&self, f: impl FnOnce(&mut ProgramStatusStore) -> T) -> T {
        f(&mut self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()))
    }
    pub fn records(&self) -> Vec<ProgramStatusRecord> {
        self.with_store(|store| store.records().to_vec())
    }
    pub fn replace(&self, record: ProgramStatusRecord) {
        self.with_store(|store| store.replace(record));
    }
    pub fn clear(&self, id: &str) {
        self.with_store(|store| store.clear(id));
    }
    pub fn program_ended(&self) {
        self.with_store(ProgramStatusStore::program_ended);
    }
    pub fn acknowledge(&self) {
        self.with_store(ProgramStatusStore::acknowledge);
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
    // Generate counts crossing the 256 boundary, replacements and timestamp ties.
    #[test]
    fn replacement_and_capacity() {
        let mut store = ProgramStatusStore::default();
        for n in 0..258 {
            store.replace(record(&n.to_string(), ProgramState::Working, n));
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

    // Untrusted text loses bidi and invisible formatting while retaining visible Unicode.
    #[test]
    fn strips_formatting() {
        assert_eq!(strip_invisible_formatting("a\u{202e}b\u{2066}é\u{200b}\u{feff}\u{e0061}"), "abé");
    }
}
