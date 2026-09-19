//! Structured keyboard identity and per-attachment ownership, independent of PTYs.
use std::collections::BTreeMap;

use crate::provider::{TerminalKey, TerminalKeyAction, TerminalKeyEvent, TerminalModifiers};

pub(crate) fn functional_name(key: &TerminalKey) -> Option<String> {
    match key {
        TerminalKey::UnicodeScalar(_) => None,
        TerminalKey::Code(name) => Some(name.clone()),
        TerminalKey::Named(crate::provider::TerminalNamedKey::Function(n)) => Some(format!("F{n}")),
        TerminalKey::Named(key) => Some(format!("{key:?}")),
    }
}

#[derive(Default)]
pub(crate) struct HeldKeys {
    by_source: BTreeMap<u128, Vec<TerminalKeyEvent>>,
    // Keep the identity of the first delivered press until the last source
    // releases it, even when sources supply different physical metadata.
    delivered: Vec<TerminalKeyEvent>,
}
impl HeldKeys {
    pub fn sources(&self) -> Vec<u128> {
        self.by_source.keys().copied().collect()
    }
    fn identity(a: &TerminalKeyEvent, b: &TerminalKeyEvent) -> bool {
        match (&a.physical_key, &b.physical_key) {
            (Some(a), Some(b)) => a == b,
            (None, None) => a.key == b.key,
            _ => false,
        }
    }
    fn held(&self, key: &TerminalKey) -> bool {
        self.by_source.values().flatten().any(|event| &event.key == key)
    }
    pub fn event(&mut self, source: u128, mut event: TerminalKeyEvent) -> Result<Option<TerminalKeyEvent>, String> {
        let already_held = self.held(&event.key);
        let keys = self.by_source.entry(source).or_default();
        let index = keys.iter().position(|held| Self::identity(held, &event));
        match event.action {
            TerminalKeyAction::Release => {
                let Some(index) = index else { return Ok(None) };
                let held = keys.remove(index);
                // Layout or modifiers can change while a physical key is held.
                event.key = held.key;
                event.generated_text = held.generated_text;
                event.physical_key = held.physical_key;
                if keys.is_empty() {
                    self.by_source.remove(&source);
                }
                if self.held(&event.key) {
                    return Ok(None);
                }
            }
            TerminalKeyAction::Repeat => {
                let Some(index) = index else {
                    return Ok(None);
                };
                event.key = keys[index].key.clone();
            }
            TerminalKeyAction::Press => {
                if let Some(index) = index {
                    event.key = keys[index].key.clone();
                } else {
                    if keys.len() >= 256 {
                        return Err("attachment exceeds held-key limit".into());
                    }
                    keys.push(event.clone());
                    if already_held {
                        return Ok(None);
                    }
                    self.delivered.push(event.clone());
                }
            }
        }
        if let Some(index) = self.delivered.iter().position(|held| held.key == event.key) {
            if event.action == TerminalKeyAction::Release {
                let original = self.delivered.remove(index);
                event.physical_key = original.physical_key;
                event.generated_text = original.generated_text;
            } else {
                event.physical_key = self.delivered[index].physical_key.clone();
            }
        }
        Ok(Some(event))
    }
    pub fn release(&mut self, source: u128) -> Vec<TerminalKeyEvent> {
        let Some(keys) = self.by_source.remove(&source) else { return vec![] };
        let modifiers = self.by_source.values().flatten().fold(TerminalModifiers::empty(), |mods, e| {
            mods | match functional_name(&e.key).as_deref() {
                Some("ShiftLeft" | "ShiftRight") => TerminalModifiers::SHIFT,
                Some("ControlLeft" | "ControlRight") => TerminalModifiers::CTRL,
                Some("AltLeft" | "AltRight") => TerminalModifiers::ALT,
                Some("MetaLeft" | "MetaRight") => TerminalModifiers::SUPER,
                _ => TerminalModifiers::empty(),
            }
        });
        let mut released = Vec::new();
        for event in keys {
            if self.held(&event.key) || released.iter().any(|e: &TerminalKeyEvent| e.key == event.key) {
                continue;
            }
            let index = self.delivered.iter().position(|held| held.key == event.key).expect("held key has delivered press");
            let mut event = self.delivered.remove(index);
            event.action = TerminalKeyAction::Release;
            event.modifiers = modifiers;
            event.consumed_modifiers = TerminalModifiers::empty();
            released.push(event);
        }
        released
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(action: TerminalKeyAction) -> TerminalKeyEvent {
        TerminalKeyEvent {
            key: TerminalKey::UnicodeScalar(b'w' as u32),
            action,
            modifiers: TerminalModifiers::empty(),
            consumed_modifiers: TerminalModifiers::empty(),
            generated_text: Some("w".into()),
            platform_keycode: 0,
            physical_key: Some("KeyW".into()),
        }
    }
    #[test]
    fn final_release_matches_first_press_when_sources_supply_different_physical_metadata() {
        for explicit_release in [true, false] {
            let mut held = HeldKeys::default();
            let first = held.event(1, key(TerminalKeyAction::Press)).unwrap().unwrap();
            let mut second = key(TerminalKeyAction::Press);
            second.physical_key = None;
            held.event(2, second.clone()).unwrap();
            assert!(held.release(1).is_empty());
            let last = if explicit_release {
                second.action = TerminalKeyAction::Release;
                held.event(2, second).unwrap().unwrap()
            } else {
                held.release(2).remove(0)
            };
            assert_eq!(last.physical_key, first.physical_key);
            assert_eq!(last.generated_text, first.generated_text);
        }
    }

    #[test]
    fn shared_holds_release_only_after_last_driver_and_never_replay() {
        let mut held = HeldKeys::default();
        assert!(held.event(1, key(TerminalKeyAction::Press)).unwrap().is_some());
        assert!(held.event(2, key(TerminalKeyAction::Press)).unwrap().is_none());
        assert!(held.release(1).is_empty());
        let releases = held.release(2);
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].action, TerminalKeyAction::Release);
        assert!(held.release(2).is_empty());
        assert!(held.event(2, key(TerminalKeyAction::Repeat)).unwrap().is_none());
        assert!(held.event(2, key(TerminalKeyAction::Release)).unwrap().is_none());
        assert!(held.event(2, key(TerminalKeyAction::Press)).unwrap().is_some());
    }
    #[test]
    fn physical_release_uses_identity_from_press_even_after_layout_change() {
        let mut held = HeldKeys::default();
        held.event(1, key(TerminalKeyAction::Press)).unwrap();
        let mut release = key(TerminalKeyAction::Release);
        release.key = TerminalKey::UnicodeScalar(b'z' as u32);
        assert_eq!(held.event(1, release).unwrap().unwrap().key, TerminalKey::UnicodeScalar(b'w' as u32));
        assert!(held.release(1).is_empty());
    }
    #[test]
    fn release_from_one_driver_does_not_release_another_driver() {
        let mut held = HeldKeys::default();
        held.event(1, key(TerminalKeyAction::Press)).unwrap();
        held.event(2, key(TerminalKeyAction::Press)).unwrap();
        assert!(held.event(1, key(TerminalKeyAction::Release)).unwrap().is_none());
        assert!(held.event(2, key(TerminalKeyAction::Release)).unwrap().is_some());
    }
}
