//! The hook-side half of the Trigger Key: a pure, allocation-free tracker
//! that turns raw key transitions into chord signals for the configured
//! [`TriggerChord`] and decides, per event, whether the key passes through and
//! whether the mask key must be injected.
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
use crate::trigger_key::{Need, TriggerChord, scan_code};

/// The unassigned virtual key injected as the mask, the one AutoHotkey and
/// PowerToys use. The Windows hook only.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub const MASK_VK: u8 = 0xE8;
/// `dwExtraInfo` of every key Voisu injects ("VOIS"), so the hook can tell
/// its own events from another app's.
pub const MASK_TAG: usize = 0x564F_4953;
/// `VK_PACKET`: text another app injects as Unicode, with the UTF-16 unit
/// where the scan code goes. Never a chord key.
pub const VK_PACKET: u8 = 0xE7;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key {
    Mod(Modifier, Side),
    Escape,
    /// Any other key: its virtual key on the active layout, and its scan code
    /// with 0x100 marking an extended key (0 when an injected event has none).
    Other(u8, u16),
}

impl Key {
    /// An injected event's scan code is whatever the injecting app chose, so
    /// it never identifies a position: an injected key matches by virtual
    /// key alone. The Windows hook only.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub fn injected(self) -> Self {
        match self {
            Self::Other(vk, _) => Self::Other(vk, 0),
            key => key,
        }
    }
}

/// Whether `vk`/`scan` is the key at `key_vk`/`key_scan`: by scan code, the
/// physical position, unless either side has none. `VK_PACKET` is text, never
/// a key.
fn same_key(vk: u8, scan: u16, key_vk: u8, key_scan: u16) -> bool {
    match () {
        _ if vk == VK_PACKET => false,
        _ if scan == 0 || key_scan == 0 => vk == key_vk,
        _ => scan == key_scan,
    }
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
    /// A previous chord's key, held and swallowed when the chord changed:
    /// swallow its repeats and key-up too, then forget it.
    carried: Option<(u8, u16)>,
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
            carried: None,
        }
    }

    /// Clears all tracked state, for example after the hook is reinstalled and
    /// key-ups may have been missed. Returns Interrupted when the chord was
    /// held, so the gesture thread never keeps a Recording nobody can end.
    pub fn reset(&mut self) -> Option<ChordSignal> {
        let signal = self.active.then_some(ChordSignal::Interrupted);
        *self = Self::new(self.chord);
        signal
    }

    /// Switches to another chord, forgetting the tracked modifiers.
    /// Interrupts a held chord, and keeps swallowing a key whose key-down was
    /// swallowed, so the focused app never sees a stray key-up or repeat.
    pub fn set_chord(&mut self, chord: TriggerChord) -> Option<ChordSignal> {
        let signal = self.active.then_some(ChordSignal::Interrupted);
        let carried = match self.chord.key() {
            Some(vk) if self.key_swallowed => Some((vk, self.key_scan)),
            _ => self.carried,
        };
        let escape_swallowed = self.escape_swallowed;
        *self = Self::new(chord);
        self.carried = carried;
        self.escape_swallowed = escape_swallowed;
        signal
    }

    /// Swallows the rest of a carried key: its repeats and its key-up.
    fn carried(&mut self, key: Key, down: bool) -> bool {
        match (key, self.carried) {
            (Key::Other(vk, scan), Some((key_vk, key_scan)))
                if same_key(vk, scan, key_vk, key_scan) =>
            {
                if !down {
                    self.carried = None;
                }
                true
            }
            _ => false,
        }
    }

    /// Feeds one key transition.
    ///
    /// `os_held` is the operating system's current view of the modifiers. It
    /// is read only on a fresh modifier or chord-key press, so a key-up the
    /// hook missed (secure desktop, lock screen) cannot leave a stale
    /// modifier that forms or blocks a chord. `armed` is true while the gesture thread has a
    /// Recording or a pending tap in flight, which is when Esc means Cancel.
    pub fn handle(
        &mut self,
        key: Key,
        down: bool,
        os_held: impl FnOnce() -> ModMask,
        armed: bool,
    ) -> Verdict {
        if self.carried(key, down) {
            return Verdict::SWALLOW;
        }
        match key {
            Key::Mod(modifier, side) => self.modifier(modifier, side, down, os_held),
            Key::Escape => self.escape(down, armed),
            Key::Other(vk, scan) if self.is_chord_key(vk, scan) => self.chord_key(down, os_held),
            Key::Other(..) => self.other(down),
        }
    }

    /// Which injected events the hook may feed in. Voisu's own tagged keys
    /// never, so the mask cannot feed back; injected modifiers never, so an
    /// app typing on the user's behalf cannot form a chord; the chord's own
    /// key yes, so a key a remapper such as PowerToys turns into F24 works.
    /// The Windows hook only; the Mac tap skips tagged events itself.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub fn admits_injected(&self, key: Key, tagged: bool) -> bool {
        !tagged && matches!(key, Key::Other(vk, scan) if self.is_chord_key(vk, scan))
    }

    /// The chord's key is a physical position: matched by scan code, so a
    /// layout that moves the virtual key does not move the Trigger Key. An
    /// injected event without a scan code falls back to the virtual key.
    fn is_chord_key(&self, vk: u8, scan: u16) -> bool {
        self.chord
            .key()
            .is_some_and(|key| same_key(vk, scan, key, self.key_scan))
    }

    /// Drops every tracked modifier but `except` that the OS says is up: its
    /// key-up never reached the hook. Clear-only, since the OS also counts
    /// injected modifiers the tracker deliberately ignores.
    fn reconcile(&mut self, os: ModMask, except: Option<Modifier>) {
        for modifier in Modifier::ALL {
            if Some(modifier) != except && !os.has(modifier) {
                self.held[modifier as usize] = SideSet::default();
            }
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

    fn modifier(
        &mut self,
        modifier: Modifier,
        side: Side,
        down: bool,
        os: impl FnOnce() -> ModMask,
    ) -> Verdict {
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
            self.reconcile(os(), Some(modifier));
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

    fn chord_key(&mut self, down: bool, os: impl FnOnce() -> ModMask) -> Verdict {
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
        self.reconcile(os(), None);
        if !self.chord_mod_held() {
            // Whatever poisoned the chord is over.
            self.poisoned = false;
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

#[cfg(test)]
mod tests {
    use super::*;

    const LCTRL: Key = Key::Mod(Modifier::Ctrl, Side::Left);
    const RCTRL: Key = Key::Mod(Modifier::Ctrl, Side::Right);
    const LWIN: Key = Key::Mod(Modifier::Win, Side::Left);
    const LALT: Key = Key::Mod(Modifier::Alt, Side::Left);
    const LSHIFT: Key = Key::Mod(Modifier::Shift, Side::Left);
    const SPACE: Key = Key::Other(0x20, 0x39);
    const D: Key = Key::Other(b'D', 0x20);
    const C: Key = Key::Other(b'C', 0x2E);
    const T: Key = Key::Other(b'T', 0x14);
    const Z: Key = Key::Other(b'Z', 0x2C);
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
                tracker.handle(key, down, || os, armed)
            })
            .collect()
    }

    fn signals(verdicts: &[Verdict]) -> Vec<ChordSignal> {
        verdicts.iter().filter_map(|v| v.signal).collect()
    }

    #[test]
    fn ctrl_then_win_presses_once_and_masks_start() {
        let mut t = ChordTracker::new(TriggerChord::CTRL_WIN);
        let v = feed(&mut t, &[(LCTRL, true), (LWIN, true)], false);
        assert_eq!(signals(&v), [ChordSignal::Pressed]);
        assert!(v[1].inject_mask && v[1].pass);
        assert!(!v[0].inject_mask);
    }

    #[test]
    fn win_then_ctrl_presses_too() {
        let mut t = ChordTracker::new(TriggerChord::CTRL_WIN);
        let v = feed(&mut t, &[(LWIN, true), (LCTRL, true)], false);
        assert_eq!(signals(&v), [ChordSignal::Pressed]);
        assert!(v[1].inject_mask);
    }

    #[test]
    fn modifiers_always_pass_through() {
        let mut t = ChordTracker::new(TriggerChord::CTRL_WIN);
        let v = feed(
            &mut t,
            &[(LCTRL, true), (LWIN, true), (LWIN, false), (LCTRL, false)],
            false,
        );
        assert!(v.iter().all(|v| v.pass));
    }

    #[test]
    fn auto_repeat_does_not_press_again() {
        let mut t = ChordTracker::new(TriggerChord::CTRL_WIN);
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
        let mut t = ChordTracker::new(TriggerChord::CTRL_WIN);
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
        let mut t = ChordTracker::new(TriggerChord::CTRL_WIN);
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
        let mut t = ChordTracker::new(TriggerChord::CTRL_WIN);
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
        let mut t = ChordTracker::new(TriggerChord::CTRL_WIN);
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
        let mut t = ChordTracker::new(TriggerChord::CTRL_WIN);
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
        let mut t = ChordTracker::new(TriggerChord::CTRL_WIN);
        let v = feed(
            &mut t,
            &[(LCTRL, true), (C, true), (C, false), (LWIN, true)],
            false,
        );
        assert!(signals(&v).is_empty());
        // Win+Shift+S, then Ctrl.
        let mut t = ChordTracker::new(TriggerChord::CTRL_WIN);
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
        let mut t = ChordTracker::new(TriggerChord::CTRL_WIN);
        let v = feed(
            &mut t,
            &[(LSHIFT, true), (LCTRL, true), (LWIN, true)],
            false,
        );
        assert!(signals(&v).is_empty());
    }

    #[test]
    fn plain_win_tap_is_untouched() {
        let mut t = ChordTracker::new(TriggerChord::CTRL_WIN);
        let v = feed(&mut t, &[(LWIN, true), (LWIN, false)], false);
        assert!(signals(&v).is_empty());
        assert!(v.iter().all(|v| v.pass && !v.inject_mask));
    }

    #[test]
    fn other_side_release_keeps_the_modifier_held() {
        let mut t = ChordTracker::new(TriggerChord::CTRL_WIN);
        let v = feed(
            &mut t,
            &[(LCTRL, true), (RCTRL, true), (RCTRL, false), (LWIN, true)],
            false,
        );
        assert_eq!(signals(&v), [ChordSignal::Pressed]);
    }

    #[test]
    fn missed_key_up_is_corrected_by_the_os_view() {
        let mut t = ChordTracker::new(TriggerChord::CTRL_WIN);
        // Ctrl went down, then its key-up was lost (e.g. the lock screen).
        t.handle(LCTRL, true, ModMask::default, false);
        // Win now goes down; the OS reports Ctrl as up.
        let v = t.handle(LWIN, true, ModMask::default, false);
        assert_eq!(v.signal, None);
        assert!(!v.inject_mask);
    }

    #[test]
    fn a_missed_extra_modifier_up_does_not_block_the_chord() {
        let mut t = ChordTracker::new(TriggerChord::CTRL_WIN);
        // Shift's key-up was lost; the OS now reports only Ctrl.
        t.handle(LSHIFT, true, ModMask::default, false);
        t.handle(LCTRL, true, ModMask::default, false);
        let os = ModMask::default().with(Modifier::Ctrl);
        assert_eq!(
            t.handle(LWIN, true, || os, false).signal,
            Some(ChordSignal::Pressed)
        );
    }

    #[test]
    fn escape_is_swallowed_only_when_armed() {
        let mut t = ChordTracker::new(TriggerChord::CTRL_WIN);
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
        let mut t = ChordTracker::new(TriggerChord::CTRL_WIN);
        let down = t.handle(Key::Escape, true, ModMask::default, true);
        let up = t.handle(Key::Escape, false, ModMask::default, false);
        assert!(!down.pass && !up.pass);
    }

    #[test]
    fn escape_during_the_chord_cancels_before_arming() {
        let mut t = ChordTracker::new(TriggerChord::CTRL_WIN);
        let v = feed(
            &mut t,
            &[(LCTRL, true), (LWIN, true), (Key::Escape, true)],
            false,
        );
        assert_eq!(signals(&v), [ChordSignal::Pressed, ChordSignal::Escape]);
    }

    #[test]
    fn reset_clears_held_state() {
        let mut t = ChordTracker::new(TriggerChord::CTRL_WIN);
        feed(&mut t, &[(LCTRL, true), (LWIN, true)], false);
        assert_eq!(
            t.reset(),
            Some(ChordSignal::Interrupted),
            "never a stuck Recording"
        );
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
        let mut t = ChordTracker::new(chord("Right Ctrl"));
        let v = feed(&mut t, &[(LCTRL, true), (LCTRL, false)], false);
        assert!(signals(&v).is_empty());

        let mut t = ChordTracker::new(chord("Right Ctrl"));
        let v = feed(&mut t, &[(RCTRL, true), (LCTRL, true)], false);
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
        assert!(!ChordTracker::new(TriggerChord::CTRL_WIN).admits_injected(f24, false));
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
        let mut t = ChordTracker::new(TriggerChord::CTRL_WIN);
        feed(&mut t, &[(LCTRL, true)], false);
        t.set_chord(chord("Ctrl+Alt"));
        let v = feed(&mut t, &[(LCTRL, true), (LALT, true)], false);
        assert_eq!(signals(&v), [ChordSignal::Pressed]);
    }

    #[test]
    fn injected_unicode_text_never_triggers() {
        // A password manager typing ":" as Unicode text: VK_PACKET, with the
        // UTF-16 unit 0x3A where Caps Lock's scan code would be.
        let colon = Key::Other(VK_PACKET, 0x3A);
        let mut t = ChordTracker::new(chord("CapsLock"));
        assert!(!t.admits_injected(colon, false));
        assert!(!t.admits_injected(colon.injected(), false));
        let v = feed(&mut t, &[(colon, true), (colon, false)], false);
        assert!(signals(&v).is_empty() && v.iter().all(|v| v.pass));
    }

    #[test]
    fn injected_keys_match_by_virtual_key_alone() {
        // An app injecting D with F13's scan code is still D.
        let t = ChordTracker::new(chord("F13"));
        assert!(!t.admits_injected(Key::Other(b'D', 0x64).injected(), false));
        assert!(t.admits_injected(Key::Other(0x7C, 0x64).injected(), false));
    }

    #[test]
    fn a_stale_modifier_cannot_turn_a_bare_key_into_the_chord() {
        // Ctrl went down, and its key-up went to the secure desktop (UAC).
        let mut t = ChordTracker::new(chord("Ctrl+`"));
        t.handle(LCTRL, true, ModMask::default, false);
        let v = [
            t.handle(D, true, ModMask::default, false),
            t.handle(BACKTICK, true, ModMask::default, false),
            t.handle(BACKTICK, false, ModMask::default, false),
        ];
        assert!(v.iter().all(|v| v.pass && v.signal.is_none()), "{v:?}");

        // A stale Shift no longer blocks Caps Lock.
        let mut t = ChordTracker::new(chord("CapsLock"));
        t.handle(LSHIFT, true, ModMask::default, false);
        let v = t.handle(CAPS, true, ModMask::default, false);
        assert_eq!(v.signal, Some(ChordSignal::Pressed));
    }

    #[test]
    fn changing_the_chord_while_it_is_held_interrupts_and_swallows_the_rest_of_its_key() {
        let mut t = ChordTracker::new(chord("Ctrl+`"));
        feed(&mut t, &[(LCTRL, true), (BACKTICK, true)], false);
        assert_eq!(
            t.set_chord(chord("Ctrl+Shift+Space")),
            Some(ChordSignal::Interrupted)
        );
        let v = feed(
            &mut t,
            &[(BACKTICK, true), (BACKTICK, false), (BACKTICK, true)],
            false,
        );
        assert!(
            !v[0].pass && !v[1].pass,
            "no backtick leaks after the switch"
        );
        assert!(v[2].pass, "a fresh press types again");
        assert!(signals(&v).is_empty());
        assert_eq!(t.set_chord(chord("Ctrl+Win")), None, "nothing was held");
    }
}
