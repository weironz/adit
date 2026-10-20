//! Do not let a later swallowed key overtake a window-delivered modifier.
use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub(crate) fn is_altgr_control(vk: u32, scan: u32) -> bool {
    // Windows tags AltGr's auxiliary left Ctrl with the full 0x21d scan.
    // winit filters that Ctrl before delivering window keys. Truncating it to
    // 0x1d would wait forever for a real Ctrl event which will never arrive.
    matches!(vk, 0x11 | 0xa2) && scan == 0x21d
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ModifierEdge {
    pub scan: u8,
    pub extended: bool,
    pub pressed: bool,
}

pub(crate) enum Entry<T> {
    Key(T),
    WindowModifier(ModifierEdge),
}

pub(crate) struct OrderedQueue<T> {
    entries: VecDeque<Entry<T>>,
    waiting_since: Option<Instant>,
}

impl<T> Default for OrderedQueue<T> {
    fn default() -> Self {
        Self { entries: VecDeque::new(), waiting_since: None }
    }
}

impl<T> OrderedQueue<T> {
    pub fn push(&mut self, entry: Entry<T>) {
        self.entries.push_back(entry);
    }

    pub fn drain(&mut self, mut acknowledged: Option<ModifierEdge>, now: Instant) -> (Vec<T>, bool) {
        let mut keys = Vec::new();
        while let Some(entry) = self.entries.front() {
            match entry {
                Entry::WindowModifier(edge) => {
                    if acknowledged == Some(*edge) {
                        acknowledged = None;
                        self.entries.pop_front();
                        self.waiting_since = None;
                    } else {
                        // Missing window input must release capture, not leave a
                        // green toolbar and a permanently blocked key queue.
                        let started = self.waiting_since.get_or_insert(now);
                        return (keys, now.saturating_duration_since(*started) >= Duration::from_secs(3));
                    }
                }
                Entry::Key(_) => {
                    if let Some(Entry::Key(key)) = self.entries.pop_front() {
                        keys.push(key);
                    }
                }
            }
        }
        self.waiting_since = None;
        (keys, false)
    }

    pub fn take_all(&mut self) -> Vec<T> {
        self.waiting_since = None;
        self.entries.drain(..).filter_map(|entry| match entry {
            Entry::Key(key) => Some(key),
            Entry::WindowModifier(_) => None,
        }).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn altgr_auxiliary_control_is_not_a_window_modifier_fence() {
        let now = Instant::now();
        let mut queue = OrderedQueue::default();
        for pressed in [true, false] {
            if !is_altgr_control(0xa2, 0x21d) {
                queue.push(Entry::WindowModifier(ModifierEdge { scan: 0x1d, extended: false, pressed }));
            }
            let alt = ModifierEdge { scan: 0x38, extended: true, pressed };
            queue.push(Entry::WindowModifier(alt));
            queue.push(Entry::Key(pressed));
            assert_eq!(queue.drain(Some(alt), now), (vec![pressed], false));
        }
        assert_eq!(queue.drain(None, now + Duration::from_secs(4)), (vec![], false));
        assert!(!is_altgr_control(0xa2, 0x1d));
        assert!(!is_altgr_control(0xa3, 0x21d));
        assert!(!is_altgr_control(0xa2, 0x121d));
    }

    #[test]
    fn queued_win_cannot_overtake_shift_or_alt_window_release() {
        for scan in [0x2a, 0x36, 0x38, 0x1d] {
            let down = ModifierEdge { scan, extended: false, pressed: true };
            let up = ModifierEdge { pressed: false, ..down };
            let mut queue = OrderedQueue::default();
            queue.push(Entry::WindowModifier(down));
            queue.push(Entry::Key("first chord"));
            queue.push(Entry::WindowModifier(up));
            queue.push(Entry::Key("next Win down"));
            let now = Instant::now();
            assert_eq!(queue.drain(None, now), (vec![], false));
            assert_eq!(queue.drain(Some(up), now), (vec![], false));
            assert_eq!(queue.drain(Some(down), now), (vec!["first chord"], false));
            assert_eq!(queue.drain(None, now), (vec![], false));
            assert_eq!(queue.drain(Some(up), now), (vec!["next Win down"], false));
        }
    }

    #[test]
    fn missing_window_modifier_times_out_without_sleeping() {
        let now = Instant::now();
        let mut queue = OrderedQueue::default();
        queue.push(Entry::WindowModifier(ModifierEdge { scan: 0x38, extended: true, pressed: true }));
        queue.push(Entry::Key(42));
        assert_eq!(queue.drain(None, now), (vec![], false));
        assert_eq!(queue.drain(None, now + Duration::from_secs(2)), (vec![], false));
        assert_eq!(queue.drain(None, now + Duration::from_secs(3)), (vec![], true));
        // Epoch retirement discards the blocked keys rather than replaying them.
        queue = OrderedQueue::default();
        assert_eq!(queue.drain(None, now), (vec![], false));
    }
}
