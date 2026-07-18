//! Minimal persisted daemon configuration.
//!
//! Today this holds the Deepgram Provider switch and Delivery mode. It is
//! persisted as TOML at
//! `$XDG_CONFIG_HOME/voisu/config.toml` (default `~/.config/voisu/config.toml`),
//! read once at daemon start.
//!
//! The default is **ON**: a fresh install runs the reconciled dual-Provider path
//! for the best jargon accuracy, and the user opts into the fast Groq-only path
//! with `voisu deepgram off` (or the `VOISU_DISABLE_DEEPGRAM` env override). The
//! file is
//! deliberately hand-parsed — two small root keys do not justify a full TOML
//! dependency, and the parser tolerates comments, blank lines, surrounding
//! whitespace, and unrelated keys so a hand-edited file degrades to the default
//! rather than erroring.

use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

/// The single configuration key: whether the Deepgram Provider is enabled.
const DEEPGRAM_ENABLED_KEY: &str = "deepgram_enabled";

/// The root configuration key selecting how a final Transcript is delivered.
const DELIVERY_MODE_KEY: &str = "delivery_mode";

/// How Voisu delivers a final Transcript after preserving it on the clipboard.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DeliveryMode {
    /// Preserve the Transcript and submit it through the compositor.
    #[default]
    Type,
    /// Preserve the Transcript without emulated input.
    Clipboard,
    /// Reserved for the focus guard that ships in ticket 04.
    Guarded,
}

impl DeliveryMode {
    /// The hand-authored TOML value persisted for this mode.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Type => "type",
            Self::Clipboard => "clipboard",
            Self::Guarded => "guarded",
        }
    }
}

/// Presence disables the Deepgram Provider regardless of the persisted file,
/// mirroring `VOISU_DISABLE_DIRECT_DELIVERY`/`VOISU_DISABLE_SHORTCUTS`.
const DISABLE_DEEPGRAM_ENV: &str = "VOISU_DISABLE_DEEPGRAM";

/// Deepgram is ON by default, so a fresh install runs the reconciled
/// dual-Provider path for the best jargon accuracy until the user runs
/// `voisu deepgram off`.
pub const DEFAULT_DEEPGRAM_ENABLED: bool = true;

/// Whether the Deepgram Provider is enabled for Recordings.
///
/// The env override [`DISABLE_DEEPGRAM_ENV`] wins over the persisted file: when
/// it is set, Deepgram is disabled regardless of the file. Otherwise the
/// persisted `config.toml` decides, defaulting to [`DEFAULT_DEEPGRAM_ENABLED`]
/// (ON) when the file is absent, unreadable, or does not carry the key.
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

/// The configured Delivery mode, defaulting safely to compositor submission.
/// Missing, unreadable, and unrecognised values all degrade to [`DeliveryMode::Type`].
pub fn delivery_mode() -> DeliveryMode {
    resolve_delivery_mode(read_delivery_mode(&config_path()))
}

/// Persists the Delivery mode, creating the `voisu` config directory if needed,
/// and returns the path written so the CLI can report it.
pub fn set_delivery_mode(mode: DeliveryMode) -> Result<PathBuf, String> {
    let path = config_path();
    write_delivery_mode(&path, mode)?;
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

/// Resolves a persisted Delivery mode, defaulting to the historic direct
/// Delivery behavior when no recognised root-scope value is present.
fn resolve_delivery_mode(persisted: Option<DeliveryMode>) -> DeliveryMode {
    persisted.unwrap_or_default()
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

/// Reads the persisted Delivery mode, applying the same failure handling as
/// the Deepgram switch.
fn read_delivery_mode(path: &Path) -> Option<DeliveryMode> {
    match std::fs::read_to_string(path) {
        Ok(contents) => parse_delivery_mode(&contents),
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

/// Parses the root-scope `deepgram_enabled` boolean from a minimal TOML
/// document. Comments (`#`), blank lines, surrounding whitespace, and unrelated
/// keys are ignored. Only the root table is honored: once a `[table]` (or
/// `[[array]]`) header is seen the key belongs to that table, never the root
/// toggle, so `[other]\ndeepgram_enabled = false` is ignored and the root
/// setting still decides (falling back to the default when absent). A
/// missing key or an unrecognised value yields `None` so the caller falls back
/// to the default instead of failing on a hand-edited file.
fn parse_deepgram_enabled(contents: &str) -> Option<bool> {
    for line in contents.lines() {
        let line = strip_comment(line).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            // A table header: root-scope keys are done, so the toggle is either
            // already returned above or absent from the root.
            return None;
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

/// Parses the root-scope `delivery_mode` string from the minimal TOML document.
/// Its tolerance and table scoping deliberately mirror [`parse_deepgram_enabled`].
fn parse_delivery_mode(contents: &str) -> Option<DeliveryMode> {
    for line in contents.lines() {
        let line = strip_comment(line).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            return None;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != DELIVERY_MODE_KEY {
            continue;
        }
        // Both TOML string forms are accepted so a hand-edited single-quoted
        // literal string is honored rather than silently defaulting.
        return match value.trim() {
            "\"type\"" | "'type'" => Some(DeliveryMode::Type),
            "\"clipboard\"" | "'clipboard'" => Some(DeliveryMode::Clipboard),
            "\"guarded\"" | "'guarded'" => Some(DeliveryMode::Guarded),
            _ => None,
        };
    }
    None
}

/// Returns `line` with any comment removed. Supported values are simple
/// booleans or fixed quoted strings, so a `#` anywhere begins a comment.
fn strip_comment(line: &str) -> &str {
    line.split('#').next().unwrap_or(line)
}

/// The managed comment lines emitted above the root settings. Stripped when
/// merging so a rewrite never accumulates duplicate headers.
const MANAGED_LINES: [&str; 3] = [
    "# Voisu daemon configuration.",
    "# Recording Provider and Delivery settings; read once at daemon start.",
    "# Managed by the `voisu deepgram` and `voisu delivery` commands.",
];

/// Managed header lines emitted by earlier releases. Stripped alongside
/// [`MANAGED_LINES`] so upgrading an existing config never strands stale
/// headers above the rewritten block.
const LEGACY_MANAGED_LINES: [&str; 2] = [
    "# Whether the Deepgram Provider participates in a Recording.",
    "# Managed by `voisu deepgram on|off`; read once at daemon start.",
];

/// Persists the toggle, creating the parent `voisu` directory if needed and
/// preserving any unrelated content already in the file. The write is atomic: a
/// same-directory temp file is fully written then renamed into place, so an
/// interrupted write never leaves a partially written config.
fn write_setting(path: &Path, enabled: bool) -> Result<(), String> {
    write_config(path, Some(enabled), None)
}

/// Persists the Delivery mode without discarding the Deepgram root setting.
fn write_delivery_mode(path: &Path, mode: DeliveryMode) -> Result<(), String> {
    write_config(path, None, Some(mode))
}

/// Rewrites managed root settings while preserving every other line. Only the
/// setting supplied by the caller is replaced; the other managed key remains in
/// the preserved body, so the two public setters never discard one another.
fn write_config(
    path: &Path,
    deepgram_enabled: Option<bool>,
    delivery_mode: Option<DeliveryMode>,
) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("config path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent).map_err(|error| {
        format!("cannot create config directory {}: {error}", parent.display())
    })?;
    // Only a genuinely absent file starts from empty. A permission error or
    // invalid UTF-8 must abort the write untouched — treating it as empty would
    // let the atomic replace destroy content the merge promised to preserve.
    let existing = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(format!(
                "cannot read existing config {} before writing: {error}",
                path.display()
            ));
        }
    };
    write_atomic(
        path,
        parent,
        &merge_content(&existing, deepgram_enabled, delivery_mode),
    )
}

/// Writes `contents` to `path` atomically via a same-directory temp file and a
/// rename, so a reader never observes a torn file and a crash mid-write leaves
/// the previous config intact.
fn write_atomic(path: &Path, parent: &Path, contents: &str) -> Result<(), String> {
    let mut file = tempfile::Builder::new()
        .prefix(".config.toml.")
        .tempfile_in(parent)
        .map_err(|error| format!("cannot stage config write in {}: {error}", parent.display()))?;
    file.write_all(contents.as_bytes())
        .and_then(|()| file.as_file().sync_all())
        .map_err(|error| format!("cannot write config {}: {error}", path.display()))?;
    file.persist(path)
        .map_err(|error| format!("cannot persist config {}: {}", path.display(), error.error))?;
    Ok(())
}

/// Produces the new file body: the managed root setting, followed by every
/// unrelated line preserved verbatim. The root key being replaced and managed
/// header comments are dropped so the result never duplicates them; the other
/// root key and keys under a `[table]` are preserved untouched.
fn merge_content(
    existing: &str,
    deepgram_enabled: Option<bool>,
    delivery_mode: Option<DeliveryMode>,
) -> String {
    let mut in_root = true;
    let mut preserved: Vec<&str> = Vec::new();
    for line in existing.lines() {
        let trimmed = strip_comment(line).trim();
        if trimmed.starts_with('[') {
            in_root = false;
        }
        let is_managed_comment = MANAGED_LINES.contains(&line.trim())
            || LEGACY_MANAGED_LINES.contains(&line.trim());
        let is_root_deepgram_enabled = deepgram_enabled.is_some()
            && in_root
            && trimmed
                .split_once('=')
                .is_some_and(|(key, _)| key.trim() == DEEPGRAM_ENABLED_KEY);
        let is_root_delivery_mode = delivery_mode.is_some()
            && in_root
            && trimmed
                .split_once('=')
                .is_some_and(|(key, _)| key.trim() == DELIVERY_MODE_KEY);
        if is_managed_comment || is_root_deepgram_enabled || is_root_delivery_mode {
            continue;
        }
        preserved.push(line);
    }
    let mut out = render(deepgram_enabled, delivery_mode);
    let body = preserved.join("\n");
    let body = body.trim_matches('\n');
    if !body.is_empty() {
        out.push('\n');
        out.push_str(body);
        out.push('\n');
    }
    out
}

/// Renders the managed block: the header comments and the supplied root keys.
fn render(deepgram_enabled: Option<bool>, delivery_mode: Option<DeliveryMode>) -> String {
    let mut out = String::new();
    for line in MANAGED_LINES {
        out.push_str(line);
        out.push('\n');
    }
    if let Some(enabled) = deepgram_enabled {
        out.push_str(&format!("{DEEPGRAM_ENABLED_KEY} = {enabled}\n"));
    }
    if let Some(mode) = delivery_mode {
        out.push_str(&format!("{DELIVERY_MODE_KEY} = \"{}\"\n", mode.as_str()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_on_when_nothing_is_persisted() {
        assert!(resolve(false, None));
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
    fn setting_each_root_key_preserves_the_other_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("voisu").join("config.toml");

        write_setting(&path, false).unwrap();
        write_delivery_mode(&path, DeliveryMode::Clipboard).unwrap();

        assert_eq!(read_setting(&path), Some(false));
        assert_eq!(read_delivery_mode(&path), Some(DeliveryMode::Clipboard));
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.matches(DEEPGRAM_ENABLED_KEY).count(), 1, "{contents}");
        assert_eq!(contents.matches(DELIVERY_MODE_KEY).count(), 1, "{contents}");
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
    fn delivery_modes_parse_and_unknown_or_missing_values_default_to_type() {
        assert_eq!(
            parse_delivery_mode("delivery_mode = \"type\"\n"),
            Some(DeliveryMode::Type)
        );
        assert_eq!(
            parse_delivery_mode("delivery_mode = \"clipboard\"\n"),
            Some(DeliveryMode::Clipboard)
        );
        assert_eq!(
            parse_delivery_mode("delivery_mode = \"guarded\"\n"),
            Some(DeliveryMode::Guarded)
        );
        assert_eq!(
            resolve_delivery_mode(parse_delivery_mode("delivery_mode = \"future\"\n")),
            DeliveryMode::Type
        );
        assert_eq!(parse_delivery_mode("other_key = true\n"), None);
        assert_eq!(resolve_delivery_mode(None), DeliveryMode::Type);
    }

    #[test]
    fn single_quoted_delivery_modes_are_honored() {
        assert_eq!(
            parse_delivery_mode("delivery_mode = 'clipboard'\n"),
            Some(DeliveryMode::Clipboard)
        );
        assert_eq!(
            parse_delivery_mode("delivery_mode = 'guarded'\n"),
            Some(DeliveryMode::Guarded)
        );
    }

    #[test]
    fn rewriting_a_config_from_an_earlier_release_drops_its_stale_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("voisu").join("config.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let legacy = "# Voisu daemon configuration.\n\
            # Whether the Deepgram Provider participates in a Recording.\n\
            # Managed by `voisu deepgram on|off`; read once at daemon start.\n\
            deepgram_enabled = false\n";
        std::fs::write(&path, legacy).unwrap();

        write_delivery_mode(&path, DeliveryMode::Clipboard).unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        for line in LEGACY_MANAGED_LINES {
            assert!(!contents.contains(line), "{contents}");
        }
        assert_eq!(read_setting(&path), Some(false));
        assert_eq!(read_delivery_mode(&path), Some(DeliveryMode::Clipboard));
    }

    #[test]
    fn a_rendered_file_round_trips_through_the_parser() {
        assert_eq!(
            parse_deepgram_enabled(&render(Some(true), None)),
            Some(true)
        );
        assert_eq!(
            parse_deepgram_enabled(&render(Some(false), None)),
            Some(false)
        );
    }

    #[test]
    fn a_toggle_under_a_table_is_not_read_as_the_root_setting() {
        // Real TOML scopes this key to `[other]`, so it must NOT be read as the
        // root toggle: a table-scoped key never decides the Provider.
        assert_eq!(
            parse_deepgram_enabled("[other]\ndeepgram_enabled = true\n"),
            None
        );
    }

    #[test]
    fn a_root_toggle_before_a_table_is_honoured() {
        assert_eq!(
            parse_deepgram_enabled("deepgram_enabled = true\n[other]\nx = 1\n"),
            Some(true)
        );
    }

    #[test]
    fn a_duplicate_root_toggle_takes_the_first_value() {
        assert_eq!(
            parse_deepgram_enabled("deepgram_enabled = false\ndeepgram_enabled = true\n"),
            Some(false)
        );
    }

    #[test]
    fn writing_preserves_unrelated_content_and_rewrites_the_toggle_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "# a user's own note\ndeepgram_enabled = true\n[keyterms]\nboost = 5\n",
        )
        .unwrap();
        write_setting(&path, false).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        // The toggle now reads false, exactly once, at the root.
        assert_eq!(read_setting(&path), Some(false));
        assert_eq!(contents.matches("deepgram_enabled").count(), 1, "{contents}");
        // Unrelated content survives untouched.
        assert!(contents.contains("# a user's own note"), "{contents}");
        assert!(contents.contains("[keyterms]"), "{contents}");
        assert!(contents.contains("boost = 5"), "{contents}");
    }

    #[test]
    fn writing_over_an_unreadable_file_fails_without_destroying_it() {
        // Invalid UTF-8 must not read as an absent file: treating it as empty
        // would let the atomic replace overwrite the original with only the
        // managed block, destroying the content the merge promised to preserve.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = [0xff, 0xfe, 0x00, 0x42];
        std::fs::write(&path, original).unwrap();
        assert!(
            write_setting(&path, true).is_err(),
            "an unreadable existing config must abort the write"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            original,
            "the original bytes are left untouched"
        );
    }

    #[test]
    fn repeated_writes_do_not_accumulate_managed_headers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_setting(&path, true).unwrap();
        write_setting(&path, false).unwrap();
        write_setting(&path, true).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            contents.matches(MANAGED_LINES[0]).count(),
            1,
            "the managed header appears exactly once: {contents}"
        );
        assert_eq!(read_setting(&path), Some(true));
    }
}
