//! Do not let a later swallowed key overtake a window-delivered modifier.
use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub(crate) fn is_altgr_control(vk: u32, scan: u32) -> bool {
    // Windows tags AltGr's auxiliary left Ctrl with the full 0x21d scan.
    // winit may filter it using next-message lookahead. Truncating it to 0x1d
    // could wait forever for that window event. The auxiliary edge itself must
    // not grant any permission to forward or acknowledge a window Ctrl.
    matches!(vk, 0x11 | 0xa2) && scan == 0x21d
}

pub(crate) fn record_window_modifier(
    vk: u32,
    pressed: bool,
    was_down: bool,
    awaiting_release: &mut bool,
) -> bool {
    if pressed && !was_down {
        *awaiting_release = true;
        true
    } else if !pressed && (*awaiting_release || (matches!(vk, 0x11 | 0xa2 | 0xa3) && was_down)) {
        // A genuine Ctrl may already have been held when capture started.
        // Its explicit LL release still authorizes releasing a remote-owned key;
        // an AltGr auxiliary release never reaches this function.
        *awaiting_release = false;
        true
    } else {
        false
    }
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
        Self {
            entries: VecDeque::new(),
            waiting_since: None,
        }
    }
}

impl<T> OrderedQueue<T> {
    pub fn push(&mut self, entry: Entry<T>) {
        self.entries.push_back(entry);
    }

    pub fn is_waiting_for(&self, edge: ModifierEdge) -> bool {
        matches!(self.entries.front(), Some(Entry::WindowModifier(front)) if *front == edge)
    }

    pub fn filtered_control_before_alt(&self, alt: ModifierEdge) -> Option<ModifierEdge> {
        if alt.scan != 0x38 || !alt.extended {
            return None;
        }
        let mut entries = self.entries.iter();
        let Entry::WindowModifier(control) = entries.next()? else {
            return None;
        };
        if control.scan != 0x1d {
            return None;
        }
        // Swallowed keys do not appear in winit's next-window-message lookahead.
        // They remain queued and will drain only after the real Ctrl is sent.
        for entry in entries {
            if let Entry::WindowModifier(next) = entry {
                return (*next == alt).then_some(*control);
            }
        }
        None
    }

    pub fn drain(
        &mut self,
        mut acknowledged: Option<ModifierEdge>,
        now: Instant,
    ) -> (Vec<T>, bool) {
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
                        return (
                            keys,
                            now.saturating_duration_since(*started) >= Duration::from_secs(3),
                        );
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
        self.entries
            .drain(..)
            .filter_map(|entry| match entry {
                Entry::Key(key) => Some(key),
                Entry::WindowModifier(_) => None,
            })
            .collect()
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
                queue.push(Entry::WindowModifier(ModifierEdge {
                    scan: 0x1d,
                    extended: false,
                    pressed,
                }));
            }
            let alt = ModifierEdge {
                scan: 0x38,
                extended: true,
                pressed,
            };
            queue.push(Entry::WindowModifier(alt));
            queue.push(Entry::Key(pressed));
            assert_eq!(queue.drain(Some(alt), now), (vec![pressed], false));
        }
        assert_eq!(
            queue.drain(None, now + Duration::from_secs(4)),
            (vec![], false)
        );
        assert!(!is_altgr_control(0xa2, 0x1d));
        assert!(!is_altgr_control(0xa3, 0x21d));
        assert!(!is_altgr_control(0xa2, 0x121d));
    }

    #[test]
    fn queued_win_cannot_overtake_shift_or_alt_window_release() {
        for scan in [0x2a, 0x36, 0x38, 0x1d] {
            let down = ModifierEdge {
                scan,
                extended: false,
                pressed: true,
            };
            let up = ModifierEdge {
                pressed: false,
                ..down
            };
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
        queue.push(Entry::WindowModifier(ModifierEdge {
            scan: 0x38,
            extended: true,
            pressed: true,
        }));
        queue.push(Entry::Key(42));
        assert_eq!(queue.drain(None, now), (vec![], false));
        assert_eq!(
            queue.drain(None, now + Duration::from_secs(2)),
            (vec![], false)
        );
        assert_eq!(
            queue.drain(None, now + Duration::from_secs(3)),
            (vec![], true)
        );
        // Epoch retirement discards the blocked keys rather than replaying them.
        queue = OrderedQueue::default();
        assert_eq!(queue.drain(None, now), (vec![], false));
    }

    #[test]
    fn altgr_fences_prevent_control_acknowledgement_across_their_order_barrier() {
        let now = Instant::now();
        let ctrl_down = ModifierEdge {
            scan: 0x1d,
            extended: false,
            pressed: true,
        };
        let ctrl_up = ModifierEdge {
            pressed: false,
            ..ctrl_down
        };
        let alt_down = ModifierEdge {
            scan: 0x38,
            extended: true,
            pressed: true,
        };
        let alt_up = ModifierEdge {
            pressed: false,
            ..alt_down
        };
        let mut queue = OrderedQueue::default();
        // True Ctrl is held across an AltGr cycle. Even when its eventual up
        // is already queued, an earlier exposed auxiliary up cannot claim it.
        queue.push(Entry::WindowModifier(ctrl_down));
        queue.push(Entry::WindowModifier(alt_down));
        queue.push(Entry::Key("AltGr character"));
        queue.push(Entry::WindowModifier(alt_up));
        queue.push(Entry::WindowModifier(ctrl_up));
        queue.push(Entry::Key("next Win"));
        assert!(queue.is_waiting_for(ctrl_down));
        assert_eq!(queue.drain(Some(ctrl_down), now), (vec![], false));
        assert!(!queue.is_waiting_for(ctrl_down)); // Auxiliary/repeated down.
        assert!(!queue.is_waiting_for(ctrl_up)); // Auxiliary up before AltGr.
        assert_eq!(
            queue.drain(Some(alt_down), now),
            (vec!["AltGr character"], false)
        );
        assert!(!queue.is_waiting_for(ctrl_down));
        assert!(!queue.is_waiting_for(ctrl_up)); // Auxiliary up precedes Alt-up.
        assert_eq!(queue.drain(None, now), (vec![], false));
        assert_eq!(queue.drain(Some(alt_up), now), (vec![], false));
        assert!(queue.is_waiting_for(ctrl_up));
        assert_eq!(queue.drain(Some(ctrl_up), now), (vec!["next Win"], false));
        assert!(!queue.is_waiting_for(ctrl_up)); // No orphan/repeated release.
    }

    #[test]
    fn real_preheld_control_release_is_authorized_but_repeat_is_not() {
        for vk in [0xa2, 0xa3] {
            let mut awaiting_release = false;
            assert!(!record_window_modifier(
                vk,
                true,
                true,
                &mut awaiting_release
            ));
            assert!(!awaiting_release);
            assert!(record_window_modifier(
                vk,
                false,
                true,
                &mut awaiting_release
            ));
            assert!(!record_window_modifier(
                vk,
                false,
                false,
                &mut awaiting_release
            ));
            assert!(record_window_modifier(
                vk,
                true,
                false,
                &mut awaiting_release
            ));
            assert!(!record_window_modifier(
                vk,
                true,
                true,
                &mut awaiting_release
            ));
            assert!(record_window_modifier(
                vk,
                false,
                true,
                &mut awaiting_release
            ));
            assert!(!awaiting_release);
        }
    }

    #[test]
    fn right_alt_witness_repairs_either_control_without_overtaking_captured_keys() {
        let now = Instant::now();
        for extended in [false, true] {
            for control_pressed in [false, true] {
                for alt_pressed in [false, true] {
                    for captured in [vec![], vec!["Win"], vec!["Win", "R"]] {
                        let control = ModifierEdge {
                            scan: 0x1d,
                            extended,
                            pressed: control_pressed,
                        };
                        let alt = ModifierEdge {
                            scan: 0x38,
                            extended: true,
                            pressed: alt_pressed,
                        };
                        let mut queue = OrderedQueue::default();
                        queue.push(Entry::WindowModifier(control));
                        for key in &captured {
                            queue.push(Entry::Key(*key));
                        }
                        queue.push(Entry::WindowModifier(alt));
                        queue.push(Entry::Key("after Alt"));
                        assert_eq!(queue.filtered_control_before_alt(alt), Some(control));
                        assert_eq!(queue.drain(None, now), (vec![], false));
                        // Sending/acking the real Ctrl must happen first. This
                        // drains its captured keys, then stops before Alt wire.
                        assert_eq!(queue.drain(Some(control), now), (captured, false));
                        assert_eq!(queue.filtered_control_before_alt(alt), None);
                        assert!(queue.is_waiting_for(alt));
                        assert_eq!(queue.drain(Some(alt), now), (vec!["after Alt"], false));
                        assert_eq!(queue.filtered_control_before_alt(alt), None);
                    }
                }
            }
        }
    }

    #[test]
    fn held_control_across_altgr_has_no_invented_control_release() {
        let now = Instant::now();
        let down = ModifierEdge {
            scan: 0x1d,
            extended: false,
            pressed: true,
        };
        let up = ModifierEdge {
            pressed: false,
            ..down
        };
        let alt_down = ModifierEdge {
            scan: 0x38,
            extended: true,
            pressed: true,
        };
        let alt_up = ModifierEdge {
            pressed: false,
            ..alt_down
        };
        let mut queue = OrderedQueue::default();
        queue.push(Entry::WindowModifier(down));
        assert_eq!(queue.drain(Some(down), now), (vec![], false));
        queue.push(Entry::WindowModifier(alt_down));
        queue.push(Entry::Key("AltGr text"));
        queue.push(Entry::WindowModifier(alt_up));
        assert_eq!(queue.filtered_control_before_alt(alt_down), None);
        assert_eq!(
            queue.drain(Some(alt_down), now),
            (vec!["AltGr text"], false)
        );
        assert_eq!(queue.filtered_control_before_alt(alt_up), None);
        assert!(!queue.is_waiting_for(up));
        assert_eq!(queue.drain(Some(alt_up), now), (vec![], false));
        // A real up immediately before a later Alt-up can be filtered by winit.
        // That actual edge, unlike auxiliary Ctrl-up, is a valid repair witness.
        queue.push(Entry::WindowModifier(alt_down));
        assert_eq!(queue.drain(Some(alt_down), now), (vec![], false));
        queue.push(Entry::WindowModifier(up));
        queue.push(Entry::WindowModifier(alt_up));
        assert_eq!(queue.filtered_control_before_alt(alt_up), Some(up));
        assert_eq!(queue.drain(Some(up), now), (vec![], false));
        assert_eq!(queue.drain(Some(alt_up), now), (vec![], false));
    }

    #[test]
    fn control_repair_requires_the_exact_prefix_in_the_current_queue() {
        let control = ModifierEdge {
            scan: 0x1d,
            extended: false,
            pressed: true,
        };
        let alt = ModifierEdge {
            scan: 0x38,
            extended: true,
            pressed: true,
        };
        let other = ModifierEdge {
            scan: 0x2a,
            extended: false,
            pressed: true,
        };
        let mut queue = OrderedQueue::default();
        queue.push(Entry::WindowModifier(control));
        queue.push(Entry::Key("Win"));
        assert_eq!(queue.filtered_control_before_alt(alt), None); // No Alt marker.
        queue.push(Entry::WindowModifier(other));
        queue.push(Entry::WindowModifier(alt));
        assert_eq!(queue.filtered_control_before_alt(alt), None); // Other modifier intervened.
        let mut queue = OrderedQueue::<()>::default();
        queue.push(Entry::WindowModifier(control));
        queue.push(Entry::WindowModifier(alt));
        assert_eq!(
            queue.filtered_control_before_alt(ModifierEdge {
                pressed: false,
                ..alt
            }),
            None
        );
        assert_eq!(
            queue.filtered_control_before_alt(ModifierEdge {
                extended: false,
                ..alt
            }),
            None
        );
        assert_eq!(queue.filtered_control_before_alt(alt), Some(control));
        // receive_current replaces the ordered queue when a hook generation
        // retires; a delayed RightAlt event cannot revive the old prefix.
        queue = OrderedQueue::default();
        assert_eq!(queue.filtered_control_before_alt(alt), None);
    }
}
