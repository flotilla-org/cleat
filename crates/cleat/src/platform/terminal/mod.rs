#[cfg(unix)]
mod unix;
#[cfg(not(unix))]
mod unsupported;

#[cfg(unix)]
pub use unix::*;
#[cfg(not(unix))]
pub use unsupported::*;

pub fn current_terminal_size() -> (u16, u16) {
    if let Some(size) = os_terminal_size() {
        return size;
    }
    size_from_env(std::env::var("COLUMNS").ok().as_deref(), std::env::var("LINES").ok().as_deref())
}

fn size_from_env(columns: Option<&str>, lines: Option<&str>) -> (u16, u16) {
    let cols = columns.and_then(|value| value.parse::<u16>().ok()).unwrap_or(80);
    let rows = lines.and_then(|value| value.parse::<u16>().ok()).unwrap_or(24);
    (cols, rows)
}

#[cfg(test)]
mod tests {
    #[test]
    fn size_from_env_falls_back_to_defaults_for_missing_or_invalid_values() {
        assert_eq!(super::size_from_env(None, None), (80, 24));
        assert_eq!(super::size_from_env(Some("not-a-number"), Some("also-bad")), (80, 24));
    }

    #[test]
    fn size_from_env_uses_valid_columns_and_lines() {
        assert_eq!(super::size_from_env(Some("132"), Some("43")), (132, 43));
    }
}
