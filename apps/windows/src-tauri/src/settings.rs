//! The small non-secret settings file. API keys never go here: they live in
//! Credential Manager (see `platform::secrets`).

use std::fs;
use std::io;
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};

/// The only Trigger Key the hook supports today.
pub const DEFAULT_HOTKEY: &str = "Ctrl+Win";

/// How large the recording Overlay draws. Medium is the DESIGN.md capsule.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlaySize {
    Small,
    #[default]
    Medium,
    Large,
}

impl OverlaySize {
    /// Factor applied to the whole capsule window and everything in it.
    pub fn scale(self) -> f64 {
        match self {
            Self::Small => 0.8,
            Self::Medium => 1.0,
            Self::Large => 1.25,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct Settings {
    /// Start at login is switched on once, after the first successful setup.
    /// Later the user's own choice (stored by Windows in HKCU Run) wins, so a
    /// re-save never re-enables what they turned off.
    pub autostart_default_applied: bool,
    /// The Trigger Key chord. Stored so a later release can offer a choice;
    /// the hook only recognises Ctrl+Win.
    pub hotkey: String,
    /// Paste the Transcript into the focused app. Off, it only goes to the
    /// clipboard and Last transcript.
    #[serde(deserialize_with = "lenient_on")]
    pub auto_paste: bool,
    #[serde(deserialize_with = "lenient")]
    pub overlay_size: OverlaySize,
    /// The user's boost words, normalized (see `vocabulary::normalize`).
    #[serde(deserialize_with = "lenient_terms")]
    pub boost_words: Vec<String>,
}

/// A field with a value this version does not understand (a size a later
/// release added, a hand edit) falls back to its default instead of
/// discarding every other setting with it.
fn lenient<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned + Default,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

/// [`lenient`] for a word list: a stray non-string entry is dropped on its
/// own, so it cannot erase the rest of the list on the next save.
fn lenient_terms<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    let terms = value.as_array().map(|items| {
        items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect()
    });
    Ok(terms.unwrap_or_default())
}

/// [`lenient`] for a toggle whose default is on.
fn lenient_on<'de, D: Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value.as_bool().unwrap_or(true))
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            autostart_default_applied: false,
            hotkey: DEFAULT_HOTKEY.to_owned(),
            auto_paste: true,
            overlay_size: OverlaySize::Medium,
            boost_words: Vec::new(),
        }
    }
}

impl Settings {
    /// Loads settings; a missing or unreadable file yields defaults, because
    /// nothing in it is worth refusing to start over.
    pub fn load(path: &Path) -> Self {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                tracing::warn!(%error, "settings file unreadable; using defaults");
                Self::default()
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Self::default(),
            Err(error) => {
                tracing::warn!(%error, "settings file could not be read; using defaults");
                Self::default()
            }
        }
    }

    /// Writes atomically (temp file + rename) so a crash mid-write never
    /// leaves a truncated file behind.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let dir = path
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "settings path"))?;
        fs::create_dir_all(dir)?;
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        let temp = path.with_extension("json.tmp");
        fs::write(&temp, json)?;
        fs::rename(&temp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_loads_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Settings::load(&dir.path().join("settings.json")),
            Settings::default()
        );
    }

    #[test]
    fn save_creates_the_directory_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Voisu").join("settings.json");
        let settings = Settings {
            autostart_default_applied: true,
            ..Settings::default()
        };
        settings.save(&path).unwrap();
        assert_eq!(Settings::load(&path), settings);
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn corrupt_file_loads_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, b"{not json").unwrap();
        assert_eq!(Settings::load(&path), Settings::default());
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, br#"{"autostart_default_applied":true,"future":1}"#).unwrap();
        assert!(Settings::load(&path).autostart_default_applied);
    }

    #[test]
    fn file_from_before_the_hotkey_field_gets_ctrl_win() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, br#"{"autostart_default_applied": true}"#).unwrap();
        let settings = Settings::load(&path);
        assert!(settings.autostart_default_applied);
        assert_eq!(settings.hotkey, DEFAULT_HOTKEY);
    }

    #[test]
    fn defaults_paste_automatically_at_the_medium_size_with_no_boost_words() {
        let settings = Settings::default();
        assert!(settings.auto_paste);
        assert_eq!(settings.overlay_size, OverlaySize::Medium);
        assert_eq!(settings.overlay_size.scale(), 1.0);
        assert!(settings.boost_words.is_empty());
    }

    #[test]
    fn file_from_before_the_preferences_keeps_its_values_and_gets_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            br#"{"autostart_default_applied": true, "hotkey": "Ctrl+Win"}"#,
        )
        .unwrap();
        assert_eq!(
            Settings::load(&path),
            Settings {
                autostart_default_applied: true,
                ..Settings::default()
            }
        );
    }

    #[test]
    fn preferences_round_trip_in_snake_case() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let settings = Settings {
            auto_paste: false,
            overlay_size: OverlaySize::Large,
            boost_words: vec!["Voisu".to_owned(), "Tauri".to_owned()],
            ..Settings::default()
        };
        settings.save(&path).unwrap();
        let json = fs::read_to_string(&path).unwrap();
        assert!(json.contains(r#""overlay_size": "large""#), "{json}");
        assert!(json.contains(r#""auto_paste": false"#), "{json}");
        assert_eq!(Settings::load(&path), settings);
    }

    #[test]
    fn an_unknown_or_mistyped_preference_falls_back_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            br#"{"autostart_default_applied": true, "auto_paste": "yes",
                 "overlay_size": "huge", "boost_words": ["Voisu"]}"#,
        )
        .unwrap();
        let settings = Settings::load(&path);
        assert!(settings.autostart_default_applied);
        assert!(settings.auto_paste, "a mistyped toggle keeps its default");
        assert_eq!(settings.overlay_size, OverlaySize::Medium);
        assert_eq!(settings.boost_words, ["Voisu"]);
    }

    #[test]
    fn a_stray_boost_word_entry_drops_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, br#"{"boost_words": ["Voisu", 5, null, "Tauri"]}"#).unwrap();
        assert_eq!(Settings::load(&path).boost_words, ["Voisu", "Tauri"]);
        fs::write(&path, br#"{"boost_words": "Voisu"}"#).unwrap();
        assert!(Settings::load(&path).boost_words.is_empty());
    }
}
