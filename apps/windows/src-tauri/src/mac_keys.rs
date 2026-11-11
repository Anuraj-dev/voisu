//! The Mac keyboard as the Trigger Key sees it: Quartz event key codes and
//! modifier flags turned into the [`Key`] transitions [`ChordTracker`] takes.
//! Pure, so it is tested on every platform; the macOS event tap in
//! `platform::hotkey` feeds it.
//!
//! A modifier never arrives as a key-down or key-up. It arrives as a
//! `flagsChanged` event carrying the key code of the modifier that moved and
//! the flags as they now stand. Whether that key went down or up is its
//! device-dependent bit (left and right Command each have one). Some
//! virtual keyboards and remappers leave those bits clear, so without any of
//! them the device-independent bit for the modifier decides instead.
//!
//! [`ChordTracker`]: crate::chord::ChordTracker

use crate::chord::{Key, ModMask, Modifier, Side};

/// `CGEventFlags` bits, from `CGEventTypes.h` and `IOLLEvent.h`.
pub mod flags {
    pub const SHIFT: u64 = 0x0002_0000;
    pub const CONTROL: u64 = 0x0004_0000;
    pub const ALTERNATE: u64 = 0x0008_0000;
    pub const COMMAND: u64 = 0x0010_0000;
    pub const LEFT_CONTROL: u64 = 0x0000_0001;
    pub const LEFT_SHIFT: u64 = 0x0000_0002;
    pub const RIGHT_SHIFT: u64 = 0x0000_0004;
    pub const LEFT_COMMAND: u64 = 0x0000_0008;
    pub const RIGHT_COMMAND: u64 = 0x0000_0010;
    pub const LEFT_ALTERNATE: u64 = 0x0000_0020;
    pub const RIGHT_ALTERNATE: u64 = 0x0000_0040;
    pub const RIGHT_CONTROL: u64 = 0x0000_2000;
    /// Every device-dependent modifier bit above.
    pub const DEVICE: u64 = LEFT_CONTROL
        | LEFT_SHIFT
        | RIGHT_SHIFT
        | LEFT_COMMAND
        | RIGHT_COMMAND
        | LEFT_ALTERNATE
        | RIGHT_ALTERNATE
        | RIGHT_CONTROL;
}

/// `kVK_*` virtual key codes from `HIToolbox/Events.h`.
pub mod keycode {
    pub const ANSI_V: u16 = 0x09;
    pub const COMMAND: u16 = 0x37;
    pub const SHIFT: u16 = 0x38;
    pub const CAPS_LOCK: u16 = 0x39;
    pub const OPTION: u16 = 0x3A;
    pub const CONTROL: u16 = 0x3B;
    pub const RIGHT_COMMAND: u16 = 0x36;
    pub const RIGHT_SHIFT: u16 = 0x3C;
    pub const RIGHT_OPTION: u16 = 0x3D;
    pub const RIGHT_CONTROL: u16 = 0x3E;
    pub const ESCAPE: u16 = 0x35;
}

/// Marks a Mac key code inside [`Key::Other`]'s scan code, so it is never
/// zero (which would match by virtual key) and never a Windows scan code. The
/// Mac presets have no ordinary key, so an ordinary key only ever interrupts.
const OTHER_TAG: u16 = 0x8000;

/// The modifier, side and flag bits (its own, then either side's) each
/// modifier key code moves.
fn modifier_key(code: u16) -> Option<(Modifier, Side, u64, u64)> {
    use Modifier::{Alt, Ctrl, Shift, Win};
    use Side::{Left, Right};
    let found = match code {
        keycode::CONTROL => (Ctrl, Left, flags::LEFT_CONTROL, flags::CONTROL),
        keycode::RIGHT_CONTROL => (Ctrl, Right, flags::RIGHT_CONTROL, flags::CONTROL),
        keycode::OPTION => (Alt, Left, flags::LEFT_ALTERNATE, flags::ALTERNATE),
        keycode::RIGHT_OPTION => (Alt, Right, flags::RIGHT_ALTERNATE, flags::ALTERNATE),
        keycode::SHIFT => (Shift, Left, flags::LEFT_SHIFT, flags::SHIFT),
        keycode::RIGHT_SHIFT => (Shift, Right, flags::RIGHT_SHIFT, flags::SHIFT),
        keycode::COMMAND => (Win, Left, flags::LEFT_COMMAND, flags::COMMAND),
        keycode::RIGHT_COMMAND => (Win, Right, flags::RIGHT_COMMAND, flags::COMMAND),
        _ => return None,
    };
    Some(found)
}

/// A key-down or key-up event's key. Command is the Win modifier and Option
/// is Alt, as everywhere in the Trigger Key.
pub fn key(code: u16) -> Key {
    match modifier_key(code) {
        Some((modifier, side, ..)) => Key::Mod(modifier, side),
        None if code == keycode::ESCAPE => Key::Escape,
        None => Key::Other(0, OTHER_TAG | code),
    }
}

/// What a `flagsChanged` event means, as key transitions in order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlagsChange {
    /// A modifier went down (`true`) or up.
    Modifier(Key, bool),
    /// Caps Lock toggled: a whole key press, which breaks a held chord the
    /// way any other key does.
    Tap(Key),
    /// Fn (Globe) and anything unknown: not a key the Trigger Key counts.
    Ignore,
}

pub fn flags_changed(code: u16, event_flags: u64) -> FlagsChange {
    if code == keycode::CAPS_LOCK {
        return FlagsChange::Tap(key(code));
    }
    let Some((modifier, side, device_bit, class_bit)) = modifier_key(code) else {
        return FlagsChange::Ignore;
    };
    let down = if event_flags & flags::DEVICE != 0 {
        event_flags & device_bit != 0
    } else {
        event_flags & class_bit != 0
    };
    FlagsChange::Modifier(Key::Mod(modifier, side), down)
}

/// The modifiers the event's flags say are down, either side.
pub fn modifiers(event_flags: u64) -> ModMask {
    [
        (Modifier::Ctrl, flags::CONTROL),
        (Modifier::Alt, flags::ALTERNATE),
        (Modifier::Shift, flags::SHIFT),
        (Modifier::Win, flags::COMMAND),
    ]
    .into_iter()
    .filter(|&(_, bit)| event_flags & bit != 0)
    .fold(ModMask::default(), |mask, (modifier, _)| {
        mask.with(modifier)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chord::{ChordSignal, ChordTracker};
    use crate::trigger_key::{MAC_PRESETS, TriggerChord};

    #[test]
    fn modifier_keys_are_named_by_side_and_option_and_cmd_are_alt_and_win() {
        assert_eq!(key(0x3B), Key::Mod(Modifier::Ctrl, Side::Left));
        assert_eq!(key(0x3E), Key::Mod(Modifier::Ctrl, Side::Right));
        assert_eq!(key(0x3A), Key::Mod(Modifier::Alt, Side::Left));
        assert_eq!(key(0x3D), Key::Mod(Modifier::Alt, Side::Right));
        assert_eq!(key(0x37), Key::Mod(Modifier::Win, Side::Left));
        assert_eq!(key(0x36), Key::Mod(Modifier::Win, Side::Right));
        assert_eq!(key(0x38), Key::Mod(Modifier::Shift, Side::Left));
        assert_eq!(key(0x3C), Key::Mod(Modifier::Shift, Side::Right));
        assert_eq!(key(0x35), Key::Escape);
    }

    #[test]
    fn ordinary_keys_never_look_like_a_windows_key() {
        // kVK_ANSI_A is 0, the code most likely to collide.
        assert_eq!(key(0x00), Key::Other(0, 0x8000));
        assert_eq!(key(keycode::ANSI_V), Key::Other(0, 0x8009));
    }

    #[test]
    fn a_modifier_goes_down_or_up_by_its_own_side_bit() {
        let ctrl = flags::CONTROL | flags::LEFT_CONTROL;
        let right_cmd = flags::COMMAND | flags::RIGHT_COMMAND;
        assert_eq!(
            flags_changed(keycode::CONTROL, ctrl),
            FlagsChange::Modifier(Key::Mod(Modifier::Ctrl, Side::Left), true)
        );
        // Left Command released while right Command is still held: the
        // class bit stays set, the left bit does not.
        assert_eq!(
            flags_changed(keycode::COMMAND, right_cmd),
            FlagsChange::Modifier(Key::Mod(Modifier::Win, Side::Left), false)
        );
        assert_eq!(
            flags_changed(keycode::RIGHT_COMMAND, right_cmd),
            FlagsChange::Modifier(Key::Mod(Modifier::Win, Side::Right), true)
        );
        assert_eq!(
            flags_changed(keycode::RIGHT_OPTION, 0),
            FlagsChange::Modifier(Key::Mod(Modifier::Alt, Side::Right), false)
        );
    }

    #[test]
    fn without_side_bits_the_class_bit_decides() {
        assert_eq!(
            flags_changed(keycode::OPTION, flags::ALTERNATE),
            FlagsChange::Modifier(Key::Mod(Modifier::Alt, Side::Left), true)
        );
        assert_eq!(
            flags_changed(keycode::OPTION, flags::CONTROL),
            FlagsChange::Modifier(Key::Mod(Modifier::Alt, Side::Left), false)
        );
    }

    #[test]
    fn caps_lock_is_a_tap_and_fn_is_ignored() {
        assert_eq!(
            flags_changed(keycode::CAPS_LOCK, 0x0001_0000),
            FlagsChange::Tap(Key::Other(0, 0x8039))
        );
        assert_eq!(flags_changed(0x3F, 0x0080_0000), FlagsChange::Ignore);
    }

    #[test]
    fn the_flags_name_the_modifiers_held_on_either_side() {
        let mask = modifiers(flags::CONTROL | flags::ALTERNATE | 0x0001_0000);
        assert_eq!(
            mask,
            ModMask::default().with(Modifier::Ctrl).with(Modifier::Alt)
        );
        assert_eq!(modifiers(0), ModMask::default());
    }

    /// Feeds the tracker the way the event tap does, one `flagsChanged` per
    /// modifier, with the flags each event would carry.
    fn feed(tracker: &mut ChordTracker, code: u16, event_flags: u64) -> Option<ChordSignal> {
        let FlagsChange::Modifier(key, down) = flags_changed(code, event_flags) else {
            panic!("not a modifier");
        };
        tracker
            .handle(key, down, || modifiers(event_flags), false)
            .signal
    }

    #[test]
    fn ctrl_option_starts_and_ends_a_recording() {
        let mut tracker = ChordTracker::new(TriggerChord::CTRL_ALT);
        let ctrl = flags::CONTROL | flags::LEFT_CONTROL;
        let both = ctrl | flags::ALTERNATE | flags::LEFT_ALTERNATE;
        assert_eq!(feed(&mut tracker, keycode::CONTROL, ctrl), None);
        assert_eq!(
            feed(&mut tracker, keycode::OPTION, both),
            Some(ChordSignal::Pressed)
        );
        assert_eq!(
            feed(&mut tracker, keycode::OPTION, ctrl),
            Some(ChordSignal::Released)
        );
    }

    #[test]
    fn right_cmd_ignores_the_left_one_and_a_shortcut_interrupts_it() {
        let right_cmd = MAC_PRESETS[2].chord;
        let mut tracker = ChordTracker::new(right_cmd);
        let left = flags::COMMAND | flags::LEFT_COMMAND;
        assert_eq!(feed(&mut tracker, keycode::COMMAND, left), None);
        assert_eq!(feed(&mut tracker, keycode::COMMAND, 0), None);
        let right = flags::COMMAND | flags::RIGHT_COMMAND;
        assert_eq!(
            feed(&mut tracker, keycode::RIGHT_COMMAND, right),
            Some(ChordSignal::Pressed)
        );
        // Right Cmd+C: the C goes down while the chord is held.
        let verdict = tracker.handle(key(0x08), true, || modifiers(right), false);
        assert_eq!(verdict.signal, Some(ChordSignal::Interrupted));
    }
}
