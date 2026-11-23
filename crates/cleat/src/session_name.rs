//! Session names: any text a client chooses, mapped to the two places cleat
//! needs a restricted form of it.
//!
//! A session name is any non-empty text without NUL. Clients use it as given
//! for create, attach, send, kill and list; `list` reports it unchanged.
//!
//! - On disk a session lives in `sessions/<directory>`. A name made only of
//!   `[A-Za-z0-9._-]` (other than `.` and `..`) is its own directory, which is
//!   how every session was stored before arbitrary names. Any other name is
//!   stored as `~` followed by its UTF-8 bytes, each byte outside
//!   `[A-Za-z0-9_-]` written `%XX`. No plain name contains `~`, so the two
//!   forms never collide, and each directory maps back to one name.
//! - In the daemon's HTTP paths a name is one percent-encoded path segment.

const ESCAPED_PREFIX: char = '~';
/// The longest directory name a session may need: `NAME_MAX` on Linux and
/// macOS, and the per-component limit on Windows.
const MAX_DIRECTORY_BYTES: usize = 255;

/// Check a session name: non-empty, no NUL, and short enough that its
/// directory fits one path component.
pub fn validate_session_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("session name must not be empty".to_string());
    }
    if name.contains('\0') {
        return Err("session name must not contain NUL".to_string());
    }
    let directory = directory_for(name);
    if directory.len() > MAX_DIRECTORY_BYTES {
        return Err(format!(
            "session name is too long: it needs a {}-byte directory name, over the {MAX_DIRECTORY_BYTES}-byte limit",
            directory.len()
        ));
    }
    Ok(())
}

fn is_plain(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The directory under `sessions/` that holds the session called `name`.
pub fn directory_for(name: &str) -> String {
    if is_plain(name) {
        return name.to_string();
    }
    let mut out = String::with_capacity(name.len() + 1);
    out.push(ESCAPED_PREFIX);
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The session name a directory under `sessions/` holds, or `None` for an
/// entry that is not a session directory in either form.
pub fn name_from_directory(directory: &str) -> Option<String> {
    let Some(escaped) = directory.strip_prefix(ESCAPED_PREFIX) else {
        return is_plain(directory).then(|| directory.to_string());
    };
    let name = String::from_utf8(percent_decode(escaped)?).ok()?;
    // Only the canonical spelling names a session, so one name has one directory.
    (directory_for(&name) == directory && validate_session_name(&name).is_ok()).then_some(name)
}

/// `name` as one HTTP path segment.
pub fn path_segment(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The session name an HTTP path segment carries.
pub fn name_from_path_segment(segment: &str) -> Option<String> {
    String::from_utf8(percent_decode(segment)?).ok()
}

fn percent_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_names_are_their_own_directory() {
        for name in ["alpha", "terminal-convoy-abc-work.2312007-coder", "a.b_c-d", "session-1"] {
            assert_eq!(directory_for(name), name);
            assert_eq!(name_from_directory(name).as_deref(), Some(name));
        }
    }

    #[test]
    fn other_names_round_trip_through_an_escaped_directory() {
        for name in [
            "terminal-convoy-abc-work#12007-coder",
            "project:katzensteg/convoy:governor-govern#7673-governor",
            ".",
            "..",
            "with space",
            "ünïcode",
            "~tilde",
            "%41",
        ] {
            let directory = directory_for(name);
            assert!(directory.starts_with('~'), "{name} -> {directory}");
            assert!(!directory.contains('/') && !directory.contains('\\') && !directory.contains(':'), "{directory}");
            assert_eq!(name_from_directory(&directory).as_deref(), Some(name));
        }
    }

    #[test]
    fn non_canonical_and_foreign_directories_name_no_session() {
        // `~a` spells the plain name `a`, which lives in `a`.
        assert_eq!(name_from_directory("~a"), None);
        // Lowercase hex is not the canonical spelling.
        assert_eq!(name_from_directory("~a%2fb"), None);
        assert_eq!(name_from_directory("~%ZZ"), None);
        assert_eq!(name_from_directory("~%FF"), None);
        assert_eq!(name_from_directory("."), None);
        assert_eq!(name_from_directory("with space"), None);
    }

    #[test]
    fn validation_accepts_any_text_but_empty_nul_or_too_long() {
        assert!(validate_session_name("terminal-convoy-abc-work#12007-coder").is_ok());
        assert!(validate_session_name("a/b:c d").is_ok());
        assert!(validate_session_name("").is_err());
        assert!(validate_session_name("a\0b").is_err());
        assert!(validate_session_name(&"a".repeat(255)).is_ok());
        assert!(validate_session_name(&"a".repeat(256)).is_err());
        assert!(validate_session_name(&"#".repeat(85)).is_err());
    }

    #[test]
    fn path_segments_round_trip() {
        for name in ["alpha", "work#1/coder", "a b?c%d", "ünïcode"] {
            let segment = path_segment(name);
            assert!(!segment.contains('/') && !segment.contains('#') && !segment.contains('?'), "{segment}");
            assert_eq!(name_from_path_segment(&segment).as_deref(), Some(name));
        }
    }
}
