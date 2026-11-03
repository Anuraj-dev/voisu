//! The hook-side half of the Trigger Key: a pure, allocation-free tracker
//! that turns raw key transitions into chord signals for the configured
//! [`TriggerChord`] and decides, per event, whether the key passes through and
//! whether the mask key must be injected. [`Recorder`] is the same hook's
//! capture mode, used by the settings window's shortcut recorder.
//!
//! The Windows keyboard hook (`platform::hotkey`) filters injected and AltGr
//! events, classifies the virtual key and feeds this tracker inside the hook
//! callback, so every method here is O(1) and never blocks or allocates.
//!
//! Modifiers always pass through: swallowing a modifier risks a stuck key in
//! the focused app. Opening the Start menu (Win) or focusing the menu bar
//! (Alt) is suppressed instead by injecting an unassigned "mask" key while the
//! modifier is still held, the technique AutoHotkey (`#MenuMaskKey`) and
//! PowerToys use. Windows only acts on a Win or Alt release when nothing went
//! by in between. A chord's ordinary key, if it has one, is swallowed down
//! and up so it never types into the focused app.

pub use crate::trigger_key::{Modifier, Side};
use crate::trigger_key::{Need, Rejection, TriggerChord, key_at, key_name, scan_code};

/// The unassigned virtual key injected as the mask, the one AutoHotkey and
/// PowerToys use.
pub const MASK_VK: u8 = 0xE8;
/// `dwExtraInfo` of every key Voisu injects ("VOIS"), so the hook can tell
/// its own events from another app's.
pub const MASK_TAG: usize = 0x564F_4953;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key {
    Mod(Modifier, Side),
    Escape,
    /// Any other key: its virtual key on the active layout, and its scan code
    /// with 0x100 marking an extended key (0 when an injected event has none).
    Other(u8, u16),
}

/// The operating system's view of which modifiers are down, either side.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ModMask(u8);

impl ModMask {
    pub fn with(self, modifier: Modifier) -> Self {
        Self(self.0 | 1 << modifier as u8)
    }

    fn has(self, modifier: Modifier) -> bool {
        self.0 & (1 << modifier as u8) != 0
    }
}

/// What the tracker reports to the gesture thread.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChordSignal {
    /// The whole chord is held and nothing else interfered.
    Pressed,
    /// The chord was held and one of its keys went up.
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
    /// Inject the mask key now, while the chord is still held.
    pub inject_mask: bool,
    pub signal: Option<ChordSignal>,
}

impl Verdict {
    const PASS: Self = Self {
        pass: true,
        inject_mask: false,
        signal: None,
    };
    const SWALLOW: Self = Self {
        pass: false,
        inject_mask: false,
        signal: None,
    };
}

#[derive(Debug)]
pub struct ChordTracker {
    chord: TriggerChord,
    /// Every modifier, chord or not: a chord forms only with exactly its own
    /// modifiers down, so Ctrl+Shift+Win is not Ctrl+Win.
    held: [SideSet; 4],
    /// The US scan code of the chord's ordinary key, 0 when it has none.
    key_scan: u16,
    /// The chord's ordinary key is down.
    key_held: bool,
    /// Its key-down was swallowed; swallow its repeats and key-up as well.
    key_swallowed: bool,
    /// The chord is held and reported as Pressed.
    active: bool,
    /// Some other key went down while a chord modifier was held. The chord
    /// cannot form again until the chord's keys are all up, so Ctrl+C followed
    /// by Win, or Win+Shift+S, never starts a Recording.
    poisoned: bool,
    /// An Esc down was swallowed; swallow its repeats and its key-up as well,
    /// so the focused app never sees half an Esc.
    escape_swallowed: bool,
}

impl Default for ChordTracker {
    fn default() -> Self {
        Self::new(TriggerChord::default())
    }
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

    fn meets(self, need: Option<Need>) -> bool {
        match need {
            None => !self.any(),
            Some(Need::Either) => self.any(),
            Some(Need::Only(Side::Left)) => self.left && !self.right,
            Some(Need::Only(Side::Right)) => self.right && !self.left,
        }
    }
}

impl ChordTracker {
    pub fn new(chord: TriggerChord) -> Self {
        Self {
            chord,
            key_scan: chord.key().and_then(scan_code).unwrap_or(0),
            held: [SideSet::default(); 4],
            key_held: false,
            key_swallowed: false,
            active: false,
            poisoned: false,
            escape_swallowed: false,
        }
    }

    /// Clears all tracked state, for example after the hook is reinstalled and
    /// key-ups may have been missed.
    pub fn reset(&mut self) {
        *self = Self::new(self.chord);
    }

    /// Switches to another chord, forgetting all tracked state.
    pub fn set_chord(&mut self, chord: TriggerChord) {
        *self = Self::new(chord);
    }

    /// Feeds one key transition.
    ///
    /// `os_held` is the operating system's current view of the modifiers. It
    /// is read only on a fresh modifier press, so a key-up the hook missed
    /// (secure desktop, lock screen) cannot leave a stale modifier that forms
    /// or blocks a chord. `armed` is true while the gesture thread has a
    /// Recording or a pending tap in flight, which is when Esc means Cancel.
    pub fn handle(&mut self, key: Key, down: bool, os_held: ModMask, armed: bool) -> Verdict {
        match key {
            Key::Mod(modifier, side) => self.modifier(modifier, side, down, os_held),
            Key::Escape => self.escape(down, armed),
            Key::Other(vk, scan) if self.is_chord_key(vk, scan) => self.chord_key(down),
            Key::Other(..) => self.other(down),
        }
    }

    /// Which injected events the hook may feed in. Voisu's own tagged keys
    /// never, so the mask cannot feed back; injected modifiers never, so an
    /// app typing on the user's behalf cannot form a chord; the chord's own
    /// key yes, so a key a remapper such as PowerToys turns into F24 works.
    pub fn admits_injected(&self, key: Key, tagged: bool) -> bool {
        !tagged && matches!(key, Key::Other(vk, scan) if self.is_chord_key(vk, scan))
    }

    /// The chord's key is a physical position: matched by scan code, so a
    /// layout that moves the virtual key does not move the Trigger Key. An
    /// injected event without a scan code falls back to the virtual key.
    fn is_chord_key(&self, vk: u8, scan: u16) -> bool {
        match self.chord.key() {
            None => false,
            Some(key) if scan == 0 || self.key_scan == 0 => vk == key,
            Some(_) => scan == self.key_scan,
        }
    }

    fn mods_satisfied(&self) -> bool {
        Modifier::ALL
            .iter()
            .all(|&m| self.held[m as usize].meets(self.chord.need(m)))
    }

    /// Whether an active chord is still held. A chord with an ordinary key
    /// follows that key alone, so "hold Ctrl+Shift, tap Space twice" is a
    /// double tap and letting go of the modifiers mid-Recording does not
    /// stop it.
    fn holding(&self) -> bool {
        if self.chord.key().is_some() {
            self.key_held
        } else {
            self.mods_satisfied()
        }
    }

    fn chord_mod_held(&self) -> bool {
        Modifier::ALL
            .iter()
            .any(|&m| self.chord.uses(m) && self.held[m as usize].any())
    }

    fn press(&mut self, pass: bool) -> Verdict {
        self.active = true;
        Verdict {
            pass,
            inject_mask: self.chord.needs_mask(),
            signal: Some(ChordSignal::Pressed),
        }
    }

    /// Ends a chord whose keys went up, and lifts the poison once they all are.
    fn after_release(&mut self, mut verdict: Verdict) -> Verdict {
        if self.active && !self.holding() {
            self.active = false;
            verdict.signal = Some(ChordSignal::Released);
        }
        if !self.chord_mod_held() && !self.key_held {
            self.poisoned = false;
        }
        verdict
    }

    fn modifier(&mut self, modifier: Modifier, side: Side, down: bool, os: ModMask) -> Verdict {
        let index = modifier as usize;
        if !down {
            self.held[index].set(side, false);
            return self.after_release(Verdict::PASS);
        }
        if self.held[index].get(side) {
            // Auto-repeat of a held modifier.
            return Verdict::PASS;
        }
        let fresh = !self.held[index].any();
        self.held[index].set(side, true);
        if fresh {
            for other in Modifier::ALL {
                if other != modifier && !os.has(other) {
                    // The OS says it is up, but no key-up reached the hook:
                    // drop the stale state.
                    self.held[other as usize] = SideSet::default();
                }
            }
        }
        if !self.chord.uses(modifier) || (self.active && !self.holding()) {
            // A modifier outside the chord, or the wrong side of a one-sided
            // chord modifier, breaks it like any other key.
            return self.other(true);
        }
        if self.chord.key().is_none() && !self.active && !self.poisoned && self.mods_satisfied() {
            return self.press(true);
        }
        Verdict::PASS
    }

    fn chord_key(&mut self, down: bool) -> Verdict {
        if !down {
            self.key_held = false;
            let pass = !std::mem::take(&mut self.key_swallowed);
            return self.after_release(Verdict {
                pass,
                ..Verdict::PASS
            });
        }
        if self.key_held {
            // Auto-repeat.
            return if self.key_swallowed {
                Verdict::SWALLOW
            } else {
                Verdict::PASS
            };
        }
        self.key_held = true;
        if !self.active && !self.poisoned && self.mods_satisfied() {
            self.key_swallowed = true;
            return self.press(false);
        }
        // Typed outside the chord: an ordinary key.
        self.other(true)
    }

    fn escape(&mut self, down: bool, armed: bool) -> Verdict {
        if down {
            if self.escape_swallowed {
                // Auto-repeat of the swallowed Esc.
                return Verdict::SWALLOW;
            }
            if armed || self.active {
                self.escape_swallowed = true;
                return Verdict {
                    signal: Some(ChordSignal::Escape),
                    ..Verdict::SWALLOW
                };
            }
            return self.other(true);
        }
        if std::mem::take(&mut self.escape_swallowed) {
            return Verdict::SWALLOW;
        }
        Verdict::PASS
    }

    fn other(&mut self, down: bool) -> Verdict {
        if !down || !(self.chord_mod_held() || self.active) {
            return Verdict::PASS;
        }
        self.poisoned = true;
        if std::mem::take(&mut self.active) {
            return Verdict {
                signal: Some(ChordSignal::Interrupted),
                ..Verdict::PASS
            };
        }
        Verdict::PASS
    }
}

/// What the shortcut recorder reports to the settings window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Capture {
    /// The keys pressed so far.
    Preview(TriggerChord),
    /// Everything pressed was released; this is the recorded chord.
    Done(TriggerChord),
    /// The keys pressed cannot form a chord: a key no chord can use (Enter,
    /// an arrow, a numpad key...), or both sides of one modifier.
    Invalid(Rejection),
    /// Esc ended the recording.
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaptureVerdict {
    pub pass: bool,
    pub inject_mask: bool,
    pub update: Option<Capture>,
}

/// The hook's capture mode. It records the keys held together and finishes
/// once they are all released. Modifiers still pass through, with the mask
/// key injected under Win and Alt, while ordinary keys are swallowed so
/// recording Win+D does not show the desktop.
#[derive(Debug, Default)]
pub struct Recorder {
    /// Modifier sides pressed since recording began.
    seen: [SideSet; 4],
    /// Modifier sides held now.
    held: [SideSet; 4],
    /// Ordinary keys whose key-down was swallowed and are still down, one bit
    /// per virtual key.
    down: [u64; 4],
    /// The recorded key's position, as its US virtual key.
    key: Option<u8>,
    unsupported: bool,
    /// A key went up: the chord is fixed, and later presses are ignored.
    frozen: bool,
}

impl Recorder {
    pub fn handle(&mut self, key: Key, down: bool) -> CaptureVerdict {
        let mut verdict = CaptureVerdict {
            pass: true,
            inject_mask: false,
            update: None,
        };
        match key {
            Key::Escape => {
                if down {
                    verdict.pass = false;
                    verdict.update = Some(Capture::Cancelled);
                }
                return verdict;
            }
            Key::Mod(modifier, side) => {
                let held = &mut self.held[modifier as usize];
                if down && !held.get(side) {
                    verdict.inject_mask =
                        !held.any() && matches!(modifier, Modifier::Win | Modifier::Alt);
                    held.set(side, true);
                    if !self.frozen {
                        self.seen[modifier as usize].set(side, true);
                        verdict.update = Some(Capture::Preview(self.chord()));
                    }
                } else if !down && held.get(side) {
                    held.set(side, false);
                    self.frozen = true;
                }
            }
            Key::Other(vk, scan) => {
                let (word, bit) = (usize::from(vk / 64), 1u64 << (vk % 64));
                if down {
                    verdict.pass = false;
                    if self.down[word] & bit == 0 && !self.frozen {
                        let position = match scan {
                            0 => key_name(vk).map(|_| vk),
                            _ => key_at(scan),
                        };
                        match position {
                            None => self.unsupported = true,
                            Some(key) if self.key.is_none() => {
                                self.key = Some(key);
                                verdict.update = Some(Capture::Preview(self.chord()));
                            }
                            Some(_) => {}
                        }
                    }
                    self.down[word] |= bit;
                } else if self.down[word] & bit != 0 {
                    self.down[word] &= !bit;
                    verdict.pass = false;
                    self.frozen = true;
                }
            }
        }
        if self.frozen && !self.anything_held() {
            let both_sides = self.seen.iter().any(|s| s.left && s.right);
            verdict.update = Some(if self.unsupported {
                Capture::Invalid(Rejection::Unsupported)
            } else if both_sides {
                Capture::Invalid(Rejection::BothSides)
            } else {
                Capture::Done(self.chord())
            });
        }
        verdict
    }

    /// While recording, an injected ordinary key may be recorded (a remapped
    /// F24), but never Voisu's own mask or an injected modifier.
    pub fn admits_injected(key: Key, tagged: bool) -> bool {
        !tagged && matches!(key, Key::Other(..))
    }

    fn anything_held(&self) -> bool {
        self.held.iter().any(|s| s.any()) || self.down.iter().any(|&w| w != 0)
    }

    /// Modifiers match either side, except a lone modifier, which keeps the
    /// side it was pressed on (Right Alt is not Left Alt).
    fn chord(&self) -> TriggerChord {
        let lone = self.key.is_none() && self.seen.iter().filter(|s| s.any()).count() == 1;
        let mut chord = TriggerChord::EMPTY;
        for modifier in Modifier::ALL {
            let seen = self.seen[modifier as usize];
            let need = match (seen.left, seen.right) {
                (false, false) => continue,
                (true, false) if lone => Need::Only(Side::Left),
                (false, true) if lone => Need::Only(Side::Right),
                _ => Need::Either,
            };
            chord = chord.with(modifier, need);
        }
        match self.key {
            Some(vk) => chord.with_key(vk),
            None => chord,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LCTRL: Key = Key::Mod(Modifier::Ctrl, Side::Left);
    const RCTRL: Key = Key::Mod(Modifier::Ctrl, Side::Right);
    const LWIN: Key = Key::Mod(Modifier::Win, Side::Left);
    const LALT: Key = Key::Mod(Modifier::Alt, Side::Left);
    const RALT: Key = Key::Mod(Modifier::Alt, Side::Right);
    const LSHIFT: Key = Key::Mod(Modifier::Shift, Side::Left);
    const SPACE: Key = Key::Other(0x20, 0x39);
    const D: Key = Key::Other(b'D', 0x20);
    const C: Key = Key::Other(b'C', 0x2E);
    const T: Key = Key::Other(b'T', 0x14);
    const Z: Key = Key::Other(b'Z', 0x2C);
    const ENTER: Key = Key::Other(0x0D, 0x1C);
    /// The key below Esc on a US layout.
    const BACKTICK: Key = Key::Other(0xC0, 0x29);
    const CAPS: Key = Key::Other(0x14, 0x3A);

    fn chord(text: &str) -> TriggerChord {
        text.parse().unwrap()
    }

    /// The OS view that matches the tracker's own.
    fn os(tracker: &ChordTracker) -> ModMask {
        Modifier::ALL
            .iter()
            .filter(|&&m| tracker.held[m as usize].any())
            .fold(ModMask::default(), |mask, &m| mask.with(m))
    }

    /// Feeds events with the OS agreeing with the tracker's own modifier view.
    fn feed(tracker: &mut ChordTracker, events: &[(Key, bool)], armed: bool) -> Vec<Verdict> {
        events
            .iter()
            .map(|&(key, down)| {
                let os = os(tracker);
                tracker.handle(key, down, os, armed)
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
                (D, true),
                (D, false),
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
                (D, true),
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
            &[(LCTRL, true), (C, true), (C, false), (LWIN, true)],
            false,
        );
        assert!(signals(&v).is_empty());
        // Win+Shift+S, then Ctrl.
        let mut t = ChordTracker::default();
        let v = feed(
            &mut t,
            &[(LWIN, true), (LSHIFT, true), (LCTRL, true)],
            false,
        );
        assert!(signals(&v).is_empty());
        assert!(v.iter().all(|v| !v.inject_mask && v.pass));
    }

    #[test]
    fn an_extra_modifier_held_first_is_a_different_shortcut() {
        // Ctrl+Shift+Win is not Ctrl+Win.
        let mut t = ChordTracker::default();
        let v = feed(
            &mut t,
            &[(LSHIFT, true), (LCTRL, true), (LWIN, true)],
            false,
        );
        assert!(signals(&v).is_empty());
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
        t.handle(LCTRL, true, ModMask::default(), false);
        // Win now goes down; the OS reports Ctrl as up.
        let v = t.handle(LWIN, true, ModMask::default(), false);
        assert_eq!(v.signal, None);
        assert!(!v.inject_mask);
    }

    #[test]
    fn a_missed_extra_modifier_up_does_not_block_the_chord() {
        let mut t = ChordTracker::default();
        // Shift's key-up was lost; the OS now reports only Ctrl.
        t.handle(LSHIFT, true, ModMask::default(), false);
        t.handle(LCTRL, true, ModMask::default(), false);
        let os = ModMask::default().with(Modifier::Ctrl);
        assert_eq!(
            t.handle(LWIN, true, os, false).signal,
            Some(ChordSignal::Pressed)
        );
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
        let down = t.handle(Key::Escape, true, ModMask::default(), true);
        let up = t.handle(Key::Escape, false, ModMask::default(), false);
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

    // A configured chord other than Ctrl+Win.

    #[test]
    fn ctrl_alt_presses_masks_the_menu_bar_and_releases() {
        let mut t = ChordTracker::new(chord("Ctrl+Alt"));
        let v = feed(
            &mut t,
            &[(LCTRL, true), (LALT, true), (LALT, false), (LCTRL, false)],
            false,
        );
        assert_eq!(signals(&v), [ChordSignal::Pressed, ChordSignal::Released]);
        assert!(v[1].inject_mask);
        assert!(v.iter().all(|v| v.pass));
    }

    #[test]
    fn ctrl_alt_no_longer_reacts_to_win() {
        let mut t = ChordTracker::new(chord("Ctrl+Alt"));
        let v = feed(&mut t, &[(LCTRL, true), (LWIN, true)], false);
        assert!(signals(&v).is_empty());
        assert!(v.iter().all(|v| !v.inject_mask));
    }

    #[test]
    fn ctrl_alt_is_interrupted_and_cancelled_by_escape() {
        let mut t = ChordTracker::new(chord("Ctrl+Alt"));
        let v = feed(&mut t, &[(LCTRL, true), (LALT, true), (T, true)], true);
        assert_eq!(
            signals(&v),
            [ChordSignal::Pressed, ChordSignal::Interrupted]
        );
        assert!(v[2].pass, "Ctrl+Alt+T still reaches the app");

        let mut t = ChordTracker::new(chord("Ctrl+Alt"));
        let v = feed(
            &mut t,
            &[
                (LALT, true),
                (LCTRL, true),
                (Key::Escape, true),
                (Key::Escape, false),
            ],
            false,
        );
        assert_eq!(signals(&v), [ChordSignal::Pressed, ChordSignal::Escape]);
        assert!(!v[2].pass && !v[3].pass);
    }

    #[test]
    fn ctrl_shift_masks_so_its_release_never_switches_layout() {
        let mut t = ChordTracker::new(chord("Ctrl+Shift"));
        let v = feed(&mut t, &[(LCTRL, true), (LSHIFT, true)], false);
        assert_eq!(signals(&v), [ChordSignal::Pressed]);
        assert!(v[1].inject_mask && !v[0].inject_mask);
    }

    #[test]
    fn a_one_sided_chord_ignores_the_other_side() {
        let mut t = ChordTracker::new(chord("Right Ctrl+Right Alt"));
        let v = feed(&mut t, &[(LCTRL, true), (RALT, true)], false);
        assert!(signals(&v).is_empty());

        let mut t = ChordTracker::new(chord("Right Ctrl+Right Alt"));
        let v = feed(&mut t, &[(RCTRL, true), (RALT, true), (LALT, true)], false);
        assert_eq!(
            signals(&v),
            [ChordSignal::Pressed, ChordSignal::Interrupted]
        );
    }

    #[test]
    fn a_chord_key_presses_on_its_key_and_never_types() {
        let mut t = ChordTracker::new(chord("Ctrl+Shift+Space"));
        let v = feed(
            &mut t,
            &[
                (LCTRL, true),
                (LSHIFT, true),
                (SPACE, true),
                (SPACE, true),
                (SPACE, false),
                (LSHIFT, false),
                (LCTRL, false),
            ],
            false,
        );
        assert_eq!(signals(&v), [ChordSignal::Pressed, ChordSignal::Released]);
        assert_eq!(v[2].signal, Some(ChordSignal::Pressed));
        assert_eq!(v[4].signal, Some(ChordSignal::Released));
        assert!(!v[2].pass && !v[3].pass && !v[4].pass, "Space is swallowed");
        assert!(v[0].pass && v[1].pass && v[5].pass && v[6].pass);
    }

    #[test]
    fn a_keyed_chord_follows_its_key_not_its_modifiers() {
        // Releasing Ctrl mid-Recording does not stop it; Space going up does.
        let mut t = ChordTracker::new(chord("Ctrl+Shift+Space"));
        let v = feed(
            &mut t,
            &[
                (LCTRL, true),
                (LSHIFT, true),
                (SPACE, true),
                (LCTRL, false),
                (LSHIFT, false),
                (SPACE, false),
            ],
            false,
        );
        assert_eq!(signals(&v), [ChordSignal::Pressed, ChordSignal::Released]);
        assert_eq!(v[5].signal, Some(ChordSignal::Released));
        assert!(!v[5].pass, "the key-up of a swallowed key is swallowed");
    }

    #[test]
    fn holding_the_modifiers_and_tapping_the_key_twice_double_taps() {
        let mut t = ChordTracker::new(chord("Ctrl+Shift+Space"));
        let v = feed(
            &mut t,
            &[
                (LCTRL, true),
                (LSHIFT, true),
                (SPACE, true),
                (SPACE, false),
                (SPACE, true),
                (SPACE, false),
            ],
            false,
        );
        assert_eq!(
            signals(&v),
            [
                ChordSignal::Pressed,
                ChordSignal::Released,
                ChordSignal::Pressed,
                ChordSignal::Released
            ]
        );
        assert!(v[2..].iter().all(|v| !v.pass));
    }

    #[test]
    fn ctrl_backtick_holds_to_talk_without_typing_a_backtick() {
        let mut t = ChordTracker::new(chord("Ctrl+`"));
        let v = feed(
            &mut t,
            &[
                (LCTRL, true),
                (BACKTICK, true),
                (BACKTICK, true),
                (BACKTICK, true),
                (BACKTICK, false),
                (LCTRL, false),
            ],
            false,
        );
        assert_eq!(signals(&v), [ChordSignal::Pressed, ChordSignal::Released]);
        assert!(v[1].inject_mask, "Ctrl's release is not a bare tap");
        assert!(v[1..5].iter().all(|v| !v.pass), "no backtick leaks");
        assert!(v[0].pass && v[5].pass, "Ctrl passes through");

        let mut t = ChordTracker::new(chord("Ctrl+`"));
        let v = feed(&mut t, &[(LCTRL, true), (BACKTICK, true), (D, true)], true);
        assert_eq!(
            signals(&v),
            [ChordSignal::Pressed, ChordSignal::Interrupted]
        );

        let mut t = ChordTracker::new(chord("Ctrl+`"));
        let v = feed(
            &mut t,
            &[(LCTRL, true), (BACKTICK, true), (Key::Escape, true)],
            false,
        );
        assert_eq!(signals(&v), [ChordSignal::Pressed, ChordSignal::Escape]);
    }

    #[test]
    fn the_chord_key_is_matched_by_position_not_layout() {
        // On a German layout the key below Esc reports VK_OEM_5 (^), and Ö,
        // at the US semicolon position, reports VK_OEM_3.
        let below_esc = Key::Other(0xDC, 0x29);
        let o_umlaut = Key::Other(0xC0, 0x27);
        let mut t = ChordTracker::new(chord("Ctrl+`"));
        let v = feed(&mut t, &[(LCTRL, true), (o_umlaut, true)], false);
        assert!(signals(&v).is_empty() && v[1].pass);

        let mut t = ChordTracker::new(chord("Ctrl+`"));
        let v = feed(&mut t, &[(LCTRL, true), (below_esc, true)], false);
        assert_eq!(signals(&v), [ChordSignal::Pressed]);
        assert!(!v[1].pass);
    }

    #[test]
    fn an_injected_key_without_a_scan_code_matches_by_virtual_key() {
        // A remapper such as PowerToys may inject F24 with no scan code.
        let mut t = ChordTracker::new(chord("F24"));
        let v = feed(&mut t, &[(Key::Other(0x87, 0), true)], false);
        assert_eq!(signals(&v), [ChordSignal::Pressed]);
    }

    #[test]
    fn only_the_chord_key_is_admitted_when_injected() {
        let t = ChordTracker::new(chord("F24"));
        let f24 = Key::Other(0x87, 0);
        assert!(t.admits_injected(f24, false));
        assert!(
            !t.admits_injected(f24, true),
            "Voisu's own tagged keys never"
        );
        assert!(!t.admits_injected(Key::Other(MASK_VK, 0), true));
        assert!(!t.admits_injected(LCTRL, false), "injected modifiers never");
        assert!(!t.admits_injected(D, false));
        assert!(!t.admits_injected(Key::Escape, false));
        assert!(!ChordTracker::default().admits_injected(f24, false));
        assert!(!Recorder::admits_injected(Key::Other(MASK_VK, 0), true));
        assert!(Recorder::admits_injected(f24, false));
        assert!(!Recorder::admits_injected(LWIN, false));
    }

    #[test]
    fn caps_lock_triggers_alone_and_shift_caps_lock_still_toggles_caps() {
        let mut t = ChordTracker::new(chord("CapsLock"));
        let v = feed(&mut t, &[(CAPS, true), (CAPS, true), (CAPS, false)], false);
        assert_eq!(signals(&v), [ChordSignal::Pressed, ChordSignal::Released]);
        assert!(v.iter().all(|v| !v.pass && !v.inject_mask));

        let v = feed(
            &mut t,
            &[(LSHIFT, true), (CAPS, true), (CAPS, false), (LSHIFT, false)],
            false,
        );
        assert!(signals(&v).is_empty());
        assert!(v.iter().all(|v| v.pass));
    }

    #[test]
    fn a_chord_key_without_its_modifiers_types_normally() {
        let mut t = ChordTracker::new(chord("Ctrl+Shift+Space"));
        let v = feed(
            &mut t,
            &[(SPACE, true), (SPACE, false), (LCTRL, true), (SPACE, true)],
            false,
        );
        assert!(signals(&v).is_empty());
        assert!(v.iter().all(|v| v.pass));
    }

    #[test]
    fn a_win_chord_key_is_interrupted_and_masks_start() {
        let mut t = ChordTracker::new(chord("Win+Z"));
        let v = feed(&mut t, &[(LWIN, true), (Z, true), (D, true)], true);
        assert_eq!(
            signals(&v),
            [ChordSignal::Pressed, ChordSignal::Interrupted]
        );
        assert!(v[1].inject_mask && !v[1].pass);
        assert!(v[2].pass);
    }

    #[test]
    fn set_chord_switches_live() {
        let mut t = ChordTracker::default();
        feed(&mut t, &[(LCTRL, true)], false);
        t.set_chord(chord("Ctrl+Alt"));
        let v = feed(&mut t, &[(LCTRL, true), (LALT, true)], false);
        assert_eq!(signals(&v), [ChordSignal::Pressed]);
    }

    // The shortcut recorder.

    fn record(events: &[(Key, bool)]) -> Vec<CaptureVerdict> {
        let mut r = Recorder::default();
        events.iter().map(|&(k, d)| r.handle(k, d)).collect()
    }

    fn updates(verdicts: &[CaptureVerdict]) -> Vec<Capture> {
        verdicts.iter().filter_map(|v| v.update).collect()
    }

    #[test]
    fn recording_ctrl_alt_previews_then_finishes_on_full_release() {
        let v = record(&[(LCTRL, true), (LALT, true), (LALT, false), (LCTRL, false)]);
        assert_eq!(
            updates(&v),
            [
                Capture::Preview(chord("Left Ctrl")),
                Capture::Preview(chord("Ctrl+Alt")),
                Capture::Done(chord("Ctrl+Alt")),
            ]
        );
        assert!(v.iter().all(|v| v.pass), "modifiers pass through");
        assert!(v[1].inject_mask && !v[0].inject_mask);
    }

    #[test]
    fn recording_swallows_the_ordinary_key() {
        let v = record(&[
            (LWIN, true),
            (D, true),
            (D, true),
            (D, false),
            (LWIN, false),
        ]);
        assert_eq!(updates(&v).last(), Some(&Capture::Done(chord("Win+D"))));
        assert!(v[0].inject_mask, "Start never opens while recording");
        assert!(!v[1].pass && !v[2].pass && !v[3].pass);
    }

    #[test]
    fn recording_a_lone_modifier_keeps_its_side() {
        let v = record(&[(RALT, true), (RALT, false)]);
        assert_eq!(updates(&v).last(), Some(&Capture::Done(chord("Right Alt"))));
    }

    #[test]
    fn recording_ignores_presses_after_the_first_release() {
        let v = record(&[
            (LCTRL, true),
            (LSHIFT, true),
            (LSHIFT, false),
            (LALT, true),
            (LALT, false),
            (LCTRL, false),
        ]);
        assert_eq!(
            updates(&v).last(),
            Some(&Capture::Done(chord("Ctrl+Shift")))
        );
    }

    #[test]
    fn recording_reports_an_unusable_key() {
        let v = record(&[(LCTRL, true), (ENTER, true), (ENTER, false), (LCTRL, false)]);
        assert_eq!(
            updates(&v).last(),
            Some(&Capture::Invalid(Rejection::Unsupported))
        );
    }

    #[test]
    fn recording_both_sides_of_a_modifier_is_invalid() {
        let v = record(&[(LCTRL, true), (RCTRL, true), (RCTRL, false), (LCTRL, false)]);
        assert_eq!(
            updates(&v).last(),
            Some(&Capture::Invalid(Rejection::BothSides))
        );
    }

    #[test]
    fn recording_stores_the_position_not_the_layout_key() {
        // German layout: the key below Esc reports VK_OEM_5.
        let v = record(&[
            (LCTRL, true),
            (Key::Other(0xDC, 0x29), true),
            (Key::Other(0xDC, 0x29), false),
            (LCTRL, false),
        ]);
        assert_eq!(updates(&v).last(), Some(&Capture::Done(chord("Ctrl+`"))));
    }

    #[test]
    fn escape_cancels_recording() {
        let v = record(&[(LCTRL, true), (Key::Escape, true)]);
        assert_eq!(updates(&v).last(), Some(&Capture::Cancelled));
        assert!(!v[1].pass);
    }

    #[test]
    fn key_ups_from_before_recording_pass_and_do_not_finish_it() {
        let v = record(&[(LCTRL, false), (D, false), (LALT, true)]);
        assert!(v[0].pass && v[1].pass);
        assert_eq!(updates(&v), [Capture::Preview(chord("Left Alt"))]);
    }
}
