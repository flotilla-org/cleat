//! Session hosting epoch storage. Callers must serialize increments under their
//! exclusive session authority; this is storage, not an ownership election.
use std::{fs, io, path::Path};
pub const EPOCH_FILE_NAME: &str = "epoch";

pub fn read(session_dir: &Path) -> io::Result<u64> {
    let text = match fs::read_to_string(session_dir.join(EPOCH_FILE_NAME)) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(1),
        Err(err) => return Err(err),
    };
    text.trim()
        .parse::<u64>()
        .ok()
        .filter(|epoch| *epoch > 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid hosting epoch"))
}

/// Write and sync a temporary file, then atomically replace epoch. A failed
/// write leaves the old epoch intact. The session directory must already exist.
pub fn increment(session_dir: &Path) -> io::Result<u64> {
    use std::io::Write;
    let next = read(session_dir)?.checked_add(1).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "hosting epoch overflow"))?;
    let temporary = session_dir.join(format!(".epoch-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&temporary)?;
        writeln!(file, "{next}")?;
        file.sync_all()?;
        fs::rename(&temporary, session_dir.join(EPOCH_FILE_NAME))?;
        fs::File::open(session_dir)?.sync_all()?;
        Ok(next)
    })();
    let _ = fs::remove_file(temporary);
    result
}
