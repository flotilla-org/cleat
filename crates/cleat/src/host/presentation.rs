//! Presentation gate for synchronized output (DEC mode 2026).
//!
//! A program brackets a repaint with begin/end synchronized update so its
//! terminal shows the finished frame, never an intermediate one. Transports
//! may split that repaint anywhere, so the engine can be observed mid-batch.
//! The engine keeps parsing and answering queries throughout; this gate sits
//! at the publication boundary and keeps consumers on the last completed
//! presentation until the batch ends.
//!
//! Behaviour, following Ghostty's application renderer:
//! - While held, incremental consumers receive the retained presentation
//!   (cursor, modes, scrollbar and image placements of the last published
//!   update) with no new generation, so no damage is acknowledged unseen.
//! - First frame: a consumer with no completed presentation to retain is
//!   rendered from current state; deferring would leave it with nothing.
//! - Resize and full reset end the batch inside the engine (Ghostty resets
//!   mode 2026 on both), so the next publication is immediate.
//! - An abandoned batch is ended after [`SYNCHRONIZED_OUTPUT_DEADLINE`] from
//!   when it was first observed, or as soon as the child exits. The host
//!   wakes for the deadline itself, without waiting for further output.

use std::time::{Duration, Instant};

use crate::provider::{DirtyState, TerminalRenderUpdate};

/// Recovery deadline for a batch that never ends; Ghostty uses the same.
pub(crate) const SYNCHRONIZED_OUTPUT_DEADLINE: Duration = Duration::from_millis(1000);

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum GateTransition {
    Unchanged,
    Held,
    Released,
}

#[derive(Debug, Default)]
pub(crate) struct PresentationGate {
    held_since: Option<Instant>,
    retained: Option<TerminalRenderUpdate>,
}

impl PresentationGate {
    pub(crate) fn is_held(&self) -> bool {
        self.held_since.is_some()
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.held_since.map(|since| since + SYNCHRONIZED_OUTPUT_DEADLINE)
    }

    pub(crate) fn expired(&self, now: Instant) -> bool {
        self.deadline().is_some_and(|deadline| now >= deadline)
    }

    /// Follow the engine's synchronized-output mode. The deadline runs from
    /// the first observation of a batch; a batch ended and restarted within
    /// one read is indistinguishable from a continuing one.
    pub(crate) fn reconcile(&mut self, synchronized: bool, now: Instant) -> GateTransition {
        match (self.held_since, synchronized) {
            (None, true) => {
                self.held_since = Some(now);
                GateTransition::Held
            }
            (Some(_), false) => {
                self.held_since = None;
                GateTransition::Released
            }
            _ => GateTransition::Unchanged,
        }
    }

    /// True when a publication must be withheld: a batch is open and a
    /// completed presentation exists to stand in for it.
    pub(crate) fn withholding(&self) -> bool {
        self.is_held() && self.retained.is_some()
    }

    /// The retained presentation to serve in place of a fresh render, if
    /// publication is withheld.
    pub(crate) fn withheld_update(&self) -> Option<TerminalRenderUpdate> {
        if self.is_held() {
            self.retained.clone()
        } else {
            None
        }
    }

    /// Publish a presentation: the retained one while withheld, otherwise a
    /// fresh `render`, which becomes the new retained presentation. `render`
    /// is not called while withheld, so engine damage stays pending.
    pub(crate) fn present(
        &mut self,
        render: impl FnOnce() -> Result<TerminalRenderUpdate, String>,
    ) -> Result<TerminalRenderUpdate, String> {
        if let Some(retained) = self.withheld_update() {
            return Ok(retained);
        }
        let update = render()?;
        self.retain(&update);
        Ok(update)
    }

    /// Record a published presentation. Only the frame-wide state is kept:
    /// consumers already hold the cells, and a retained update carries no ops.
    pub(crate) fn retain(&mut self, update: &TerminalRenderUpdate) {
        self.retained = Some(TerminalRenderUpdate {
            cols: update.cols,
            rows: update.rows,
            geometry: update.geometry,
            viewport_kind: update.viewport_kind,
            scrollback_offset_rows: update.scrollback_offset_rows,
            scrollbar: update.scrollbar,
            terminal_modes: update.terminal_modes,
            render_generation: update.render_generation,
            cursor: update.cursor,
            dirty: DirtyState::Clean,
            ops: Vec::new(),
            image_resources: update.image_resources.clone(),
            image_placements: update.image_placements.clone(),
        });
    }
}

/// Drive `gate` the way the session actor does: reconcile after output was
/// fed, then publish. For tests that stand an engine in for a session.
#[cfg(all(test, feature = "ghostty-vt"))]
pub(crate) fn publish_from_engine(
    gate: &mut PresentationGate,
    engine: &mut dyn crate::vt::VtEngine,
    dirty: DirtyState,
) -> TerminalRenderUpdate {
    gate.reconcile(engine.synchronized_output_active().unwrap(), Instant::now());
    gate.present(|| engine.render_update(dirty)).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_from_first_observation_until_the_batch_ends() {
        let start = Instant::now();
        let mut gate = PresentationGate::default();
        assert_eq!(gate.reconcile(false, start), GateTransition::Unchanged);
        assert_eq!(gate.reconcile(true, start), GateTransition::Held);
        // Continued output inside the batch does not extend the deadline.
        assert_eq!(gate.reconcile(true, start + Duration::from_millis(900)), GateTransition::Unchanged);
        assert_eq!(gate.deadline(), Some(start + SYNCHRONIZED_OUTPUT_DEADLINE));
        assert!(!gate.expired(start + Duration::from_millis(999)));
        assert!(gate.expired(start + SYNCHRONIZED_OUTPUT_DEADLINE));
        assert_eq!(gate.reconcile(false, start + Duration::from_millis(950)), GateTransition::Released);
        assert_eq!(gate.deadline(), None);
    }

    #[test]
    fn withholds_only_with_a_completed_presentation_to_retain() {
        let mut gate = PresentationGate::default();
        gate.reconcile(true, Instant::now());
        assert!(!gate.withholding(), "first frame has nothing to retain");
        assert!(gate.withheld_update().is_none());

        let mut update = TerminalRenderUpdate { render_generation: 4, dirty: DirtyState::Partial, ..Default::default() };
        update.cursor.visible = true;
        update.ops.push(Default::default());
        gate.retain(&update);
        let withheld = gate.withheld_update().expect("retained while held");
        assert_eq!(withheld.render_generation, 4);
        assert_eq!(withheld.dirty, DirtyState::Clean);
        assert!(withheld.ops.is_empty());
        assert!(withheld.cursor.visible);

        gate.reconcile(false, Instant::now());
        assert!(!gate.withholding());
        assert!(gate.withheld_update().is_none());
    }

    #[cfg(feature = "ghostty-vt")]
    mod ghostty {
        use super::super::*;
        use crate::vt::{ghostty::GhosttyVtEngine, VtEngine};

        fn text(update: &TerminalRenderUpdate) -> String {
            update
                .ops
                .iter()
                .flat_map(|op| &op.rows)
                .flat_map(|row| &row.cells)
                .flat_map(|cell| cell.graphemes.iter().copied())
                .filter_map(char::from_u32)
                .collect()
        }

        fn ready() -> (GhosttyVtEngine, PresentationGate) {
            let mut engine = GhosttyVtEngine::new(20, 3);
            let mut gate = PresentationGate::default();
            engine.feed(b"ready\x1b[?25h").unwrap();
            assert!(publish_from_engine(&mut gate, &mut engine, DirtyState::Full).cursor.visible);
            (engine, gate)
        }

        #[test]
        fn synchronized_repaint_does_not_publish_hidden_cursor_mid_batch() {
            let (mut engine, mut gate) = ready();
            let before = gate.retained.clone().unwrap();
            engine.feed(b"\x1b[?2026h\x1b[?25l").unwrap();
            let during = publish_from_engine(&mut gate, &mut engine, DirtyState::Partial);
            engine.feed(b"x\x1b[?25h\x1b[?2026l").unwrap();
            let after = publish_from_engine(&mut gate, &mut engine, DirtyState::Partial);
            eprintln!("cursor visible: before={} during={} after={}", before.cursor.visible, during.cursor.visible, after.cursor.visible);
            assert!(after.cursor.visible);
            assert!(during.cursor.visible, "published intermediate hidden cursor inside synchronized repaint");
            assert_eq!(during.render_generation, before.render_generation, "a withheld frame is not a new presentation");
        }

        #[test]
        fn cells_written_mid_batch_are_withheld_then_delivered_whole() {
            let (mut engine, mut gate) = ready();
            engine.feed(b"\x1b[?2026h\x1b[?25l\rhalf").unwrap();
            let during = publish_from_engine(&mut gate, &mut engine, DirtyState::Partial);
            assert!(during.ops.is_empty(), "mid-batch cells were published");
            assert_eq!(during.dirty, DirtyState::Clean);
            // The withheld render never consumed the engine's damage, so the
            // completed frame still carries the rows written before the split.
            engine.feed(b"-done\x1b[?25h\x1b[?2026l").unwrap();
            let after = publish_from_engine(&mut gate, &mut engine, DirtyState::Partial);
            assert!(text(&after).contains("half-done"), "completed frame lost mid-batch damage: {:?}", text(&after));
            assert!(after.cursor.visible);
        }

        #[test]
        fn whole_batch_in_one_delivery_publishes_the_completed_frame() {
            let (mut engine, mut gate) = ready();
            engine.feed(b"\x1b[?2026h\x1b[?25l\rwhole\x1b[?25h\x1b[?2026l").unwrap();
            let update = publish_from_engine(&mut gate, &mut engine, DirtyState::Partial);
            assert!(!gate.is_held());
            assert!(update.cursor.visible);
            assert!(text(&update).contains("whole"));
        }

        #[test]
        fn unsynchronized_cursor_hiding_is_published() {
            let (mut engine, mut gate) = ready();
            engine.feed(b"\x1b[?25l").unwrap();
            assert!(!publish_from_engine(&mut gate, &mut engine, DirtyState::Partial).cursor.visible);
        }

        #[test]
        fn first_frame_mid_batch_renders_current_state() {
            let mut engine = GhosttyVtEngine::new(20, 3);
            let mut gate = PresentationGate::default();
            engine.feed(b"\x1b[?2026hfirst").unwrap();
            let update = publish_from_engine(&mut gate, &mut engine, DirtyState::Full);
            assert!(gate.is_held());
            assert!(text(&update).contains("first"));
        }

        #[test]
        fn resize_ends_the_batch() {
            let (mut engine, mut gate) = ready();
            engine.feed(b"\x1b[?2026h\x1b[?25l").unwrap();
            publish_from_engine(&mut gate, &mut engine, DirtyState::Partial);
            assert!(gate.is_held());
            engine.resize(24, 4).unwrap();
            let update = publish_from_engine(&mut gate, &mut engine, DirtyState::Full);
            assert!(!gate.is_held());
            assert_eq!((update.cols, update.rows), (24, 4));
        }

        #[test]
        fn ending_an_abandoned_batch_releases_the_engine_mode() {
            let (mut engine, mut gate) = ready();
            engine.feed(b"\x1b[?2026h").unwrap();
            publish_from_engine(&mut gate, &mut engine, DirtyState::Partial);
            engine.end_synchronized_output().unwrap();
            assert!(!engine.synchronized_output_active().unwrap());
            publish_from_engine(&mut gate, &mut engine, DirtyState::Partial);
            assert!(!gate.is_held());
            // A later batch is recognized afresh.
            engine.feed(b"\x1b[?2026h").unwrap();
            assert!(engine.synchronized_output_active().unwrap());
        }
    }
}
