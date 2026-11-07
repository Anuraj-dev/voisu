//! The configured Trigger Key chord: which modifiers (and optionally one
//! ordinary key) must be held together. Only the [`Platform`]'s presets can
//! be the Trigger Key. Parsed from and written to the settings string, and
//! packed into a `u16` so it can cross to the keyboard hook thread inside a
//! posted message.
//!
//! The written form is the same on every platform and names the Windows keys
//! (`Ctrl+Alt`, `Right Win`). On a Mac, Alt is the Option key and Win is the
//! Command key; [`TriggerChord::label`] shows them by their Mac names.

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

    /// The settings name, the same on every platform.
    const fn name(self) -> &'static str {
        match self {
            Self::Ctrl => "Ctrl",
            Self::Alt => "Alt",
            Self::Shift => "Shift",
            Self::Win => "Win",
        }
    }
}

/// The desktop the Trigger Key is offered and shown on. Each has its own
/// presets and names the modifiers its own way.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Platform {
    Windows,
    Mac,
}

impl Platform {
    /// The platform this build runs on. Linux builds exist only for local
    /// checks and behave as Windows.
    pub const CURRENT: Self = if cfg!(target_os = "macos") {
        Self::Mac
    } else {
        Self::Windows
    };

    /// The only Trigger Keys offered here, the default first.
    pub fn presets(self) -> &'static [Preset] {
        match self {
            Self::Windows => &WINDOWS_PRESETS,
            Self::Mac => &MAC_PRESETS,
        }
    }

    pub fn default_chord(self) -> TriggerChord {
        self.presets()[0].chord
    }

    /// What the user calls the modifier on this platform's keyboard.
    const fn modifier_name(self, modifier: Modifier) -> &'static str {
        match (self, modifier) {
            (Self::Mac, Modifier::Alt) => "Option",
            (Self::Mac, Modifier::Win) => "Cmd",
            (_, modifier) => modifier.name(),
        }
    }
}

/// The Trigger Keys this platform offers, the default first.
pub fn presets() -> &'static [Preset] {
    Platform::CURRENT.presets()
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

/// The first preset: Ctrl+Win on Windows, Ctrl+Option on a Mac.
impl Default for TriggerChord {
    fn default() -> Self {
        Platform::CURRENT.default_chord()
    }
}

impl TriggerChord {
    pub const CTRL_WIN: Self = Self {
        mods: [Some(Need::Either), None, None, Some(Need::Either)],
        key: None,
    };
    /// Ctrl+Option on a Mac.
    pub const CTRL_ALT: Self = Self {
        mods: [Some(Need::Either), Some(Need::Either), None, None],
        key: None,
    };
    /// No keys at all: what parsing and unpacking start from.
    const EMPTY: Self = Self {
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
    /// macOS gives a bare modifier tap no meaning, so its event tap ignores
    /// this.
    pub fn needs_mask(&self) -> bool {
        self.modifier_count() > 0
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

    /// The preset this chord is on this platform, if any. Anything else
    /// cannot be the Trigger Key.
    pub fn preset(&self) -> Option<&'static Preset> {
        self.preset_on(Platform::CURRENT)
    }

    pub fn preset_on(&self, platform: Platform) -> Option<&'static Preset> {
        platform
            .presets()
            .iter()
            .find(|preset| preset.chord == *self)
    }

    /// The name shown to the user on this platform.
    pub fn label(&self, key_label: Option<&str>) -> String {
        self.label_on(Platform::CURRENT, key_label)
    }

    /// The written form with `platform`'s modifier names (`Ctrl+Option` on a
    /// Mac for `Ctrl+Alt`), and the key named as the active layout names that
    /// position (`Ctrl+^` on German for ``Ctrl+` ``).
    pub fn label_on(&self, platform: Platform, key_label: Option<&str>) -> String {
        let written = Named(self, platform).to_string();
        match (self.key, key_label) {
            (Some(_), Some(local)) => match written.rsplit_once('+') {
                Some((mods, _)) => format!("{mods}+{local}"),
                None => local.to_owned(),
            },
            _ => written,
        }
    }
}

/// A Trigger Key the settings window offers.
#[derive(Debug)]
pub struct Preset {
    pub chord: TriggerChord,
    /// The name on its button.
    pub name: &'static str,
    /// A risk worth telling the user about while it is chosen.
    pub warning: Option<&'static str>,
}

/// The only Trigger Keys Voisu offers on Windows, the default first. Every
/// one holds a modifier the hook masks or is a dedicated key; see
/// `docs/research/2026-11-03-windows-custom-trigger-key.md`.
pub const WINDOWS_PRESETS: [Preset; 5] = [
    Preset {
        chord: TriggerChord::CTRL_WIN,
        name: "Ctrl+Win",
        warning: None,
    },
    Preset {
        chord: TriggerChord {
            mods: [Some(Need::Either), None, None, None],
            key: Some(VK_BACKTICK),
        },
        name: "Ctrl+`",
        warning: Some(
            "VS Code, Cursor and some terminals use Ctrl+` to toggle the terminal. Voisu will take it over while it's running.",
        ),
    },
    Preset {
        chord: TriggerChord {
            mods: [Some(Need::Only(Side::Right)), None, None, None],
            key: None,
        },
        name: "Right Ctrl",
        warning: None,
    },
    Preset {
        chord: TriggerChord {
            mods: [None; 4],
            key: Some(VK_CAPSLOCK),
        },
        name: "Caps Lock",
        warning: None,
    },
    Preset {
        chord: TriggerChord {
            mods: [Some(Need::Either), None, Some(Need::Either), None],
            key: Some(VK_SPACE),
        },
        name: "Ctrl+Shift+Space",
        warning: Some("Some apps, such as 1Password, also use Ctrl+Shift+Space."),
    },
];

/// The Mac's Trigger Keys, the default first. Modifiers only: the event tap
/// listens and never swallows a key, so a chord with an ordinary key would
/// also type it. A bare modifier does nothing on a Mac, so none needs a
/// mask. Right Option is left out because many layouts type characters with
/// it, and every Ctrl+Cmd chord is left out because macOS owns them
/// (Ctrl+Cmd+Q locks the screen, Ctrl+Cmd+Space opens the emoji picker).
pub const MAC_PRESETS: [Preset; 3] = [
    Preset {
        chord: TriggerChord::CTRL_ALT,
        name: "Ctrl+Option",
        warning: None,
    },
    Preset {
        chord: TriggerChord {
            mods: [None, Some(Need::Either), None, Some(Need::Either)],
            key: None,
        },
        name: "Option+Cmd",
        warning: None,
    },
    Preset {
        chord: TriggerChord {
            mods: [None, None, None, Some(Need::Only(Side::Right))],
            key: None,
        },
        name: "Right Cmd",
        warning: Some(
            "Shortcuts typed with the right Cmd key, such as Cmd+C, briefly start and cancel a Recording.",
        ),
    },
];

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

/// The written form: the settings string, with Windows modifier names.
impl fmt::Display for TriggerChord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Named(self, Platform::Windows).fmt(f)
    }
}

/// A chord written with one platform's modifier names.
struct Named<'a>(&'a TriggerChord, Platform);

impl fmt::Display for Named<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self(chord, platform) = *self;
        let mut first = true;
        let mut part = |f: &mut fmt::Formatter<'_>, text: fmt::Arguments<'_>| {
            if !std::mem::take(&mut first) {
                f.write_str("+")?;
            }
            f.write_fmt(text)
        };
        for modifier in Modifier::ALL {
            let name = platform.modifier_name(modifier);
            match chord.need(modifier) {
                None => {}
                Some(Need::Either) => part(f, format_args!("{name}"))?,
                Some(Need::Only(Side::Left)) => part(f, format_args!("Left {name}"))?,
                Some(Need::Only(Side::Right)) => part(f, format_args!("Right {name}"))?,
            }
        }
        if let Some(vk) = chord.key {
            part(f, format_args!("{}", key_name(vk).unwrap_or("?")))?;
        }
        Ok(())
    }
}

impl FromStr for TriggerChord {
    type Err = ParseError;

    /// Accepts the written form (`Ctrl+Win`, `Right Alt`, `Ctrl+Alt+Space`)
    /// case-insensitively, plus the `LCtrl`/`RAlt` and `Control`/`Windows`
    /// spellings and the Mac names (`Option`/`Opt`, `Cmd`/`Command`).
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
        if chord.key.is_some() || chord.modifier_count() > 1 {
            // Only a lone modifier keeps its side; the hook matches every
            // other chord's modifiers on either side.
            for need in chord.mods.iter_mut().flatten() {
                *need = Need::Either;
            }
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
        "alt" | "option" | "opt" => Some(Modifier::Alt),
        "shift" => Some(Modifier::Shift),
        "win" | "windows" | "cmd" | "command" => Some(Modifier::Win),
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
    (VK_CAPSLOCK, 0x3A, "CapsLock"),
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
const VK_CAPSLOCK: u8 = 0x14;
const VK_BACKTICK: u8 = 0xC0;

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

/// Settings store the written form; a value that does not parse or is not a
/// preset on this platform fails here, and the settings loader falls back to
/// the default. A custom chord saved by v0.68 or v0.69 is one of those.
impl TryFrom<String> for TriggerChord {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        let chord: Self = text
            .parse()
            .map_err(|error: ParseError| error.to_string())?;
        match chord.preset() {
            Some(_) => Ok(chord),
            None => Err(format!("{chord} is not one of Voisu's Trigger Keys")),
        }
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
        assert_eq!(
            TriggerChord::CTRL_WIN.label_on(Platform::Windows, Some("ignored")),
            "Ctrl+Win"
        );
    }

    #[test]
    fn each_platform_defaults_to_its_first_preset() {
        assert_eq!(Platform::Windows.default_chord(), TriggerChord::CTRL_WIN);
        assert_eq!(Platform::Mac.default_chord(), TriggerChord::CTRL_ALT);
        assert_eq!(parse("Ctrl+Win"), TriggerChord::CTRL_WIN);
        assert_eq!(parse("Ctrl+Alt"), TriggerChord::CTRL_ALT);
        assert_eq!(
            TriggerChord::default(),
            Platform::CURRENT.presets()[0].chord
        );
        assert_eq!(
            Platform::CURRENT,
            if cfg!(target_os = "macos") {
                Platform::Mac
            } else {
                Platform::Windows
            }
        );
    }

    #[test]
    fn a_mac_names_option_and_cmd_but_writes_alt_and_win() {
        assert_eq!(
            TriggerChord::CTRL_ALT.label_on(Platform::Mac, None),
            "Ctrl+Option"
        );
        assert_eq!(
            TriggerChord::CTRL_ALT.label_on(Platform::Windows, None),
            "Ctrl+Alt"
        );
        assert_eq!(
            parse("Right Win").label_on(Platform::Mac, None),
            "Right Cmd"
        );
        assert_eq!(
            parse("Shift+Win+Space").label_on(Platform::Mac, None),
            "Shift+Cmd+Space"
        );
        // The settings string stays the Windows form, so one parser reads it.
        assert_eq!(parse("Option+Cmd").to_string(), "Alt+Win");
        assert_eq!(parse("ctrl+opt"), TriggerChord::CTRL_ALT);
        assert_eq!(parse("Right Command"), parse("Right Win"));
        assert_eq!(parse("RCmd"), parse("Right Win"));
    }

    #[test]
    fn every_preset_is_found_on_its_own_platform_only() {
        for platform in [Platform::Windows, Platform::Mac] {
            for preset in platform.presets() {
                assert_eq!(preset.chord.preset_on(platform).unwrap().name, preset.name);
            }
        }
        // A Mac preset's label is its button name; Windows writes Caps Lock
        // as `CapsLock`, as it always has.
        for preset in &MAC_PRESETS {
            assert_eq!(preset.chord.label_on(Platform::Mac, None), preset.name);
        }
        assert!(parse("Right Win").preset_on(Platform::Windows).is_none());
        assert!(parse("Ctrl+Win").preset_on(Platform::Mac).is_none());
    }

    #[test]
    fn mac_presets_are_modifier_only_and_avoid_system_chords() {
        let names: Vec<&str> = MAC_PRESETS.iter().map(|p| p.name).collect();
        assert_eq!(names, ["Ctrl+Option", "Option+Cmd", "Right Cmd"]);
        for preset in &MAC_PRESETS {
            let chord = preset.chord;
            assert_eq!(chord.key(), None, "{}", preset.name);
            assert!(
                !(chord.uses(Modifier::Ctrl) && chord.uses(Modifier::Win)),
                "{} is a Ctrl+Cmd chord macOS owns",
                preset.name
            );
            assert_ne!(chord, parse("Right Alt"), "Right Option types characters");
        }
        // The Windows-only presets are not Trigger Keys on a Mac, and the
        // other way round.
        for text in ["Ctrl+Win", "Ctrl+`", "CapsLock", "Ctrl+Shift+Space"] {
            assert!(parse(text).preset_on(Platform::Mac).is_none(), "{text}");
        }
        for text in ["Ctrl+Alt", "Alt+Win", "Right Win"] {
            assert!(parse(text).preset_on(Platform::Windows).is_none(), "{text}");
        }
    }

    #[test]
    fn written_forms_round_trip_in_canonical_order() {
        for text in [
            "Ctrl+Alt",
            "Ctrl+Shift+Win",
            "Right Ctrl",
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
        assert_eq!(parse("RightAlt").to_string(), "Right Alt");
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
            "Right Ctrl",
            "Left Alt",
            "Ctrl+Alt+Space",
            "Win+Z",
            "F24",
        ] {
            let chord = parse(text);
            assert_eq!(TriggerChord::unpack(chord.pack()), chord, "{text}");
        }
        let sided = TriggerChord {
            mods: [
                Some(Need::Only(Side::Left)),
                Some(Need::Only(Side::Right)),
                Some(Need::Either),
                Some(Need::Only(Side::Left)),
            ],
            key: None,
        };
        assert_eq!(TriggerChord::unpack(sided.pack()), sided);
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
    fn every_preset_round_trips_in_its_written_form() {
        let written: Vec<String> = WINDOWS_PRESETS
            .iter()
            .map(|p| p.chord.to_string())
            .collect();
        assert_eq!(
            written,
            [
                "Ctrl+Win",
                "Ctrl+`",
                "Right Ctrl",
                "CapsLock",
                "Ctrl+Shift+Space"
            ]
        );
        let written: Vec<String> = MAC_PRESETS.iter().map(|p| p.chord.to_string()).collect();
        assert_eq!(written, ["Ctrl+Alt", "Alt+Win", "Right Win"]);
        for preset in WINDOWS_PRESETS.iter().chain(&MAC_PRESETS) {
            let text = preset.chord.to_string();
            assert_eq!(parse(&text), preset.chord, "{text}");
            assert_eq!(TriggerChord::unpack(preset.chord.pack()), preset.chord);
        }
        for preset in presets() {
            let text = preset.chord.to_string();
            assert_eq!(TriggerChord::try_from(text), Ok(preset.chord));
        }
    }

    #[test]
    fn only_a_preset_can_be_the_trigger_key() {
        // Chords the v0.68 and v0.69 recorder could save, and ones it refused.
        for text in [
            "Ctrl+Alt",
            "Alt+Win",
            "Right Win",
            "Ctrl+Shift",
            "Right Alt",
            "Left Ctrl",
            "F13",
            "Alt+Shift+F12",
            "Ctrl+Alt+Space",
            "Ctrl+C",
            "Win+Z",
            "Ctrl",
        ] {
            assert!(parse(text).preset_on(Platform::Windows).is_none(), "{text}");
            if parse(text).preset().is_none() {
                assert!(TriggerChord::try_from(text.to_owned()).is_err(), "{text}");
            }
        }
        for text in ["Ctrl+Shift", "Ctrl", "Right Alt", "Ctrl+Win+Alt", "Win+Z"] {
            assert!(TriggerChord::try_from(text.to_owned()).is_err(), "{text}");
        }
        assert!(TriggerChord::try_from("nonsense".to_owned()).is_err());
    }

    #[test]
    fn chords_of_several_keys_match_either_side() {
        // Only a lone modifier keeps its side, as the note says.
        assert_eq!(parse("Right Ctrl+Right Alt"), parse("Ctrl+Alt"));
        assert_eq!(parse("Left Ctrl+Win").to_string(), "Ctrl+Win");
        assert_eq!(parse("Left Ctrl+Space"), parse("Ctrl+Space"));
        assert_eq!(parse("RAlt+LCtrl").to_string(), "Ctrl+Alt");
        assert_ne!(parse("Right Ctrl"), parse("Left Ctrl"));
    }

    #[test]
    fn ctrl_backtick_is_a_preset_and_round_trips() {
        let chord = parse("Ctrl+`");
        assert_eq!(chord.to_string(), "Ctrl+`");
        assert_eq!(TriggerChord::unpack(chord.pack()), chord);
        let preset = chord.preset_on(Platform::Windows).unwrap();
        assert!(preset.warning.unwrap().contains("VS Code"));
    }

    #[test]
    fn keys_are_physical_positions_on_a_us_layout() {
        // The key below Esc, whatever the active layout prints on it.
        assert_eq!(scan_code(0xC0), Some(0x29));
        assert_eq!(scan_code(b'Q'), Some(0x10));
        assert_eq!(scan_code(b'1'), Some(0x02));
        assert_eq!(scan_code(b'0'), Some(0x0B));
        assert_eq!(scan_code(0x7B), Some(0x58), "F12");
        assert_eq!(scan_code(0x87), Some(0x76), "F24");
        assert_eq!(scan_code(0x24), Some(0x147), "Home is an extended key");
        assert_eq!(scan_code(0x14), Some(0x3A), "Caps Lock");
        assert_eq!(scan_code(0x20), Some(0x39), "Space");
        // Every named key has a position the hook can match.
        for vk in 0..=u8::MAX {
            if key_name(vk).is_some() {
                assert!(scan_code(vk).is_some(), "{vk:#x} has no scan");
            }
        }
    }

    #[test]
    fn presets_that_clash_with_common_apps_carry_a_warning() {
        let warned = |platform: Platform| -> Vec<&str> {
            platform
                .presets()
                .iter()
                .filter(|p| p.warning.is_some())
                .map(|p| p.name)
                .collect()
        };
        assert_eq!(warned(Platform::Windows), ["Ctrl+`", "Ctrl+Shift+Space"]);
        assert_eq!(warned(Platform::Mac), ["Right Cmd"]);
    }

    #[test]
    fn serde_uses_the_written_form() {
        let json = serde_json::to_string(&parse("Ctrl+Shift+Space")).unwrap();
        assert_eq!(json, r#""Ctrl+Shift+Space""#);
        let json = serde_json::to_string(&TriggerChord::CTRL_ALT).unwrap();
        assert_eq!(json, r#""Ctrl+Alt""#);
        let last = presets().last().unwrap().chord;
        let back: TriggerChord =
            serde_json::from_str(&serde_json::to_string(&last).unwrap()).unwrap();
        assert_eq!(back, last);
        assert!(serde_json::from_str::<TriggerChord>(r#""Ctrl+Shift""#).is_err());
    }
}
