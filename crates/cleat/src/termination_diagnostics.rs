//! Daemon-owned escalation diagnostics, independent of actor/recorder lifetime.
use std::{
    fs::OpenOptions,
    io::Write,
    path::Path,
    sync::mpsc::{self, Sender},
    thread::{self, JoinHandle},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::platform::signals::SignalDelivery;

#[derive(serde::Serialize)]
struct Escalation {
    event: &'static str,
    unix_ms: u128,
    session_id: String,
    signal: &'static str,
    #[serde(flatten)]
    delivery: SignalDelivery,
}

pub(crate) struct TerminationDiagnostics {
    sender: Option<Sender<Escalation>>,
    worker: Option<JoinHandle<()>>,
}

impl TerminationDiagnostics {
    pub(crate) fn open(daemon_dir: &Path) -> Result<Self, String> {
        let path = daemon_dir.join("termination.jsonl");
        let mut file = OpenOptions::new().create(true).append(true).open(&path).map_err(|err| format!("open {}: {err}", path.display()))?;
        // Rare control-plane events use an unbounded channel: neither a slow
        // disk nor a full queue may block servicing or silently drop a record.
        let (sender, receiver) = mpsc::channel::<Escalation>();
        let worker = thread::Builder::new()
            .name("termination-diagnostics".into())
            .spawn(move || {
                for event in receiver {
                    let result = (|| -> Result<(), String> {
                        let mut line = serde_json::to_vec(&event).map_err(|err| err.to_string())?;
                        line.push(b'\n');
                        file.write_all(&line).map_err(|err| err.to_string())?;
                        file.sync_data().map_err(|err| err.to_string())
                    })();
                    if let Err(err) = result {
                        eprintln!("cleat: write termination diagnostics {}: {err}", path.display());
                    }
                }
            })
            .map_err(|err| format!("start termination diagnostics: {err}"))?;
        Ok(Self { sender: Some(sender), worker: Some(worker) })
    }

    pub(crate) fn record(&self, session_id: &str, delivery: SignalDelivery) {
        let event = Escalation {
            event: "termination_escalation",
            unix_ms: SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis(),
            session_id: session_id.to_owned(),
            signal: "KILL",
            delivery,
        };
        if self.sender.as_ref().expect("diagnostics sender").send(event).is_err() {
            eprintln!("cleat: termination diagnostics worker disconnected");
        }
    }
}

impl Drop for TerminationDiagnostics {
    fn drop(&mut self) {
        // Drain on orderly daemon shutdown, never during request servicing.
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_drains_and_reopen_appends_success_empty_and_failure_reports() {
        let dir = tempfile::tempdir().unwrap();
        for delivery in [SignalDelivery { delivered: vec![123], errors: vec![] }, SignalDelivery::default(), SignalDelivery {
            delivered: vec![456],
            errors: vec!["kill 789: EPERM".into()],
        }] {
            let diagnostics = TerminationDiagnostics::open(dir.path()).unwrap();
            diagnostics.record("retired-session", delivery);
        }
        let text = std::fs::read_to_string(dir.path().join("termination.jsonl")).unwrap();
        let records: Vec<serde_json::Value> = text.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(records.len(), 3);
        for record in &records {
            assert_eq!(record["event"], "termination_escalation");
            assert_eq!(record["session_id"], "retired-session");
            assert_eq!(record["signal"], "KILL");
            assert!(record["unix_ms"].as_u64().unwrap() > 0);
        }
        assert_eq!(records[0]["delivered"], serde_json::json!([123]));
        assert_eq!(records[1]["delivered"], serde_json::json!([]));
        assert_eq!(records[2]["errors"], serde_json::json!(["kill 789: EPERM"]));
    }
}
