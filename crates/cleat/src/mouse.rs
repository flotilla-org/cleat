//! Per-source button ownership, independent of transport and terminal encoding.
use std::collections::BTreeMap;

use crate::{
    host::actor::SessionMouseEvent,
    vt::{MouseAction, MouseButton},
};

#[derive(Default)]
pub(crate) struct HeldButtons {
    sources: BTreeMap<u128, Vec<SessionMouseEvent>>,
}

impl HeldButtons {
    pub fn sources(&self) -> Vec<u128> {
        self.sources.keys().copied().collect()
    }

    fn held(&self, button: MouseButton) -> bool {
        self.sources.values().flatten().any(|e| e.button == Some(button))
    }

    pub fn event(&mut self, source: u128, mut event: SessionMouseEvent) -> Option<SessionMouseEvent> {
        if !event.x_px.is_finite() || !event.y_px.is_finite() {
            return None;
        }
        if event.action == MouseAction::Press && event.button.is_none() {
            return None;
        }
        let already = event.button.is_some_and(|button| self.held(button));
        let holds = self.sources.entry(source).or_default();
        for held in holds.iter_mut() {
            held.x_px = event.x_px;
            held.y_px = event.y_px;
        }
        if event.action == MouseAction::Motion && event.button.is_none() && event.any_button_pressed {
            event.button = holds.last().and_then(|held| held.button);
        }
        let index = holds.iter().position(|held| held.button == event.button);
        let deliver = match event.action {
            MouseAction::Press => {
                if index.is_none() {
                    holds.push(event);
                }
                !already
            }
            MouseAction::Release => {
                if let Some(index) = index {
                    holds.remove(index);
                    true
                } else {
                    false
                }
            }
            MouseAction::Motion => (event.button.is_none() && !event.any_button_pressed) || index.is_some(),
        };
        if holds.is_empty() {
            self.sources.remove(&source);
        }
        if !deliver || (event.action == MouseAction::Release && event.button.is_some_and(|b| self.held(b))) {
            return None;
        }
        event.any_button_pressed = !self.sources.is_empty();
        Some(event)
    }

    pub fn release(&mut self, source: u128) -> Vec<SessionMouseEvent> {
        let mut out = Vec::new();
        for mut event in self.sources.remove(&source).unwrap_or_default() {
            if event.button.is_some_and(|b| self.held(b)) {
                continue;
            }
            event.action = MouseAction::Release;
            event.modifiers = Default::default();
            event.any_button_pressed = !self.sources.is_empty();
            out.push(event);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn event(action: MouseAction, button: MouseButton, x: f32) -> SessionMouseEvent {
        SessionMouseEvent { action, button: Some(button), any_button_pressed: false, modifiers: Default::default(), x_px: x, y_px: 3.0 }
    }
    #[test]
    fn shared_buttons_release_only_after_last_owner_and_keep_latest_position() {
        let mut held = HeldButtons::default();
        assert!(held.event(1, event(MouseAction::Press, MouseButton::Left, 2.0)).is_some());
        assert!(held.event(2, event(MouseAction::Press, MouseButton::Left, 5.0)).is_none());
        assert!(held.release(1).is_empty());
        assert!(held.event(2, event(MouseAction::Motion, MouseButton::Left, 8.5)).is_some());
        let releases = held.release(2);
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].action, MouseAction::Release);
        assert_eq!(releases[0].x_px, 8.5);
        assert!(!releases[0].any_button_pressed);
        assert!(held.release(2).is_empty());
    }
    #[test]
    fn native_drag_without_named_button_uses_owned_button_and_rejects_stale_drag() {
        let mut held = HeldButtons::default();
        let mut drag = event(MouseAction::Motion, MouseButton::Left, 4.0);
        drag.button = None;
        drag.any_button_pressed = true;
        assert!(held.event(1, drag).is_none());
        held.event(1, event(MouseAction::Press, MouseButton::Right, 2.0));
        assert_eq!(held.event(1, drag).unwrap().button, Some(MouseButton::Right));
        held.release(1);
        assert!(held.event(1, drag).is_none());
        assert!(held.sources().is_empty());
    }
    #[test]
    fn orphan_drag_and_release_do_not_acquire_holds() {
        let mut held = HeldButtons::default();
        assert!(held.event(1, event(MouseAction::Motion, MouseButton::Left, 1.0)).is_none());
        assert!(held.event(1, event(MouseAction::Release, MouseButton::Left, 1.0)).is_none());
        assert!(held.event(1, event(MouseAction::Press, MouseButton::Left, f32::NAN)).is_none());
        assert!(held.release(1).is_empty());
    }
}
