//! The configured Trigger Key chord: which modifiers (and optionally one
//! ordinary key) must be held together. Parsed from and written to the
//! settings string, and packed into a `u16` so it can cross to the keyboard
//! hook thread inside a posted message.

use std::fmt;
use std::str::FromStr;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Modifier {
    Ctrl,
    Alt,
    Shift,
    Win,
}

impl Modifier {
    pub const ALL: [Self; 4] = [Self::Ctrl, Self::Alt, Self::Shift, Self::Win];

    const fn index(self) -> usize {
        self as usize
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Ctrl => "Ctrl",
            Self::Alt => "Alt",
            Self::Shift => "Shift",
            Self::Win => "Win",
        }
    }
}

/// Which physical side of a modifier an event came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Side {
    Left,
    Right,
}

/// How a chord needs one of its modifiers held.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Need {
    Either,
    Only(Side),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TriggerChord {
    mods: [Option<Need>; 4],
    /// A Windows virtual-key code from [`KEYS`].
    key: Option<u8>,
}

impl Default for TriggerChord {
    fn default() -> Self {
        Self::CTRL_WIN
    }
}

impl TriggerChord {
    pub const CTRL_WIN: Self = Self {
        mods: [Some(Need::Either), None, None, Some(Need::Either)],
        key: None,
    };
    /// No keys at all: what the recorder starts from. Never a valid chord.
    pub const EMPTY: Self = Self {
        mods: [None; 4],
        key: None,
    };

    pub fn need(&self, modifier: Modifier) -> Option<Need> {
        self.mods[modifier.index()]
    }

    pub fn uses(&self, modifier: Modifier) -> bool {
        self.need(modifier).is_some()
    }

    pub fn key(&self) -> Option<u8> {
        self.key
    }

    /// Whether the hook injects the mask key when the chord forms. A bare
    /// tap of a modifier means something to Windows unless another key went
    /// by while it was held: Win opens Start, Alt focuses the menu bar,
    /// Ctrl+Shift and Alt+Shift switch the keyboard layout. A swallowed chord
    /// key does not count as going by, so every chord that holds a modifier
    /// masks; a lone dedicated key (Caps Lock, F13) has nothing to mask.
    pub fn needs_mask(&self) -> bool {
        self.modifier_count() > 0
    }

    pub fn with(mut self, modifier: Modifier, need: Need) -> Self {
        self.mods[modifier.index()] = Some(need);
        self
    }

    pub fn with_key(mut self, vk: u8) -> Self {
        self.key = Some(vk);
        self
    }

    fn modifier_count(&self) -> usize {
        self.mods.iter().flatten().count()
    }

    /// Two bits per modifier, then the virtual key in the high byte.
    pub fn pack(self) -> u16 {
        let mut bits = 0u16;
        for (i, need) in self.mods.iter().enumerate() {
            let code = match need {
                None => 0,
                Some(Need::Either) => 1,
                Some(Need::Only(Side::Left)) => 2,
                Some(Need::Only(Side::Right)) => 3,
            };
            bits |= code << (i * 2);
        }
        bits | (u16::from(self.key.unwrap_or(0)) << 8)
    }

    pub fn unpack(bits: u16) -> Self {
        let mut chord = Self::EMPTY;
        for (i, need) in chord.mods.iter_mut().enumerate() {
            *need = match (bits >> (i * 2)) & 0b11 {
                0 => None,
                1 => Some(Need::Either),
                2 => Some(Need::Only(Side::Left)),
                _ => Some(Need::Only(Side::Right)),
            };
        }
        chord.key = match (bits >> 8) as u8 {
            0 => None,
            vk => Some(vk),
        };
        chord
    }

    /// Exactly these modifiers, either side, and no others.
    fn mods_are(&self, mods: &[Modifier]) -> bool {
        Modifier::ALL
            .iter()
            .all(|m| self.uses(*m) == mods.contains(m))
    }

    /// Whether this chord may be the Trigger Key. Four shapes pass: two or
    /// three modifiers (Ctrl+Win), one right-hand modifier (Right Ctrl), one
    /// dedicated key (Caps Lock, F13), or modifiers plus one key
    /// (Ctrl+Shift+Space). The rules run in order and the first match wins;
    /// see `docs/research/2026-11-03-windows-custom-trigger-key.md`.
    pub fn check(&self) -> Result<(), Rejection> {
        let mods = self.modifier_count();
        let Some(key) = self.key else {
            return match mods {
                0 => Err(Rejection::Empty),
                1 if [Modifier::Ctrl, Modifier::Alt]
                    .iter()
                    .any(|&m| self.need(m) == Some(Need::Only(Side::Right))) =>
                {
                    Ok(())
                }
                1 => Err(Rejection::LoneModifier),
                2 | 3 => Ok(()),
                _ => Err(Rejection::TooMany),
            };
        };
        let win = self.uses(Modifier::Win);
        if key == VK_PAUSE || (key == VK_F1 + 22 && win && self.uses(Modifier::Shift)) {
            // Pause sends no key-up; the Copilot key arrives as
            // Left Shift+Left Win+F23.
            return Err(Rejection::CantHold);
        }
        if win {
            return Err(Rejection::WinKey);
        }
        let reserved = key == VK_F1 + 4
            || key == VK_F1 + 10
            || (self.mods_are(&[Modifier::Alt]) && (key == VK_F1 + 3 || key == VK_SPACE))
            || (self.mods_are(&[Modifier::Ctrl]) && CTRL_EDIT_KEYS.contains(&key));
        if reserved {
            return Err(Rejection::Reserved);
        }
        if is_typing_key(key) && !self.uses(Modifier::Ctrl) && !self.uses(Modifier::Alt) {
            return Err(Rejection::TypingKey);
        }
        if mods + 1 > 3 {
            return Err(Rejection::TooMany);
        }
        Ok(())
    }

    /// A risk worth telling the user about before they save an allowed
    /// chord.
    pub fn warning(&self) -> Option<&'static str> {
        let lone_right_alt =
            self.modifier_count() == 1 && self.need(Modifier::Alt) == Some(Need::Only(Side::Right));
        let ctrl_alt = self.uses(Modifier::Ctrl) && self.uses(Modifier::Alt);
        if lone_right_alt || ctrl_alt {
            return Some(
                "On keyboard layouts with an AltGr key, typing characters such as @ or € can trigger or interrupt this shortcut.",
            );
        }
        if self.key.is_none()
            && (self.mods_are(&[Modifier::Ctrl, Modifier::Shift])
                || self.mods_are(&[Modifier::Alt, Modifier::Shift]))
        {
            return Some(
                "If more than one keyboard layout is installed, Windows may use this shortcut to switch layouts.",
            );
        }
        if self.key == Some(VK_SPACE) && self.mods_are(&[Modifier::Ctrl]) {
            return Some(
                "Ctrl+Space switches Chinese input methods and opens code completion in many editors.",
            );
        }
        if self.key == Some(VK_SPACE) && self.mods_are(&[Modifier::Ctrl, Modifier::Shift]) {
            return Some("Some apps, such as 1Password, also use Ctrl+Shift+Space.");
        }
        if self.key == Some(VK_BACKTICK) && self.mods_are(&[Modifier::Ctrl]) {
            return Some(
                "VS Code, Cursor and some terminals use Ctrl+` to toggle the terminal. Voisu will take it over while it's running.",
            );
        }
        if self.modifier_count() == 0 && self.key.is_some_and(|k| (VK_F1..VK_F1 + 12).contains(&k))
        {
            return Some(
                "Apps use F1–F12, and on many laptops they are media keys unless Fn is held.",
            );
        }
        None
    }

    /// The name shown to the user: the written form, with the key named as the
    /// active layout names that position (`Ctrl+^` on German for ``Ctrl+` ``).
    pub fn label(&self, key_label: Option<&str>) -> String {
        let written = self.to_string();
        match (self.key, key_label) {
            (Some(_), Some(local)) => match written.rsplit_once('+') {
                Some((mods, _)) => format!("{mods}+{local}"),
                None => local.to_owned(),
            },
            _ => written,
        }
    }
}

/// Why a chord cannot be the Trigger Key. Each message is shown to the user.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Rejection {
    Empty,
    CantHold,
    WinKey,
    Reserved,
    LoneModifier,
    TypingKey,
    TooMany,
    /// The recorder saw a key no chord can use (Enter, an arrow...).
    Unsupported,
    /// The recorder saw both sides of one modifier.
    BothSides,
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "Press at least one key.",
            Self::CantHold => "That key can't be held down, so it can't be the Trigger Key.",
            Self::WinKey => {
                "Windows owns Win+key shortcuts. Use Win only with other modifiers, such as Ctrl+Win."
            }
            Self::Reserved => "Windows or most apps already use that shortcut.",
            Self::LoneModifier => "On its own, only Right Ctrl or Right Alt can be the Trigger Key.",
            Self::TypingKey => "That key types text. Add Ctrl or Alt.",
            Self::TooMany => "Use at most three keys.",
            Self::Unsupported => {
                "Voisu can't use that key. Try modifiers, Caps Lock, an F-key, or Space."
            }
            Self::BothSides => "Use one side of each modifier, not both.",
        })
    }
}

/// Why a settings string is not a chord at all.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParseError {
    Empty,
    UnknownKey(String),
    Repeated(String),
    TwoKeys,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("empty shortcut"),
            Self::UnknownKey(token) => write!(f, "unknown key {token:?}"),
            Self::Repeated(token) => write!(f, "{token:?} appears twice"),
            Self::TwoKeys => f.write_str("more than one non-modifier key"),
        }
    }
}

impl fmt::Display for TriggerChord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        let mut part = |f: &mut fmt::Formatter<'_>, text: fmt::Arguments<'_>| {
            if !std::mem::take(&mut first) {
                f.write_str("+")?;
            }
            f.write_fmt(text)
        };
        for modifier in Modifier::ALL {
            match self.need(modifier) {
                None => {}
                Some(Need::Either) => part(f, format_args!("{}", modifier.name()))?,
                Some(Need::Only(Side::Left)) => part(f, format_args!("Left {}", modifier.name()))?,
                Some(Need::Only(Side::Right)) => {
                    part(f, format_args!("Right {}", modifier.name()))?
                }
            }
        }
        if let Some(vk) = self.key {
            part(f, format_args!("{}", key_name(vk).unwrap_or("?")))?;
        }
        Ok(())
    }
}

impl FromStr for TriggerChord {
    type Err = ParseError;

    /// Accepts the written form (`Ctrl+Win`, `Right Alt`, `Ctrl+Alt+Space`)
    /// case-insensitively, plus the `LCtrl`/`RAlt` and `Control`/`Windows`
    /// spellings.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let mut chord = Self::EMPTY;
        for raw in text.split('+') {
            let token = raw.trim();
            if token.is_empty() {
                return Err(ParseError::Empty);
            }
            if let Some((modifier, need)) = parse_modifier(token) {
                let slot = &mut chord.mods[modifier.index()];
                if slot.is_some() {
                    return Err(ParseError::Repeated(token.to_owned()));
                }
                *slot = Some(need);
                continue;
            }
            let vk = key_code(token).ok_or_else(|| ParseError::UnknownKey(token.to_owned()))?;
            if chord.key.is_some() {
                return Err(ParseError::TwoKeys);
            }
            chord.key = Some(vk);
        }
        Ok(chord)
    }
}

fn parse_modifier(token: &str) -> Option<(Modifier, Need)> {
    let lower = token.to_ascii_lowercase();
    let lower = lower.as_str();
    let (need, rest) = if let Some(rest) = lower.strip_prefix("left") {
        (Need::Only(Side::Left), rest.trim_start())
    } else if let Some(rest) = lower.strip_prefix("right") {
        (Need::Only(Side::Right), rest.trim_start())
    } else {
        match lower.split_at_checked(1) {
            Some(("l", rest)) if modifier_word(rest).is_some() => (Need::Only(Side::Left), rest),
            Some(("r", rest)) if modifier_word(rest).is_some() => (Need::Only(Side::Right), rest),
            _ => (Need::Either, lower),
        }
    };
    modifier_word(rest).map(|modifier| (modifier, need))
}

fn modifier_word(word: &str) -> Option<Modifier> {
    match word {
        "ctrl" | "control" => Some(Modifier::Ctrl),
        "alt" => Some(Modifier::Alt),
        "shift" => Some(Modifier::Shift),
        "win" | "windows" => Some(Modifier::Win),
        _ => None,
    }
}

/// Keys a chord may end with: the virtual key and scan code each has on a
/// US layout, and its written name. A chord's key names a physical position,
/// so `Ctrl+`` is the key below Esc even where the active layout prints ^ or
/// ² on it; the hook matches it by scan code. Scan codes above 0xFF carry the
/// extended-key bit. Letters, digits and F-keys are handled by [`key_name`]
/// and [`scan_code`] directly.
const KEYS: &[(u8, u16, &str)] = &[
    (VK_SPACE, 0x39, "Space"),
    (0x2D, 0x152, "Insert"),
    (VK_HOME, 0x147, "Home"),
    (0x21, 0x149, "PageUp"),
    (0x22, 0x151, "PageDown"),
    (VK_END, 0x14F, "End"),
    (VK_PAUSE, 0x45, "Pause"),
    (0x91, 0x46, "ScrollLock"),
    (0x14, 0x3A, "CapsLock"),
    (0x5D, 0x15D, "Menu"),
    (VK_BACKTICK, 0x29, "`"),
    (0xBD, 0x0C, "-"),
    (0xBB, 0x0D, "="),
    (0xDB, 0x1A, "["),
    (0xDD, 0x1B, "]"),
    (0xDC, 0x2B, "\\"),
    (0xBA, 0x27, ";"),
    (0xDE, 0x28, "'"),
    (0xBC, 0x33, ","),
    (0xBE, 0x34, "."),
    (0xBF, 0x35, "/"),
];
/// US scan codes of A to Z.
const LETTER_SCANS: [u8; 26] = [
    0x1E, 0x30, 0x2E, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, 0x31, 0x18, 0x19,
    0x10, 0x13, 0x1F, 0x14, 0x16, 0x2F, 0x11, 0x2D, 0x15, 0x2C,
];
/// Scan codes of F1 to F24.
const F_SCANS: [u8; 24] = [
    0x3B, 0x3C, 0x3D, 0x3E, 0x3F, 0x40, 0x41, 0x42, 0x43, 0x44, 0x57, 0x58, 0x64, 0x65, 0x66, 0x67,
    0x68, 0x69, 0x6A, 0x6B, 0x6C, 0x6D, 0x6E, 0x76,
];
const VK_F1: u8 = 0x70;
const F_KEYS: u8 = 24;
const VK_SPACE: u8 = 0x20;
const VK_PAUSE: u8 = 0x13;
const VK_HOME: u8 = 0x24;
const VK_END: u8 = 0x23;
const VK_BACKTICK: u8 = 0xC0;

/// Ctrl+key edit shortcuts nearly every app uses.
const CTRL_EDIT_KEYS: &[u8] = &[
    b'A', b'B', b'C', b'F', b'I', b'N', b'O', b'P', b'R', b'S', b'T', b'U', b'V', b'W', b'X', b'Y',
    b'Z', VK_HOME, VK_END,
];

/// Keys that type or move the caret: letters, digits, punctuation, Space and
/// the navigation block. Everything else in [`KEYS`] is a dedicated key.
fn is_typing_key(vk: u8) -> bool {
    vk.is_ascii_digit()
        || vk.is_ascii_uppercase()
        || (0xBA..=0xDE).contains(&vk)
        || matches!(vk, VK_SPACE | 0x21..=0x24 | 0x2D)
}

/// The written name of a virtual key a chord may use.
pub fn key_name(vk: u8) -> Option<&'static str> {
    const LETTERS: [&str; 36] = [
        "0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "A", "B", "C", "D", "E", "F", "G", "H",
        "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R", "S", "T", "U", "V", "W", "X", "Y", "Z",
    ];
    const F: [&str; F_KEYS as usize] = [
        "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12", "F13", "F14",
        "F15", "F16", "F17", "F18", "F19", "F20", "F21", "F22", "F23", "F24",
    ];
    match vk {
        b'0'..=b'9' => Some(LETTERS[usize::from(vk - b'0')]),
        b'A'..=b'Z' => Some(LETTERS[usize::from(vk - b'A') + 10]),
        _ if (VK_F1..VK_F1 + F_KEYS).contains(&vk) => Some(F[usize::from(vk - VK_F1)]),
        _ => KEYS.iter().find(|k| k.0 == vk).map(|k| k.2),
    }
}

/// The US-layout scan code of a chord key's position.
pub fn scan_code(vk: u8) -> Option<u16> {
    let scan = match vk {
        b'0' => 0x0B,
        b'1'..=b'9' => vk - b'1' + 0x02,
        b'A'..=b'Z' => LETTER_SCANS[usize::from(vk - b'A')],
        _ if (VK_F1..VK_F1 + F_KEYS).contains(&vk) => F_SCANS[usize::from(vk - VK_F1)],
        _ => return KEYS.iter().find(|k| k.0 == vk).map(|k| k.1),
    };
    Some(u16::from(scan))
}

/// The chord key at a physical position, by its US-layout virtual key. A
/// bounded scan of 256 codes, used only by the recorder.
pub fn key_at(scan: u16) -> Option<u8> {
    (0..=u8::MAX).find(|&vk| key_name(vk).is_some() && scan_code(vk) == Some(scan))
}

fn key_code(token: &str) -> Option<u8> {
    let bytes = token.as_bytes();
    if let [byte] = bytes
        && byte.is_ascii_alphanumeric()
    {
        return Some(byte.to_ascii_uppercase());
    }
    if let Some(n) = token
        .strip_prefix(['F', 'f'])
        .and_then(|n| n.parse::<u8>().ok())
        && (1..=F_KEYS).contains(&n)
    {
        return Some(VK_F1 + n - 1);
    }
    KEYS.iter()
        .find(|k| k.2.eq_ignore_ascii_case(token))
        .map(|k| k.0)
}

/// Settings store the written form; a value that does not parse or is not
/// allowed fails here, and the settings loader falls back to Ctrl+Win.
impl TryFrom<String> for TriggerChord {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        let chord: Self = text
            .parse()
            .map_err(|error: ParseError| error.to_string())?;
        chord.check().map_err(|rejection| rejection.to_string())?;
        Ok(chord)
    }
}

impl From<TriggerChord> for String {
    fn from(chord: TriggerChord) -> Self {
        chord.to_string()
    }
}

impl serde::Serialize for TriggerChord {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for TriggerChord {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::try_from(text).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> TriggerChord {
        text.parse().unwrap_or_else(|e| panic!("{text}: {e}"))
    }

    #[test]
    fn label_names_the_key_as_the_active_layout_does() {
        assert_eq!(parse("Ctrl+`").label(Some("^")), "Ctrl+^");
        assert_eq!(parse("CapsLock").label(Some("Feststell")), "Feststell");
        assert_eq!(parse("Ctrl+`").label(None), "Ctrl+`");
        assert_eq!(TriggerChord::CTRL_WIN.label(Some("ignored")), "Ctrl+Win");
    }

    #[test]
    fn the_default_is_ctrl_win_and_round_trips() {
        assert_eq!(TriggerChord::default().to_string(), "Ctrl+Win");
        assert_eq!(parse("Ctrl+Win"), TriggerChord::default());
        assert_eq!(TriggerChord::default().check(), Ok(()));
    }

    #[test]
    fn written_forms_round_trip_in_canonical_order() {
        for text in [
            "Ctrl+Alt",
            "Ctrl+Shift+Win",
            "Right Ctrl+Right Alt",
            "Left Ctrl+Win",
            "Ctrl+Alt+Space",
            "Alt+Shift+F9",
            "Win+Z",
            "Ctrl+Win+5",
            "Ctrl+Alt+/",
            "F13",
        ] {
            assert_eq!(parse(text).to_string(), text);
        }
        assert_eq!(parse("win+ctrl").to_string(), "Ctrl+Win");
        assert_eq!(
            parse(" shift + ALT + space ").to_string(),
            "Alt+Shift+Space"
        );
    }

    #[test]
    fn short_and_long_spellings_parse() {
        assert_eq!(parse("RAlt+LCtrl"), parse("Left Ctrl+Right Alt"));
        assert_eq!(parse("Control+Windows"), TriggerChord::CTRL_WIN);
        assert_eq!(
            parse("RightAlt+RightCtrl").to_string(),
            "Right Ctrl+Right Alt"
        );
        assert_eq!(parse("ctrl+alt+f12").to_string(), "Ctrl+Alt+F12");
        assert_eq!(parse("ctrl+alt+r").to_string(), "Ctrl+Alt+R");
        assert_eq!(parse("ctrl+alt+pageup").to_string(), "Ctrl+Alt+PageUp");
    }

    #[test]
    fn malformed_strings_do_not_parse() {
        for text in [
            "",
            "+",
            "Ctrl+",
            "Ctrl+Ctrl",
            "Ctrl+Left Ctrl",
            "Ctrl+Hyper",
            "Ctrl+A+B",
            "Ctrl+F25",
            "Ctrl+F0",
        ] {
            assert!(text.parse::<TriggerChord>().is_err(), "{text:?} parsed");
        }
    }

    #[test]
    fn pack_round_trips_every_shape() {
        for text in [
            "Ctrl+Win",
            "Right Ctrl+Right Alt",
            "Left Ctrl+Left Alt+Left Shift+Left Win",
            "Ctrl+Alt+Space",
            "Win+Z",
            "F24",
        ] {
            let chord = parse(text);
            assert_eq!(TriggerChord::unpack(chord.pack()), chord, "{text}");
        }
    }

    #[test]
    fn mask_is_needed_whenever_the_chord_holds_a_modifier() {
        assert!(parse("Ctrl+Win").needs_mask());
        assert!(parse("Ctrl+Alt").needs_mask());
        assert!(parse("Right Alt").needs_mask());
        assert!(
            parse("Ctrl+Shift").needs_mask(),
            "stops a layout switch on release"
        );
        assert!(parse("Ctrl+Shift+Space").needs_mask());
        assert!(!parse("CapsLock").needs_mask());
        assert!(!parse("F13").needs_mask());
    }

    #[test]
    fn the_four_allowed_shapes_pass() {
        for text in [
            // Two or three modifiers.
            "Ctrl+Win",
            "Alt+Win",
            "Ctrl+Alt+Win",
            "Ctrl+Shift",
            // One right-hand modifier.
            "Right Ctrl",
            "Right Alt",
            // One dedicated key.
            "CapsLock",
            "F13",
            "F24",
            "F9",
            "Menu",
            "ScrollLock",
            // Modifiers plus one key.
            "Ctrl+Shift+Space",
            "Ctrl+Alt+Space",
            "Alt+Shift+F12",
            "Ctrl+Alt+R",
        ] {
            assert_eq!(parse(text).check(), Ok(()), "{text}");
        }
    }

    #[test]
    fn rejections_follow_the_research_note() {
        for (text, why) in [
            ("Pause", Rejection::CantHold),
            ("Ctrl+Pause", Rejection::CantHold),
            ("Shift+Win+F23", Rejection::CantHold),
            ("Win+Z", Rejection::WinKey),
            ("Ctrl+Win+5", Rejection::WinKey),
            ("Win+L", Rejection::WinKey),
            ("Alt+F4", Rejection::Reserved),
            ("Alt+Space", Rejection::Reserved),
            ("Ctrl+C", Rejection::Reserved),
            ("Ctrl+V", Rejection::Reserved),
            ("Ctrl+Home", Rejection::Reserved),
            ("F5", Rejection::Reserved),
            ("Ctrl+F11", Rejection::Reserved),
            ("Ctrl", Rejection::LoneModifier),
            ("Right Shift", Rejection::LoneModifier),
            ("Left Ctrl", Rejection::LoneModifier),
            ("Left Alt", Rejection::LoneModifier),
            ("Alt", Rejection::LoneModifier),
            ("Win", Rejection::LoneModifier),
            ("Space", Rejection::TypingKey),
            ("Shift+A", Rejection::TypingKey),
            ("Shift+/", Rejection::TypingKey),
            ("Ctrl+Alt+Shift+Win", Rejection::TooMany),
            ("Ctrl+Alt+Shift+Space", Rejection::TooMany),
        ] {
            assert_eq!(parse(text).check(), Err(why), "{text}");
        }
        assert_eq!(TriggerChord::EMPTY.check(), Err(Rejection::Empty));
    }

    #[test]
    fn ctrl_backtick_is_allowed_and_round_trips() {
        let chord = parse("Ctrl+`");
        assert_eq!(chord.to_string(), "Ctrl+`");
        assert_eq!(chord.check(), Ok(()));
        assert_eq!(TriggerChord::unpack(chord.pack()), chord);
        assert!(chord.warning().unwrap().contains("VS Code"));
    }

    #[test]
    fn keys_are_physical_positions_on_a_us_layout() {
        // The key below Esc, whatever the active layout prints on it.
        assert_eq!(scan_code(0xC0), Some(0x29));
        assert_eq!(key_at(0x29), Some(0xC0));
        assert_eq!(scan_code(b'Q'), Some(0x10));
        assert_eq!(scan_code(b'1'), Some(0x02));
        assert_eq!(scan_code(b'0'), Some(0x0B));
        assert_eq!(scan_code(0x7B), Some(0x58), "F12");
        assert_eq!(scan_code(0x87), Some(0x76), "F24");
        assert_eq!(scan_code(0x24), Some(0x147), "Home is an extended key");
        // Numpad 7 shares Home's scan code without the extended bit.
        assert_eq!(key_at(0x47), None);
        // Every named key maps back to itself.
        for vk in 0..=u8::MAX {
            if key_name(vk).is_some() {
                let scan = scan_code(vk).unwrap_or_else(|| panic!("{vk:#x} has no scan"));
                assert_eq!(key_at(scan), Some(vk), "{vk:#x}");
            }
        }
    }

    #[test]
    fn risky_but_allowed_chords_carry_a_warning() {
        for text in [
            "Right Alt",
            "Ctrl+Alt",
            "Ctrl+Shift",
            "Alt+Shift",
            "Ctrl+Space",
            "Ctrl+Shift+Space",
            "F1",
            "F12",
        ] {
            assert!(parse(text).warning().is_some(), "{text}");
        }
        for text in ["Ctrl+Win", "Right Ctrl", "CapsLock", "F13", "Alt+Win"] {
            assert_eq!(parse(text).warning(), None, "{text}");
        }
    }

    #[test]
    fn settings_string_rejects_what_check_rejects() {
        assert_eq!(
            TriggerChord::try_from("Ctrl+Win".to_owned()),
            Ok(TriggerChord::CTRL_WIN)
        );
        assert!(TriggerChord::try_from("Ctrl".to_owned()).is_err());
        assert!(TriggerChord::try_from("nonsense".to_owned()).is_err());
    }

    #[test]
    fn serde_uses_the_written_form() {
        let json = serde_json::to_string(&parse("Ctrl+Alt+Space")).unwrap();
        assert_eq!(json, r#""Ctrl+Alt+Space""#);
        let back: TriggerChord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, parse("Ctrl+Alt+Space"));
    }
}
