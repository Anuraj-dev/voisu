//! The hook-side half of the Ctrl+Win Trigger Key: a pure, allocation-free
//! tracker that turns raw key transitions into chord signals and decides, per
//! event, whether the key passes through and whether a Start-menu mask key
//! must be injected.
//!
//! The Windows keyboard hook (`platform::hotkey`) filters injected and AltGr
//! events, classifies the virtual key and feeds this tracker inside the hook
//! callback, so every method here is O(1) and never blocks or allocates.
//!
//! Ctrl and Win always pass through: swallowing a modifier risks a stuck key
//! in the focused app. Opening the Start menu is suppressed instead by
//! injecting an unassigned "mask" key while Win is still held, the technique
//! AutoHotkey (`#MenuMaskKey`) and PowerToys use. Windows only opens Start when
//! Win goes down and up with nothing in between.

/// Which physical side of a modifier an event came from. Tracking sides keeps
/// a released RCtrl from clearing a still-held LCtrl.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Side {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key {
    Ctrl(Side),
    Win(Side),
    Escape,
    /// Any other key, including Shift and Alt.
    Other,
}

/// What the tracker reports to the gesture thread.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChordSignal {
    /// Ctrl and Win are both held and nothing else interfered.
    Pressed,
    /// The chord was held and one of its modifiers went up.
    Released,
    /// Another key went down while the chord was held: the chord was part of a
    /// different shortcut (Ctrl+Win+D, Win+Ctrl+Left...).
    Interrupted,
    /// Esc went down while a Recording was armed or the chord was held. That
    /// Esc is swallowed, along with its key-up.
    Escape,
}

/// The decision for one key event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Verdict {
    /// Let the event reach the focused app. `false` swallows it.
    pub pass: bool,
    /// Inject the Start-menu mask key now, while Win is still held.
    pub inject_mask: bool,
    pub signal: Option<ChordSignal>,
}

impl Verdict {
    const PASS: Self = Self {
        pass: true,
        inject_mask: false,
        signal: None,
    };
}

#[derive(Debug, Default)]
pub struct ChordTracker {
    ctrl: SideSet,
    win: SideSet,
    /// Both modifiers held and the chord reported as Pressed.
    active: bool,
    /// Some other key went down while a modifier was held. The chord cannot
    /// form again until both modifiers are up, so Ctrl+C followed by Win, or
    /// Win+Shift+S, never starts a Recording.
    poisoned: bool,
    /// An Esc down was swallowed; swallow its repeats and its key-up as well,
    /// so the focused app never sees half an Esc.
    escape_swallowed: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct SideSet {
    left: bool,
    right: bool,
}

impl SideSet {
    fn any(self) -> bool {
        self.left || self.right
    }

    fn set(&mut self, side: Side, down: bool) {
        match side {
            Side::Left => self.left = down,
            Side::Right => self.right = down,
        }
    }

    fn get(self, side: Side) -> bool {
        match side {
            Side::Left => self.left,
            Side::Right => self.right,
        }
    }
}

impl ChordTracker {
    /// Clears all tracked state, for example after the hook is reinstalled and
    /// key-ups may have been missed.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Feeds one key transition.
    ///
    /// `other_modifier_down` is the operating system's current view of the
    /// *other* chord modifier (Win for a Ctrl event, Ctrl for a Win event). It
    /// is read only on a fresh modifier press, so a key-up the hook missed
    /// (secure desktop, lock screen) cannot leave a stale modifier that forms a
    /// false chord. `armed` is true while the gesture thread has a Recording or
    /// a pending tap in flight, which is when Esc means Cancel.
    pub fn handle(
        &mut self,
        key: Key,
        down: bool,
        other_modifier_down: bool,
        armed: bool,
    ) -> Verdict {
        match key {
            Key::Ctrl(side) => self.modifier(Modifier::Ctrl, side, down, other_modifier_down),
            Key::Win(side) => self.modifier(Modifier::Win, side, down, other_modifier_down),
            Key::Escape => self.escape(down, armed),
            Key::Other => self.other(down),
        }
    }

    fn modifier(&mut self, which: Modifier, side: Side, down: bool, other_down: bool) -> Verdict {
        let (this, other) = match which {
            Modifier::Ctrl => (&mut self.ctrl, &mut self.win),
            Modifier::Win => (&mut self.win, &mut self.ctrl),
        };
        if down {
            if this.get(side) {
                // Auto-repeat of a held modifier.
                return Verdict::PASS;
            }
            let fresh = !this.any();
            this.set(side, true);
            if fresh && !other_down && other.any() {
                // The OS says the other modifier is up, but no key-up reached
                // the hook: drop the stale state.
                *other = SideSet::default();
            }
            if self.ctrl.any() && self.win.any() && !self.active && !self.poisoned {
                self.active = true;
                return Verdict {
                    pass: true,
                    inject_mask: true,
                    signal: Some(ChordSignal::Pressed),
                };
            }
            return Verdict::PASS;
        }

        this.set(side, false);
        let mut verdict = Verdict::PASS;
        if self.active && !(self.ctrl.any() && self.win.any()) {
            self.active = false;
            verdict.signal = Some(ChordSignal::Released);
        }
        if !self.ctrl.any() && !self.win.any() {
            self.poisoned = false;
        }
        verdict
    }

    fn escape(&mut self, down: bool, armed: bool) -> Verdict {
        if down {
            if self.escape_swallowed {
                // Auto-repeat of the swallowed Esc.
                return Verdict {
                    pass: false,
                    inject_mask: false,
                    signal: None,
                };
            }
            if armed || self.active {
                self.escape_swallowed = true;
                return Verdict {
                    pass: false,
                    inject_mask: false,
                    signal: Some(ChordSignal::Escape),
                };
            }
            return self.other(true);
        }
        if std::mem::take(&mut self.escape_swallowed) {
            return Verdict {
                pass: false,
                inject_mask: false,
                signal: None,
            };
        }
        Verdict::PASS
    }

    fn other(&mut self, down: bool) -> Verdict {
        if !down || !(self.ctrl.any() || self.win.any()) {
            return Verdict::PASS;
        }
        self.poisoned = true;
        if std::mem::take(&mut self.active) {
            return Verdict {
                pass: true,
                inject_mask: false,
                signal: Some(ChordSignal::Interrupted),
            };
        }
        Verdict::PASS
    }
}

#[derive(Clone, Copy)]
enum Modifier {
    Ctrl,
    Win,
}

#[cfg(test)]
mod tests {
    use super::*;

    const LCTRL: Key = Key::Ctrl(Side::Left);
    const RCTRL: Key = Key::Ctrl(Side::Right);
    const LWIN: Key = Key::Win(Side::Left);

    /// Feeds events with the OS agreeing with the tracker's own modifier view.
    fn feed(tracker: &mut ChordTracker, events: &[(Key, bool)], armed: bool) -> Vec<Verdict> {
        events
            .iter()
            .map(|&(key, down)| {
                let other = match key {
                    Key::Ctrl(_) => tracker.win.any(),
                    Key::Win(_) => tracker.ctrl.any(),
                    _ => false,
                };
                tracker.handle(key, down, other, armed)
            })
            .collect()
    }

    fn signals(verdicts: &[Verdict]) -> Vec<ChordSignal> {
        verdicts.iter().filter_map(|v| v.signal).collect()
    }

    #[test]
    fn ctrl_then_win_presses_once_and_masks_start() {
        let mut t = ChordTracker::default();
        let v = feed(&mut t, &[(LCTRL, true), (LWIN, true)], false);
        assert_eq!(signals(&v), [ChordSignal::Pressed]);
        assert!(v[1].inject_mask && v[1].pass);
        assert!(!v[0].inject_mask);
    }

    #[test]
    fn win_then_ctrl_presses_too() {
        let mut t = ChordTracker::default();
        let v = feed(&mut t, &[(LWIN, true), (LCTRL, true)], false);
        assert_eq!(signals(&v), [ChordSignal::Pressed]);
        assert!(v[1].inject_mask);
    }

    #[test]
    fn modifiers_always_pass_through() {
        let mut t = ChordTracker::default();
        let v = feed(
            &mut t,
            &[(LCTRL, true), (LWIN, true), (LWIN, false), (LCTRL, false)],
            false,
        );
        assert!(v.iter().all(|v| v.pass));
    }

    #[test]
    fn auto_repeat_does_not_press_again() {
        let mut t = ChordTracker::default();
        let v = feed(
            &mut t,
            &[(LCTRL, true), (LWIN, true), (LWIN, true), (LCTRL, true)],
            false,
        );
        assert_eq!(signals(&v), [ChordSignal::Pressed]);
        assert_eq!(v.iter().filter(|v| v.inject_mask).count(), 1);
    }

    #[test]
    fn releasing_win_first_releases() {
        let mut t = ChordTracker::default();
        let v = feed(
            &mut t,
            &[(LCTRL, true), (LWIN, true), (LWIN, false), (LCTRL, false)],
            false,
        );
        assert_eq!(signals(&v), [ChordSignal::Pressed, ChordSignal::Released]);
        assert_eq!(v[2].signal, Some(ChordSignal::Released));
    }

    #[test]
    fn releasing_ctrl_first_releases() {
        let mut t = ChordTracker::default();
        let v = feed(
            &mut t,
            &[(LCTRL, true), (LWIN, true), (LCTRL, false), (LWIN, false)],
            false,
        );
        assert_eq!(signals(&v), [ChordSignal::Pressed, ChordSignal::Released]);
        assert_eq!(v[2].signal, Some(ChordSignal::Released));
    }

    #[test]
    fn re_pressing_the_released_modifier_forms_the_chord_again() {
        // Double tap by tapping Win while Ctrl stays held.
        let mut t = ChordTracker::default();
        let v = feed(
            &mut t,
            &[(LCTRL, true), (LWIN, true), (LWIN, false), (LWIN, true)],
            false,
        );
        assert_eq!(
            signals(&v),
            [
                ChordSignal::Pressed,
                ChordSignal::Released,
                ChordSignal::Pressed
            ]
        );
    }

    #[test]
    fn other_key_mid_chord_interrupts_and_passes() {
        // Ctrl+Win+D (new virtual desktop) must still reach Windows.
        let mut t = ChordTracker::default();
        let v = feed(
            &mut t,
            &[
                (LCTRL, true),
                (LWIN, true),
                (Key::Other, true),
                (Key::Other, false),
                (LWIN, false),
                (LCTRL, false),
            ],
            true,
        );
        assert_eq!(
            signals(&v),
            [ChordSignal::Pressed, ChordSignal::Interrupted]
        );
        assert!(v[2].pass);
    }

    #[test]
    fn interrupted_chord_cannot_re_form_until_both_modifiers_are_up() {
        let mut t = ChordTracker::default();
        let v = feed(
            &mut t,
            &[
                (LCTRL, true),
                (LWIN, true),
                (Key::Other, true),
                (LWIN, false),
                (LWIN, true),
                (LWIN, false),
                (LCTRL, false),
                (LCTRL, true),
                (LWIN, true),
            ],
            false,
        );
        assert_eq!(
            signals(&v),
            [
                ChordSignal::Pressed,
                ChordSignal::Interrupted,
                ChordSignal::Pressed
            ]
        );
        assert_eq!(v[8].signal, Some(ChordSignal::Pressed));
    }

    #[test]
    fn shortcuts_with_another_key_first_never_press() {
        // Ctrl+C, then Win while Ctrl is still held.
        let mut t = ChordTracker::default();
        let v = feed(
            &mut t,
            &[
                (LCTRL, true),
                (Key::Other, true),
                (Key::Other, false),
                (LWIN, true),
            ],
            false,
        );
        assert!(signals(&v).is_empty());
        // Win+Shift+S, then Ctrl.
        let mut t = ChordTracker::default();
        let v = feed(
            &mut t,
            &[(LWIN, true), (Key::Other, true), (LCTRL, true)],
            false,
        );
        assert!(signals(&v).is_empty());
        assert!(v.iter().all(|v| !v.inject_mask && v.pass));
    }

    #[test]
    fn plain_win_tap_is_untouched() {
        let mut t = ChordTracker::default();
        let v = feed(&mut t, &[(LWIN, true), (LWIN, false)], false);
        assert!(signals(&v).is_empty());
        assert!(v.iter().all(|v| v.pass && !v.inject_mask));
    }

    #[test]
    fn other_side_release_keeps_the_modifier_held() {
        let mut t = ChordTracker::default();
        let v = feed(
            &mut t,
            &[(LCTRL, true), (RCTRL, true), (RCTRL, false), (LWIN, true)],
            false,
        );
        assert_eq!(signals(&v), [ChordSignal::Pressed]);
    }

    #[test]
    fn missed_key_up_is_corrected_by_the_os_view() {
        let mut t = ChordTracker::default();
        // Ctrl went down, then its key-up was lost (e.g. the lock screen).
        t.handle(LCTRL, true, false, false);
        // Win now goes down; the OS reports Ctrl as up.
        let v = t.handle(LWIN, true, false, false);
        assert_eq!(v.signal, None);
        assert!(!v.inject_mask);
    }

    #[test]
    fn escape_is_swallowed_only_when_armed() {
        let mut t = ChordTracker::default();
        let v = feed(&mut t, &[(Key::Escape, true), (Key::Escape, false)], false);
        assert!(v.iter().all(|v| v.pass && v.signal.is_none()));

        let v = feed(
            &mut t,
            &[
                (Key::Escape, true),
                (Key::Escape, true),
                (Key::Escape, false),
            ],
            true,
        );
        assert_eq!(signals(&v), [ChordSignal::Escape]);
        assert!(v.iter().all(|v| !v.pass));
    }

    #[test]
    fn escape_up_is_swallowed_even_after_disarming() {
        let mut t = ChordTracker::default();
        let down = t.handle(Key::Escape, true, false, true);
        let up = t.handle(Key::Escape, false, false, false);
        assert!(!down.pass && !up.pass);
    }

    #[test]
    fn escape_during_the_chord_cancels_before_arming() {
        let mut t = ChordTracker::default();
        let v = feed(
            &mut t,
            &[(LCTRL, true), (LWIN, true), (Key::Escape, true)],
            false,
        );
        assert_eq!(signals(&v), [ChordSignal::Pressed, ChordSignal::Escape]);
    }

    #[test]
    fn reset_clears_held_state() {
        let mut t = ChordTracker::default();
        feed(&mut t, &[(LCTRL, true), (LWIN, true)], false);
        t.reset();
        let v = feed(&mut t, &[(LWIN, false), (LCTRL, false)], false);
        assert!(signals(&v).is_empty());
    }
}
