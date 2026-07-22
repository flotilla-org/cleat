//! Canonical screen-activity domain model.
//!
//! Activity is driven only by output that changes the rendered screen. Engines
//! that cannot observe render changes leave the timeline untouched and
//! therefore report a stable screen. JSON consumers project the timeline with
//! [`JSON_SCREEN_ACTIVITY_STABLE_AFTER`]; packet subscribers project the same
//! timeline with their requested threshold.

use std::{
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

pub(crate) const JSON_SCREEN_ACTIVITY_STABLE_AFTER: Duration = Duration::from_secs(1);

/// Whether a session's rendered screen is changing within a stability window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScreenActivity {
    Active,
    #[default]
    Stable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScreenActivitySnapshot {
    pub(crate) screen_activity: ScreenActivity,
    /// Unix timestamp when the screen actually entered the stable state.
    pub(crate) stable_since: Option<u64>,
    /// Unix timestamp at the beginning of the current quiet window.
    pub(crate) quiet_since: u64,
    /// Unix timestamp of the most recent render-changing output.
    pub(crate) last_output_at: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ScreenActivityTime {
    instant: Instant,
    unix_ms: u64,
}

impl ScreenActivityTime {
    pub(crate) fn new(instant: Instant, unix_ms: u64) -> Self {
        Self { instant, unix_ms }
    }
}

#[derive(Clone, Copy, Debug)]
struct ScreenActivityTimeline {
    initial_stable_since: u64,
    last_render_change: Option<ScreenActivityTime>,
}

/// Shared render-change timeline used by JSON polling and packet subscriptions.
#[derive(Clone, Debug)]
pub(crate) struct ScreenActivityTracker {
    timeline: Arc<RwLock<ScreenActivityTimeline>>,
}

impl ScreenActivityTracker {
    pub(crate) fn new(started_at_unix_ms: u64) -> Self {
        Self {
            timeline: Arc::new(RwLock::new(ScreenActivityTimeline { initial_stable_since: started_at_unix_ms, last_render_change: None })),
        }
    }

    pub(crate) fn render_changed(&self, changed_at: ScreenActivityTime) {
        self.timeline.write().unwrap_or_else(|poisoned| poisoned.into_inner()).last_render_change = Some(changed_at);
    }

    pub(crate) fn json_snapshot(&self, now: Instant) -> ScreenActivitySnapshot {
        self.snapshot(now, JSON_SCREEN_ACTIVITY_STABLE_AFTER)
    }

    pub(crate) fn snapshot(&self, now: Instant, stable_after: Duration) -> ScreenActivitySnapshot {
        let timeline = *self.timeline.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(changed_at) = timeline.last_render_change else {
            return ScreenActivitySnapshot {
                screen_activity: ScreenActivity::Stable,
                stable_since: Some(timeline.initial_stable_since),
                quiet_since: timeline.initial_stable_since,
                last_output_at: None,
            };
        };

        if now.saturating_duration_since(changed_at.instant) < stable_after {
            ScreenActivitySnapshot {
                screen_activity: ScreenActivity::Active,
                stable_since: None,
                quiet_since: changed_at.unix_ms,
                last_output_at: Some(changed_at.unix_ms),
            }
        } else {
            ScreenActivitySnapshot {
                screen_activity: ScreenActivity::Stable,
                stable_since: Some(changed_at.unix_ms.saturating_add(duration_millis(stable_after))),
                quiet_since: changed_at.unix_ms,
                last_output_at: Some(changed_at.unix_ms),
            }
        }
    }
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{ScreenActivity, ScreenActivityTime, ScreenActivityTracker, JSON_SCREEN_ACTIVITY_STABLE_AFTER};

    fn activity_time(instant: Instant, unix_ms: u64) -> ScreenActivityTime {
        ScreenActivityTime::new(instant, unix_ms)
    }

    #[test]
    fn render_change_decays_from_active_to_stable_after_one_second() {
        let started_at = Instant::now();
        let tracker = ScreenActivityTracker::new(1_000);

        let initial = tracker.snapshot(started_at, JSON_SCREEN_ACTIVITY_STABLE_AFTER);
        assert_eq!(initial.screen_activity, ScreenActivity::Stable);
        assert_eq!(initial.stable_since, Some(1_000));
        assert_eq!(initial.quiet_since, 1_000);
        assert_eq!(initial.last_output_at, None);

        tracker.render_changed(activity_time(started_at + Duration::from_millis(100), 1_100));

        let active = tracker.snapshot(started_at + Duration::from_millis(1_099), JSON_SCREEN_ACTIVITY_STABLE_AFTER);
        assert_eq!(active.screen_activity, ScreenActivity::Active);
        assert_eq!(active.stable_since, None);
        assert_eq!(active.quiet_since, 1_100);
        assert_eq!(active.last_output_at, Some(1_100));

        let stable = tracker.snapshot(started_at + Duration::from_millis(1_100), JSON_SCREEN_ACTIVITY_STABLE_AFTER);
        assert_eq!(stable.screen_activity, ScreenActivity::Stable);
        assert_eq!(stable.stable_since, Some(2_100));
        assert_eq!(stable.quiet_since, 1_100);
        assert_eq!(stable.last_output_at, Some(1_100));
    }

    #[test]
    fn later_render_change_starts_a_new_active_period() {
        let started_at = Instant::now();
        let tracker = ScreenActivityTracker::new(5_000);
        tracker.render_changed(activity_time(started_at + Duration::from_secs(2), 7_000));

        let active = tracker.snapshot(started_at + Duration::from_millis(2_500), JSON_SCREEN_ACTIVITY_STABLE_AFTER);

        assert_eq!(active.screen_activity, ScreenActivity::Active);
        assert_eq!(active.stable_since, None);
        assert_eq!(active.last_output_at, Some(7_000));
    }

    #[test]
    fn json_and_subscription_project_the_same_render_change_with_distinct_thresholds() {
        let started_at = Instant::now();
        let tracker = ScreenActivityTracker::new(1_000);
        tracker.render_changed(activity_time(started_at + Duration::from_millis(100), 1_100));

        let observed_at = started_at + Duration::from_millis(600);
        let subscription = tracker.snapshot(observed_at, Duration::from_millis(400));
        let json = tracker.json_snapshot(observed_at);

        assert_eq!(subscription.screen_activity, ScreenActivity::Stable);
        assert_eq!(subscription.stable_since, Some(1_500));
        assert_eq!(subscription.quiet_since, 1_100);
        assert_eq!(json.screen_activity, ScreenActivity::Active);
        assert_eq!(json.stable_since, None);
        assert_eq!(json.quiet_since, 1_100);
        assert_eq!(subscription.last_output_at, json.last_output_at);
    }
}
