//! Minimal persisted daemon configuration.
//!
//! Today this holds a single switch — whether the Deepgram Provider
//! participates in a Recording. It is persisted as TOML at
//! `$XDG_CONFIG_HOME/voisu/config.toml` (default `~/.config/voisu/config.toml`),
//! read once at daemon start.
//!
//! The default is **OFF**: a fresh install runs the fast Groq-only path, and the
//! user opts into the dual-Provider path with `voisu deepgram on`. The file is
//! deliberately hand-parsed — one boolean key does not justify a full TOML
//! dependency, and the parser tolerates comments, blank lines, surrounding
//! whitespace, and unrelated keys so a hand-edited file degrades to the default
//! rather than erroring.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// The single configuration key: whether the Deepgram Provider is enabled.
const DEEPGRAM_ENABLED_KEY: &str = "deepgram_enabled";

/// Presence disables the Deepgram Provider regardless of the persisted file,
/// mirroring `VOISU_DISABLE_DIRECT_DELIVERY`/`VOISU_DISABLE_SHORTCUTS`.
const DISABLE_DEEPGRAM_ENV: &str = "VOISU_DISABLE_DEEPGRAM";

/// Deepgram is OFF by default, so a fresh install lives on the fast Groq-only
/// path until the user runs `voisu deepgram on`.
pub const DEFAULT_DEEPGRAM_ENABLED: bool = false;

/// Whether the Deepgram Provider is enabled for Recordings.
///
/// The env override [`DISABLE_DEEPGRAM_ENV`] wins over the persisted file: when
/// it is set, Deepgram is disabled regardless of the file. Otherwise the
/// persisted `config.toml` decides, defaulting to [`DEFAULT_DEEPGRAM_ENABLED`]
/// (OFF) when the file is absent, unreadable, or does not carry the key.
pub fn deepgram_enabled() -> bool {
    resolve(
        std::env::var_os(DISABLE_DEEPGRAM_ENV).is_some(),
        read_setting(&config_path()),
    )
}

/// Persists the Deepgram toggle, creating the `voisu` config directory if
/// needed, and returns the path written so the CLI can report it.
pub fn set_deepgram_enabled(enabled: bool) -> Result<PathBuf, String> {
    let path = config_path();
    write_setting(&path, enabled)?;
    Ok(path)
}

/// Resolves the effective setting from the env override and the persisted value.
/// Pure so the precedence rule is testable without touching the process
/// environment or the filesystem.
fn resolve(disable_env_present: bool, persisted: Option<bool>) -> bool {
    if disable_env_present {
        return false;
    }
    persisted.unwrap_or(DEFAULT_DEEPGRAM_ENABLED)
}

/// The resolved config path: `$XDG_CONFIG_HOME/voisu/config.toml`, falling back
/// to `~/.config/voisu/config.toml`. Mirrors the user dictionary resolution.
fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    base.join("voisu").join("config.toml")
}

/// Reads the persisted Deepgram setting. A missing file yields `None` (the
/// caller applies the default); a genuine read failure surfaces a local
/// diagnostic and also yields `None` rather than masquerading as a set value.
fn read_setting(path: &Path) -> Option<bool> {
    match std::fs::read_to_string(path) {
        Ok(contents) => parse_deepgram_enabled(&contents),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => {
            eprintln!(
                "voisu: ignoring unreadable config at {}: {error}",
                path.display()
            );
            None
        }
    }
}

/// Parses the `deepgram_enabled` boolean from a minimal TOML document. Comments
/// (`#`), blank lines, surrounding whitespace, and unrelated keys are ignored.
/// A missing key or an unrecognised value yields `None` so the caller falls back
/// to the default instead of failing on a hand-edited file.
fn parse_deepgram_enabled(contents: &str) -> Option<bool> {
    for line in contents.lines() {
        let line = strip_comment(line).trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != DEEPGRAM_ENABLED_KEY {
            continue;
        }
        return match value.trim() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        };
    }
    None
}

/// Returns `line` with any comment removed. Values here are bare booleans, never
/// quoted strings, so a `#` anywhere begins a comment.
fn strip_comment(line: &str) -> &str {
    line.split('#').next().unwrap_or(line)
}

/// Writes the single-key config file, creating the parent `voisu` directory if
/// needed. The file is owned entirely by this key, so it is rewritten whole.
fn write_setting(path: &Path, enabled: bool) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            format!("cannot create config directory {}: {error}", parent.display())
        })?;
    }
    std::fs::write(path, render(enabled))
        .map_err(|error| format!("cannot write config {}: {error}", path.display()))
}

/// Renders the persisted config file body for a given toggle value.
fn render(enabled: bool) -> String {
    format!(
        "# Voisu daemon configuration.\n\
         # Whether the Deepgram Provider participates in a Recording.\n\
         # Managed by `voisu deepgram on|off`; read once at daemon start.\n\
         {DEEPGRAM_ENABLED_KEY} = {enabled}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_off_when_nothing_is_persisted() {
        assert!(!resolve(false, None));
    }

    #[test]
    fn a_persisted_value_is_honoured_in_both_directions() {
        assert!(resolve(false, Some(true)));
        assert!(!resolve(false, Some(false)));
    }

    #[test]
    fn the_disable_env_override_wins_over_an_enabled_file() {
        assert!(!resolve(true, Some(true)));
        assert!(!resolve(true, None));
    }

    #[test]
    fn a_missing_config_file_reads_as_none() {
        assert_eq!(read_setting(Path::new("/nonexistent/voisu/config.toml")), None);
    }

    #[test]
    fn writing_then_reading_round_trips_and_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("voisu").join("config.toml");
        write_setting(&path, true).unwrap();
        // A second daemon start re-reads the same file (a "restart").
        assert_eq!(read_setting(&path), Some(true));
        write_setting(&path, false).unwrap();
        assert_eq!(read_setting(&path), Some(false));
    }

    #[test]
    fn writing_creates_the_missing_config_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("voisu").join("config.toml");
        assert!(!path.parent().unwrap().exists());
        write_setting(&path, true).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn parsing_tolerates_comments_whitespace_and_unrelated_keys() {
        let contents = "\
# a header comment

  deepgram_enabled = true   # inline comment
other_key = 5
";
        assert_eq!(parse_deepgram_enabled(contents), Some(true));
    }

    #[test]
    fn a_missing_key_parses_as_none() {
        assert_eq!(parse_deepgram_enabled("other_key = true\n"), None);
    }

    #[test]
    fn a_malformed_value_parses_as_none_so_the_default_applies() {
        assert_eq!(parse_deepgram_enabled("deepgram_enabled = maybe\n"), None);
    }

    #[test]
    fn a_rendered_file_round_trips_through_the_parser() {
        assert_eq!(parse_deepgram_enabled(&render(true)), Some(true));
        assert_eq!(parse_deepgram_enabled(&render(false)), Some(false));
    }
}
