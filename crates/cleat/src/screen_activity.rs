use std::time::{Duration, Instant};

use crate::protocol::ScreenActivity;

pub(crate) const SCREEN_ACTIVITY_STABLE_AFTER: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScreenActivitySnapshot {
    pub(crate) screen_activity: ScreenActivity,
    pub(crate) stable_since: Option<u64>,
    pub(crate) last_output_at: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ScreenActivityTracker {
    initial_stable_since: u64,
    last_render_change: Option<(Instant, u64)>,
}

impl ScreenActivityTracker {
    pub(crate) fn new(started_at_unix_ms: u64) -> Self {
        Self { initial_stable_since: started_at_unix_ms, last_render_change: None }
    }

    pub(crate) fn render_changed(&mut self, changed_at: Instant, changed_at_unix_ms: u64) {
        self.last_render_change = Some((changed_at, changed_at_unix_ms));
    }

    pub(crate) fn snapshot(&self, now: Instant) -> ScreenActivitySnapshot {
        let Some((changed_at, changed_at_unix_ms)) = self.last_render_change else {
            return ScreenActivitySnapshot {
                screen_activity: ScreenActivity::Stable,
                stable_since: Some(self.initial_stable_since),
                last_output_at: None,
            };
        };

        if now.saturating_duration_since(changed_at) < SCREEN_ACTIVITY_STABLE_AFTER {
            ScreenActivitySnapshot { screen_activity: ScreenActivity::Active, stable_since: None, last_output_at: Some(changed_at_unix_ms) }
        } else {
            ScreenActivitySnapshot {
                screen_activity: ScreenActivity::Stable,
                stable_since: Some(changed_at_unix_ms.saturating_add(SCREEN_ACTIVITY_STABLE_AFTER.as_millis() as u64)),
                last_output_at: Some(changed_at_unix_ms),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::ScreenActivityTracker;
    use crate::protocol::ScreenActivity;

    #[test]
    fn render_change_decays_from_active_to_stable_after_one_second() {
        let started_at = Instant::now();
        let mut tracker = ScreenActivityTracker::new(1_000);

        assert_eq!(tracker.snapshot(started_at).screen_activity, ScreenActivity::Stable);
        assert_eq!(tracker.snapshot(started_at).stable_since, Some(1_000));
        assert_eq!(tracker.snapshot(started_at).last_output_at, None);

        tracker.render_changed(started_at + Duration::from_millis(100), 1_100);

        let active = tracker.snapshot(started_at + Duration::from_millis(1_099));
        assert_eq!(active.screen_activity, ScreenActivity::Active);
        assert_eq!(active.stable_since, None);
        assert_eq!(active.last_output_at, Some(1_100));

        let stable = tracker.snapshot(started_at + Duration::from_millis(1_100));
        assert_eq!(stable.screen_activity, ScreenActivity::Stable);
        assert_eq!(stable.stable_since, Some(2_100));
        assert_eq!(stable.last_output_at, Some(1_100));
    }

    #[test]
    fn later_render_change_starts_a_new_active_period() {
        let started_at = Instant::now();
        let mut tracker = ScreenActivityTracker::new(5_000);
        tracker.render_changed(started_at + Duration::from_secs(2), 7_000);

        let active = tracker.snapshot(started_at + Duration::from_millis(2_500));

        assert_eq!(active.screen_activity, ScreenActivity::Active);
        assert_eq!(active.stable_since, None);
        assert_eq!(active.last_output_at, Some(7_000));
    }
}
