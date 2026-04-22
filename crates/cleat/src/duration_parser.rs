//! Duration parser accepting both humantime-suffixed strings and plain
//! numeric seconds. Used by `--until-idle` on `transcript` and
//! `--idle-time` on `wait`.

use std::time::Duration;

/// Parse a duration string. Accepts:
/// - humantime forms: `500ms`, `2s`, `1m30s`, `250us`, etc.
/// - plain float seconds: `2`, `0.5`, `10.25`.
///
/// Humantime is tried first; falls back to float parsing on failure.
pub fn parse_humantime_or_seconds(s: &str) -> Result<Duration, String> {
    if let Ok(d) = humantime::parse_duration(s) {
        return Ok(d);
    }
    // `Duration::from_secs_f64` panics on negative / NaN / infinite values, so
    // validate the float before converting.
    let f: f64 = s.parse().map_err(|_| format!("invalid duration: {s}"))?;
    if !f.is_finite() || f < 0.0 {
        return Err(format!("invalid duration: {s}"));
    }
    Ok(Duration::from_secs_f64(f))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_humantime_milliseconds() {
        assert_eq!(parse_humantime_or_seconds("500ms").unwrap(), Duration::from_millis(500));
    }

    #[test]
    fn accepts_humantime_seconds() {
        assert_eq!(parse_humantime_or_seconds("2s").unwrap(), Duration::from_secs(2));
    }

    #[test]
    fn accepts_humantime_compound() {
        assert_eq!(parse_humantime_or_seconds("1m30s").unwrap(), Duration::from_secs(90));
    }

    #[test]
    fn accepts_plain_integer_seconds() {
        assert_eq!(parse_humantime_or_seconds("2").unwrap(), Duration::from_secs(2));
    }

    #[test]
    fn accepts_plain_float_seconds() {
        assert_eq!(parse_humantime_or_seconds("0.5").unwrap(), Duration::from_millis(500));
    }

    #[test]
    fn rejects_invalid_input() {
        let err = parse_humantime_or_seconds("not a duration").unwrap_err();
        assert!(err.contains("invalid duration"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_empty_string() {
        assert!(parse_humantime_or_seconds("").is_err());
    }

    #[test]
    fn rejects_negative_seconds_without_panic() {
        let err = parse_humantime_or_seconds("-1").unwrap_err();
        assert!(err.contains("invalid duration"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_nan_and_infinity_without_panic() {
        assert!(parse_humantime_or_seconds("NaN").is_err());
        assert!(parse_humantime_or_seconds("inf").is_err());
    }
}
