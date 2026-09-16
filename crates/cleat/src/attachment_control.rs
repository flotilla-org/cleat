//! Session control policy, independent of transports and the PTY.
use std::collections::BTreeMap;

#[derive(Default)]
struct Attachment {
    driving: bool,
    focused: bool,
    size: Option<(u16, u16)>,
    cell_size: Option<(u32, u32)>,
}

pub(crate) struct AttachmentControl<K> {
    attachments: BTreeMap<K, Attachment>,
    exclusive: Option<K>,
}

impl<K> Default for AttachmentControl<K> {
    fn default() -> Self {
        Self { attachments: BTreeMap::new(), exclusive: None }
    }
}

impl<K: Copy + Ord> AttachmentControl<K> {
    /// `take` requests exclusive control. A plain driving request releases
    /// the caller's exclusivity, but never displaces someone else's.
    pub fn request(&mut self, key: K, drive: bool, take: bool) -> bool {
        if self.exclusive == Some(key) {
            self.exclusive = None;
        }
        if drive && take {
            for attachment in self.attachments.values_mut() {
                attachment.driving = false;
            }
            self.exclusive = Some(key);
        }
        let granted = drive && (self.exclusive.is_none() || self.exclusive == Some(key));
        self.attachments.entry(key).or_insert_with(|| Attachment { focused: true, ..Attachment::default() }).driving = granted;
        granted
    }

    pub fn exclusive(&self) -> Option<K> {
        self.exclusive
    }

    pub fn controllers(&self) -> impl Iterator<Item = K> + '_ {
        self.attachments.iter().filter(|(_, a)| a.driving).map(|(key, _)| *key)
    }

    pub fn has_controllers(&self) -> bool {
        self.controllers().next().is_some()
    }

    pub fn remove(&mut self, key: K) {
        self.attachments.remove(&key);
        if self.exclusive == Some(key) {
            self.exclusive = None;
        }
    }

    pub fn retain(&mut self, mut keep: impl FnMut(K) -> bool) {
        self.attachments.retain(|key, _| keep(*key));
        if self.exclusive.is_some_and(|key| !self.attachments.contains_key(&key)) {
            self.exclusive = None;
        }
    }

    pub fn demote_all(&mut self) {
        for attachment in self.attachments.values_mut() {
            attachment.driving = false;
        }
        self.exclusive = None;
    }

    pub fn focus(&mut self, key: K, focused: bool) {
        if let Some(attachment) = self.attachments.get_mut(&key) {
            attachment.focused = focused;
        }
    }

    pub fn focused(&self) -> bool {
        self.attachments.values().any(|a| a.driving && a.focused)
    }

    pub fn resize(&mut self, key: K, cols: u16, rows: u16) {
        if let Some(attachment) = self.attachments.get_mut(&key) {
            attachment.size = Some((cols.max(1), rows.max(1)));
        }
    }

    pub fn set_cell_size(&mut self, key: K, width: u32, height: u32) {
        if let Some(attachment) = self.attachments.get_mut(&key) {
            attachment.cell_size = Some((width.max(1), height.max(1)));
        }
    }

    pub fn cell_size(&self, key: K) -> (u32, u32) {
        self.attachments.get(&key).and_then(|a| a.cell_size).unwrap_or((1, 1))
    }

    /// Use the oldest connected driver's cell units for the one PTY's pixel
    /// geometry. Input from other clients is converted into these units.
    pub fn application_cell_size(&self) -> Option<(u32, u32)> {
        self.controllers().next().map(|key| self.cell_size(key))
    }

    /// None means keep the last PTY size. Watcher sizes are retained for a
    /// later explicit promotion, but never influence current geometry.
    pub fn geometry(&self) -> Option<(u16, u16)> {
        self.attachments.values().filter(|a| a.driving).filter_map(|a| a.size).reduce(|(cols, rows), (c, r)| (cols.min(c), rows.min(r)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_drivers_intersect_and_watchers_do_not_vote() {
        let mut control = AttachmentControl::default();
        assert!(control.request(1, true, false));
        assert!(control.request(2, true, false));
        assert!(!control.request(3, false, false));
        control.resize(1, 100, 20);
        control.resize(2, 80, 40);
        control.resize(3, 10, 5);
        assert_eq!(control.geometry(), Some((80, 20)));
        control.remove(1);
        assert_eq!(control.geometry(), Some((80, 40)));
        control.request(2, false, false);
        assert_eq!(control.geometry(), None);
        control.request(3, true, false);
        assert_eq!(control.geometry(), Some((10, 5)));
    }

    #[test]
    fn focus_is_the_union_of_drivers_only() {
        let mut control = AttachmentControl::default();
        control.request(1, true, false);
        control.request(2, true, false);
        control.request(3, false, false);
        control.focus(1, false);
        assert!(control.focused());
        control.focus(2, false);
        assert!(!control.focused());
        control.request(3, true, true);
        assert!(control.focused());
        control.remove(3);
        assert!(!control.focused());
    }

    #[test]
    fn exclusivity_demotes_without_automatic_restoration() {
        let mut control = AttachmentControl::default();
        control.request(1, true, false);
        control.request(2, true, false);
        assert!(control.request(1, true, true));
        assert_eq!(control.controllers().collect::<Vec<_>>(), vec![1]);
        assert!(!control.request(3, true, false));
        assert!(control.request(3, true, true));
        assert_eq!(control.exclusive(), Some(3));
        assert!(control.request(3, true, false));
        assert_eq!(control.exclusive(), None);
        assert_eq!(control.controllers().collect::<Vec<_>>(), vec![3]);
        assert!(control.request(2, true, false));
        control.request(2, true, true);
        control.remove(2);
        assert!(!control.has_controllers());
        assert_eq!(control.exclusive(), None);
        assert!(control.request(1, true, false));
    }
}
