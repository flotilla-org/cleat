//! `cleat replay`: play back cast files (or slices) at controlled speed.
//!
//! Pure timing logic and the playback loop live here. The CLI dispatch and
//! bound resolution are in [`crate::cli`] and [`crate::server`] respectively.

use std::{io::Write, time::Duration};

use crate::asciicast::Event;

/// Options that shape playback pacing and output.
#[derive(Debug, Clone)]
pub struct ReplayOptions {
    /// Event-gap multiplier. Must be positive and finite.
    pub speed: f64,
    /// If set, clamp any inter-event gap to this maximum after speed scaling.
    pub max_idle: Option<Duration>,
}

impl Default for ReplayOptions {
    fn default() -> Self {
        Self { speed: 1.0, max_idle: None }
    }
}

/// Compute the sleep duration before the next event given the raw inter-event
/// gap and the replay options.
pub fn sleep_for_gap(gap: Duration, opts: &ReplayOptions) -> Duration {
    let scaled = Duration::from_secs_f64(gap.as_secs_f64() / opts.speed);
    match opts.max_idle {
        Some(clamp) => scaled.min(clamp),
        None => scaled,
    }
}

/// Play an iterator of Output events to `writer`, sleeping by the scaled,
/// optionally-clamped gap between events.
///
/// `sleeper` is injected so unit tests can assert the requested sleep
/// durations without actually blocking.
pub fn play<W, S, I>(events: I, opts: &ReplayOptions, writer: &mut W, mut sleeper: S) -> Result<(), String>
where
    W: Write,
    S: FnMut(Duration),
    I: Iterator<Item = Result<Event, String>>,
{
    let mut prev_time = Duration::ZERO;
    for event in events {
        let event = event?;
        let gap = event.time.saturating_sub(prev_time);
        let sleep = sleep_for_gap(gap, opts);
        sleeper(sleep);
        match writer.write_all(event.data.as_bytes()) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => return Ok(()),
            Err(err) => return Err(format!("write output: {err}")),
        }
        match writer.flush() {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => return Ok(()),
            Err(err) => return Err(format!("flush output: {err}")),
        }
        prev_time = event.time;
    }
    Ok(())
}

/// Validate the speed value from clap. Called by the CLI value parser.
pub fn parse_speed(s: &str) -> Result<f64, String> {
    let f: f64 = s.parse().map_err(|_| format!("invalid speed: {s}"))?;
    if !f.is_finite() || f <= 0.0 {
        return Err(format!("invalid speed: {s}"));
    }
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asciicast::EventCode;

    #[test]
    fn sleep_for_gap_default_is_identity() {
        let opts = ReplayOptions::default();
        assert_eq!(sleep_for_gap(Duration::from_millis(500), &opts), Duration::from_millis(500));
    }

    #[test]
    fn sleep_for_gap_speed_2_halves_gap() {
        let opts = ReplayOptions { speed: 2.0, max_idle: None };
        assert_eq!(sleep_for_gap(Duration::from_millis(500), &opts), Duration::from_millis(250));
    }

    #[test]
    fn sleep_for_gap_speed_half_doubles_gap() {
        let opts = ReplayOptions { speed: 0.5, max_idle: None };
        assert_eq!(sleep_for_gap(Duration::from_millis(500), &opts), Duration::from_millis(1000));
    }

    #[test]
    fn sleep_for_gap_max_idle_clamps() {
        let opts = ReplayOptions { speed: 1.0, max_idle: Some(Duration::from_millis(100)) };
        assert_eq!(sleep_for_gap(Duration::from_millis(500), &opts), Duration::from_millis(100));
    }

    #[test]
    fn sleep_for_gap_max_idle_does_not_expand_below_clamp() {
        let opts = ReplayOptions { speed: 1.0, max_idle: Some(Duration::from_millis(100)) };
        assert_eq!(sleep_for_gap(Duration::from_millis(50), &opts), Duration::from_millis(50));
    }

    #[test]
    fn parse_speed_accepts_positive_finite() {
        assert_eq!(parse_speed("1.0").unwrap(), 1.0);
        assert_eq!(parse_speed("0.5").unwrap(), 0.5);
        assert_eq!(parse_speed("1000").unwrap(), 1000.0);
    }

    #[test]
    fn parse_speed_rejects_zero_and_negative_and_nan_and_inf() {
        assert!(parse_speed("0").is_err());
        assert!(parse_speed("-1").is_err());
        assert!(parse_speed("NaN").is_err());
        assert!(parse_speed("inf").is_err());
    }

    #[test]
    fn play_writes_events_with_scaled_sleeps() {
        let events = vec![
            Ok(Event { time: Duration::from_millis(100), code: EventCode::Output, data: "hello ".into() }),
            Ok(Event { time: Duration::from_millis(300), code: EventCode::Output, data: "world".into() }),
        ];
        let opts = ReplayOptions { speed: 2.0, max_idle: None };
        let mut buf: Vec<u8> = Vec::new();
        let mut sleeps: Vec<Duration> = Vec::new();
        play(events.into_iter(), &opts, &mut buf, |d| sleeps.push(d)).expect("play");
        assert_eq!(buf, b"hello world");
        // Gap 1: 100ms / 2 = 50ms. Gap 2: (300-100)ms / 2 = 100ms.
        assert_eq!(sleeps, vec![Duration::from_millis(50), Duration::from_millis(100)]);
    }

    #[test]
    fn play_propagates_iterator_errors() {
        let events =
            vec![Ok(Event { time: Duration::from_millis(100), code: EventCode::Output, data: "a".into() }), Err("bad event".to_string())];
        let opts = ReplayOptions::default();
        let mut buf: Vec<u8> = Vec::new();
        let result = play(events.into_iter(), &opts, &mut buf, |_| {});
        assert_eq!(result, Err("bad event".to_string()));
        assert_eq!(buf, b"a");
    }

    #[test]
    fn play_exits_cleanly_on_broken_pipe() {
        use std::io;

        struct BrokenPipeWriter;
        impl Write for BrokenPipeWriter {
            fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let events = vec![Ok(Event { time: Duration::from_millis(100), code: EventCode::Output, data: "x".into() })];
        let opts = ReplayOptions::default();
        let mut w = BrokenPipeWriter;
        let result = play(events.into_iter(), &opts, &mut w, |_| {});
        assert_eq!(result, Ok(()));
    }
}
