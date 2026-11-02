//! The small non-secret settings file. API keys never go here: they live in
//! Credential Manager (see `platform::secrets`).

use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// The only Trigger Key the hook supports today.
pub const DEFAULT_HOTKEY: &str = "Ctrl+Win";

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
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            autostart_default_applied: false,
            hotkey: DEFAULT_HOTKEY.to_owned(),
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
}
