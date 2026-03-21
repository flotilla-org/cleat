use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::Path,
};

const OUTPUT_LOG_NAME: &str = "output.log";

pub struct OutputRecorder {
    log_file: File,
    bytes_written: u64,
}

impl OutputRecorder {
    pub fn new(session_dir: &Path) -> Result<Self, String> {
        let log_path = session_dir.join(OUTPUT_LOG_NAME);
        let log_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|err| format!("open output log {}: {err}", log_path.display()))?;

        let bytes_written = log_file.metadata().map(|m| m.len()).unwrap_or(0);

        Ok(Self { log_file, bytes_written })
    }

    pub fn record(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.log_file.write_all(bytes).map_err(|err| format!("write output log: {err}"))?;
        self.bytes_written += bytes.len() as u64;
        Ok(())
    }

    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }
}
