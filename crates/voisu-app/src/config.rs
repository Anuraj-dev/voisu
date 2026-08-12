//! Minimal persisted daemon configuration.
//!
//! Today this holds the Deepgram Provider switch, Delivery mode, Writing Mode,
//! and Developer Prompt Rendering policy. It is persisted as TOML at
//! `$XDG_CONFIG_HOME/voisu/config.toml` (default `~/.config/voisu/config.toml`),
//! read once at daemon start.
//!
//! The Deepgram default is **ON**: a fresh install runs the reconciled
//! dual-Provider path for the best jargon accuracy, and the user opts into the
//! fast Groq-only path with `voisu deepgram off` (or the
//! `VOISU_DISABLE_DEEPGRAM` env override). Writing Mode defaults to **Smart**
//! on a fresh install; unreadable files and unknown values fail closed to
//! Literal. Rendering Policy defaults to **Adaptive**; unreadable files and
//! unknown values fail closed to **Natural** (local-only safest). The file is
//! deliberately hand-parsed — a few small root keys do not justify a full TOML
//! dependency, and the parser tolerates comments, blank lines, surrounding
//! whitespace, and unrelated keys so a hand-edited file degrades to a defined
//! default rather than erroring.
//!
//! **Snapshot rule (DPR):** `rendering_policy` is intended to be snapshotted at
//! Recording start. Mid-utterance config flips must not affect in-flight work.
//! Pure resolution tests cover the snapshot contract here; full daemon wire is
//! DPR-T5. Persisting the policy does **not** change Smart Writing outcomes.

use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

/// Re-export for CLI and callers that already import config modes.
pub use voisu_core::RenderingPolicy;

/// The single configuration key: whether the Deepgram Provider is enabled.
const DEEPGRAM_ENABLED_KEY: &str = "deepgram_enabled";

/// The root configuration key selecting how a final Transcript is delivered.
const DELIVERY_MODE_KEY: &str = "delivery_mode";

/// The root configuration key selecting Smart vs Literal Writing Mode.
const WRITING_MODE_KEY: &str = "writing_mode";

/// The root configuration key selecting Developer Prompt Rendering policy.
const RENDERING_POLICY_KEY: &str = "rendering_policy";

/// Explicit rollout gate for the DPR pipeline. Only `1` or `true` enables it;
/// missing, empty, or malformed values keep Smart Writing in production.
pub const ENABLE_DPR_ENV: &str = "VOISU_ENABLE_DPR";

/// Explicit rollout gate for the small-edit formatting job. Off by default so
/// Tickets 1–2 can ship without enabling Qwen formatting. Only `1` or `true`
/// switch the formatting cloud contract off #139 derivation.
pub const ENABLE_QWEN_FORMAT_ENV: &str = "VOISU_ENABLE_QWEN_FORMAT";

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

/// How Voisu turns a Validated Transcript into a Rendered Transcript.
///
/// `Smart` applies Formatting and optional Minimal Grammar Correction.
/// `Literal` preserves spoken wording while still honoring explicit formatting
/// commands. The type is `Copy` so a Recording can snapshot the resolved mode
/// before work begins and keep that snapshot stable through Delivery.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WritingMode {
    /// Apply local Formatting and, when eligible, Minimal Grammar Correction.
    #[default]
    Smart,
    /// Preserve wording; only explicit formatting commands change the text.
    Literal,
}

impl WritingMode {
    /// The hand-authored TOML value persisted for this mode.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Smart => "smart",
            Self::Literal => "literal",
        }
    }
}

/// Outcome of loading `writing_mode` from a config source. Distinct from a
/// plain `Option` so missing (Smart default) and fail-closed (Literal) never
/// collapse into the same path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WritingModeLoad {
    /// File missing, or readable file with no root-scope `writing_mode` key.
    Missing,
    /// Unreadable file, or a present root key with an unrecognised value.
    FailClosed,
    /// A recognised `smart` or `literal` value.
    Known(WritingMode),
}

/// Outcome of loading `rendering_policy` from a config source. Distinct from a
/// plain `Option` so missing (Adaptive default) and fail-closed (Natural)
/// never collapse into the same path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RenderingPolicyLoad {
    /// File missing, or readable file with no root-scope `rendering_policy` key.
    Missing,
    /// Unreadable file, or a present root key with an unrecognised value.
    FailClosed,
    /// A recognised `natural`, `adaptive`, or `structured` value.
    Known(RenderingPolicy),
}

/// Presence disables the Deepgram Provider regardless of the persisted file,
/// mirroring `VOISU_DISABLE_DIRECT_DELIVERY`/`VOISU_DISABLE_SHORTCUTS`.
const DISABLE_DEEPGRAM_ENV: &str = "VOISU_DISABLE_DEEPGRAM";

/// Deepgram is ON by default, so a fresh install runs the reconciled
/// dual-Provider path for the best jargon accuracy until the user runs
/// `voisu deepgram off`.
pub const DEFAULT_DEEPGRAM_ENABLED: bool = true;

/// Writing Mode defaults to Smart so a fresh install gets Formatting (and
/// optional Minimal Grammar when eligible). Unreadable or invalid config fails
/// closed to Literal instead — see [`resolve_writing_mode`].
pub const DEFAULT_WRITING_MODE: WritingMode = WritingMode::Smart;

/// Rendering Policy defaults to Adaptive (constants JSON / #144). Unreadable
/// or invalid config fails closed to Natural (local-only safest) — see
/// [`resolve_rendering_policy`].
pub const DEFAULT_RENDERING_POLICY: RenderingPolicy = voisu_core::DEFAULT_RENDERING_POLICY;

/// Smart Writing remains the production path until an explicit rollout flag.
pub const DEFAULT_DPR_ENABLED: bool = false;

/// Qwen small-edit formatting stays off until a test host sets
/// [`ENABLE_QWEN_FORMAT_ENV`] to `1` or `true`. Independent of
/// [`DEFAULT_DPR_ENABLED`]: silence/outro and validate-before-format can
/// ship while this remains false.
pub const DEFAULT_QWEN_FORMAT_ENABLED: bool = false;

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

/// The configured Writing Mode.
///
/// A missing file or missing key resolves to [`DEFAULT_WRITING_MODE`] (Smart).
/// An unreadable file or a present but unrecognised value fails closed to
/// [`WritingMode::Literal`]. There is no environment override in v1.
pub fn writing_mode() -> WritingMode {
    resolve_writing_mode(read_writing_mode(&config_path()))
}

/// Persists the Writing Mode, creating the `voisu` config directory if needed,
/// and returns the path written so the CLI can report it.
pub fn set_writing_mode(mode: WritingMode) -> Result<PathBuf, String> {
    let path = config_path();
    write_writing_mode(&path, mode)?;
    Ok(path)
}

/// The configured Developer Prompt Rendering policy.
///
/// A missing file or missing key resolves to [`DEFAULT_RENDERING_POLICY`]
/// (Adaptive). An unreadable file or a present but unrecognised value fails
/// closed to [`RenderingPolicy::Natural`]. There is no environment override
/// in v1. Callers that process a Recording should snapshot this value at
/// Recording start so mid-utterance config flips do not affect in-flight work.
pub fn rendering_policy() -> RenderingPolicy {
    resolve_rendering_policy(read_rendering_policy(&config_path()))
}

/// Whether the flagged Developer Prompt Rendering pipeline is active for this
/// daemon process. This is deliberately not inferred from `rendering_policy`:
/// policy can be configured while the rollout gate remains off.
pub fn dpr_enabled() -> bool {
    parse_optional_dpr_enablement(std::env::var(ENABLE_DPR_ENV).ok().as_deref())
}

/// Whether the small-edit formatting contract is active. Independent of
/// [`dpr_enabled`]: DPR can run on the existing #139 derivation path while
/// this formatter remains off. Unset, `0`, `false`, or any value other than
/// `1`/`true` is the instant rollback switch ([`ENABLE_QWEN_FORMAT_ENV`]).
pub fn qwen_format_enabled() -> bool {
    parse_optional_dpr_enablement(std::env::var(ENABLE_QWEN_FORMAT_ENV).ok().as_deref())
}

fn parse_dpr_enablement(value: &str) -> bool {
    matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true")
}

/// Shared missing-or-explicit-true parser for independent rollout env flags.
/// `None` is the packaged default (off). Each flag is parsed on its own.
fn parse_optional_dpr_enablement(value: Option<&str>) -> bool {
    value.is_some_and(parse_dpr_enablement)
}

/// Persists the Rendering Policy, creating the `voisu` config directory if
/// needed, and returns the path written so the CLI can report it.
pub fn set_rendering_policy(policy: RenderingPolicy) -> Result<PathBuf, String> {
    let path = config_path();
    write_rendering_policy(&path, policy)?;
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

/// Pure Writing Mode resolution. Missing → Smart; fail-closed → Literal;
/// known value → that mode. No environment override participates.
fn resolve_writing_mode(loaded: WritingModeLoad) -> WritingMode {
    match loaded {
        WritingModeLoad::Missing => DEFAULT_WRITING_MODE,
        WritingModeLoad::FailClosed => WritingMode::Literal,
        WritingModeLoad::Known(mode) => mode,
    }
}

/// Pure Rendering Policy resolution. Missing → Adaptive; fail-closed →
/// Natural; known value → that policy. No environment override participates.
fn resolve_rendering_policy(loaded: RenderingPolicyLoad) -> RenderingPolicy {
    match loaded {
        RenderingPolicyLoad::Missing => DEFAULT_RENDERING_POLICY,
        RenderingPolicyLoad::FailClosed => RenderingPolicy::Natural,
        RenderingPolicyLoad::Known(policy) => policy,
    }
}

/// The `voisu` config directory: `$XDG_CONFIG_HOME/voisu`, falling back to
/// `~/.config/voisu`. Shared by the config file and the credential fallback
/// file so both honour the same XDG resolution.
pub fn config_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    base.join("voisu")
}

/// The resolved config path: `$XDG_CONFIG_HOME/voisu/config.toml`, falling back
/// to `~/.config/voisu/config.toml`. Mirrors the user dictionary resolution.
fn config_path() -> PathBuf {
    config_dir().join("config.toml")
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

/// Reads the persisted Writing Mode. A missing file is [`WritingModeLoad::Missing`]
/// (Smart default). A genuine read failure emits a bounded diagnostic and
/// returns [`WritingModeLoad::FailClosed`] (Literal).
fn read_writing_mode(path: &Path) -> WritingModeLoad {
    match std::fs::read_to_string(path) {
        Ok(contents) => parse_writing_mode(&contents),
        Err(error) if error.kind() == ErrorKind::NotFound => WritingModeLoad::Missing,
        Err(error) => {
            eprintln!(
                "voisu: ignoring unreadable config at {}: {error}",
                path.display()
            );
            WritingModeLoad::FailClosed
        }
    }
}

/// Reads the persisted Rendering Policy. A missing file is
/// [`RenderingPolicyLoad::Missing`] (Adaptive default). A genuine read failure
/// emits a bounded diagnostic and returns [`RenderingPolicyLoad::FailClosed`]
/// (Natural).
fn read_rendering_policy(path: &Path) -> RenderingPolicyLoad {
    match std::fs::read_to_string(path) {
        Ok(contents) => parse_rendering_policy(&contents),
        Err(error) if error.kind() == ErrorKind::NotFound => RenderingPolicyLoad::Missing,
        Err(error) => {
            eprintln!(
                "voisu: ignoring unreadable config at {}: {error}",
                path.display()
            );
            RenderingPolicyLoad::FailClosed
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

/// Parses the root-scope `writing_mode` string. Missing key →
/// [`WritingModeLoad::Missing`]; present but unrecognised →
/// [`WritingModeLoad::FailClosed`]; `smart`/`literal` → Known. Tolerance and
/// table scoping mirror [`parse_delivery_mode`].
fn parse_writing_mode(contents: &str) -> WritingModeLoad {
    for line in contents.lines() {
        let line = strip_comment(line).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            // A table header ends root scope; the key is absent from the root.
            return WritingModeLoad::Missing;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != WRITING_MODE_KEY {
            continue;
        }
        // Both TOML string forms are accepted so a hand-edited single-quoted
        // literal string is honored rather than fail-closing to Literal.
        return match value.trim() {
            "\"smart\"" | "'smart'" => WritingModeLoad::Known(WritingMode::Smart),
            "\"literal\"" | "'literal'" => WritingModeLoad::Known(WritingMode::Literal),
            _ => WritingModeLoad::FailClosed,
        };
    }
    WritingModeLoad::Missing
}

/// Parses the root-scope `rendering_policy` string. Missing key →
/// [`RenderingPolicyLoad::Missing`]; present but unrecognised →
/// [`RenderingPolicyLoad::FailClosed`] plus a bounded, value-free diagnostic;
/// `natural`/`adaptive`/`structured` → Known. Tolerance and table scoping
/// mirror [`parse_writing_mode`].
fn parse_rendering_policy(contents: &str) -> RenderingPolicyLoad {
    for line in contents.lines() {
        let line = strip_comment(line).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            return RenderingPolicyLoad::Missing;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != RENDERING_POLICY_KEY {
            continue;
        }
        // Both TOML string forms are accepted so a hand-edited single-quoted
        // literal string is honored rather than fail-closing to Natural.
        return match value.trim() {
            "\"natural\"" | "'natural'" => RenderingPolicyLoad::Known(RenderingPolicy::Natural),
            "\"adaptive\"" | "'adaptive'" => RenderingPolicyLoad::Known(RenderingPolicy::Adaptive),
            "\"structured\"" | "'structured'" => {
                RenderingPolicyLoad::Known(RenderingPolicy::Structured)
            }
            // Spec §12 / DAG T0: unknown or corrupt present value fails closed
            // to Natural with one bounded diagnostic (no raw config dump).
            _ => {
                eprintln!(
                    "voisu: unrecognized rendering_policy in config; failing closed to natural"
                );
                RenderingPolicyLoad::FailClosed
            }
        };
    }
    RenderingPolicyLoad::Missing
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
    "# Recording Provider, Delivery, Writing Mode, and Rendering Policy settings; read once at daemon start.",
    "# Managed by the `voisu deepgram`, `voisu delivery`, `voisu writing`, and `voisu rendering` commands.",
];

/// Managed header lines emitted by earlier releases. Stripped alongside
/// [`MANAGED_LINES`] so upgrading an existing config never strands stale
/// headers above the rewritten block.
const LEGACY_MANAGED_LINES: [&str; 6] = [
    "# Whether the Deepgram Provider participates in a Recording.",
    "# Managed by `voisu deepgram on|off`; read once at daemon start.",
    // Pre-Writing-Mode managed body lines (delivery-only era).
    "# Recording Provider and Delivery settings; read once at daemon start.",
    "# Managed by the `voisu deepgram` and `voisu delivery` commands.",
    // Pre-Rendering-Policy managed body lines (writing-mode era).
    "# Recording Provider, Delivery, and Writing Mode settings; read once at daemon start.",
    "# Managed by the `voisu deepgram`, `voisu delivery`, and `voisu writing` commands.",
];

/// Persists the toggle, creating the parent `voisu` directory if needed and
/// preserving any unrelated content already in the file. The write is atomic: a
/// same-directory temp file is fully written then renamed into place, so an
/// interrupted write never leaves a partially written config.
fn write_setting(path: &Path, enabled: bool) -> Result<(), String> {
    write_config(path, Some(enabled), None, None, None)
}

/// Persists the Delivery mode without discarding the other managed root keys.
fn write_delivery_mode(path: &Path, mode: DeliveryMode) -> Result<(), String> {
    write_config(path, None, Some(mode), None, None)
}

/// Persists the Writing Mode without discarding the other managed root keys.
fn write_writing_mode(path: &Path, mode: WritingMode) -> Result<(), String> {
    write_config(path, None, None, Some(mode), None)
}

/// Persists the Rendering Policy without discarding the other managed root keys.
fn write_rendering_policy(path: &Path, policy: RenderingPolicy) -> Result<(), String> {
    write_config(path, None, None, None, Some(policy))
}

/// Rewrites managed root settings while preserving every other line. Only the
/// settings supplied by the caller are replaced; the other managed keys remain
/// in the preserved body, so the public setters never discard one another.
fn write_config(
    path: &Path,
    deepgram_enabled: Option<bool>,
    delivery_mode: Option<DeliveryMode>,
    writing_mode: Option<WritingMode>,
    rendering_policy: Option<RenderingPolicy>,
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
        &merge_content(
            &existing,
            deepgram_enabled,
            delivery_mode,
            writing_mode,
            rendering_policy,
        ),
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
/// unrelated line preserved verbatim. The root keys being replaced and managed
/// header comments are dropped so the result never duplicates them; the other
/// root keys and keys under a `[table]` are preserved untouched.
fn merge_content(
    existing: &str,
    deepgram_enabled: Option<bool>,
    delivery_mode: Option<DeliveryMode>,
    writing_mode: Option<WritingMode>,
    rendering_policy: Option<RenderingPolicy>,
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
        let is_root_writing_mode = writing_mode.is_some()
            && in_root
            && trimmed
                .split_once('=')
                .is_some_and(|(key, _)| key.trim() == WRITING_MODE_KEY);
        let is_root_rendering_policy = rendering_policy.is_some()
            && in_root
            && trimmed
                .split_once('=')
                .is_some_and(|(key, _)| key.trim() == RENDERING_POLICY_KEY);
        if is_managed_comment
            || is_root_deepgram_enabled
            || is_root_delivery_mode
            || is_root_writing_mode
            || is_root_rendering_policy
        {
            continue;
        }
        preserved.push(line);
    }
    let mut out = render(
        deepgram_enabled,
        delivery_mode,
        writing_mode,
        rendering_policy,
    );
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
fn render(
    deepgram_enabled: Option<bool>,
    delivery_mode: Option<DeliveryMode>,
    writing_mode: Option<WritingMode>,
    rendering_policy: Option<RenderingPolicy>,
) -> String {
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
    if let Some(mode) = writing_mode {
        out.push_str(&format!("{WRITING_MODE_KEY} = \"{}\"\n", mode.as_str()));
    }
    if let Some(policy) = rendering_policy {
        out.push_str(&format!(
            "{RENDERING_POLICY_KEY} = \"{}\"\n",
            policy.as_str()
        ));
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
    fn setting_any_of_the_three_root_keys_preserves_the_other_two() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("voisu").join("config.toml");

        write_setting(&path, false).unwrap();
        write_delivery_mode(&path, DeliveryMode::Clipboard).unwrap();
        write_writing_mode(&path, WritingMode::Literal).unwrap();

        // Rewrite each key once more; the other two must survive every write.
        write_setting(&path, true).unwrap();
        assert_eq!(read_setting(&path), Some(true));
        assert_eq!(read_delivery_mode(&path), Some(DeliveryMode::Clipboard));
        assert_eq!(
            read_writing_mode(&path),
            WritingModeLoad::Known(WritingMode::Literal)
        );

        write_delivery_mode(&path, DeliveryMode::Guarded).unwrap();
        assert_eq!(read_setting(&path), Some(true));
        assert_eq!(read_delivery_mode(&path), Some(DeliveryMode::Guarded));
        assert_eq!(
            read_writing_mode(&path),
            WritingModeLoad::Known(WritingMode::Literal)
        );

        write_writing_mode(&path, WritingMode::Smart).unwrap();
        assert_eq!(read_setting(&path), Some(true));
        assert_eq!(read_delivery_mode(&path), Some(DeliveryMode::Guarded));
        assert_eq!(
            read_writing_mode(&path),
            WritingModeLoad::Known(WritingMode::Smart)
        );

        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.matches(DEEPGRAM_ENABLED_KEY).count(), 1, "{contents}");
        assert_eq!(contents.matches(DELIVERY_MODE_KEY).count(), 1, "{contents}");
        assert_eq!(contents.matches(WRITING_MODE_KEY).count(), 1, "{contents}");
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
            parse_deepgram_enabled(&render(Some(true), None, None, None)),
            Some(true)
        );
        assert_eq!(
            parse_deepgram_enabled(&render(Some(false), None, None, None)),
            Some(false)
        );
        assert_eq!(
            parse_writing_mode(&render(None, None, Some(WritingMode::Literal), None)),
            WritingModeLoad::Known(WritingMode::Literal)
        );
        assert_eq!(
            parse_writing_mode(&render(None, None, Some(WritingMode::Smart), None)),
            WritingModeLoad::Known(WritingMode::Smart)
        );
        assert_eq!(
            parse_rendering_policy(&render(
                None,
                None,
                None,
                Some(RenderingPolicy::Structured)
            )),
            RenderingPolicyLoad::Known(RenderingPolicy::Structured)
        );
        assert_eq!(
            parse_rendering_policy(&render(
                None,
                None,
                None,
                Some(RenderingPolicy::Adaptive)
            )),
            RenderingPolicyLoad::Known(RenderingPolicy::Adaptive)
        );
    }

    #[test]
    fn writing_mode_defaults_to_smart_when_nothing_is_persisted() {
        assert_eq!(DEFAULT_WRITING_MODE, WritingMode::Smart);
        assert_eq!(WritingMode::default(), WritingMode::Smart);
        assert_eq!(resolve_writing_mode(WritingModeLoad::Missing), WritingMode::Smart);
        assert_eq!(
            resolve_writing_mode(parse_writing_mode("other_key = true\n")),
            WritingMode::Smart
        );
        assert_eq!(
            read_writing_mode(Path::new("/nonexistent/voisu/config.toml")),
            WritingModeLoad::Missing
        );
    }

    #[test]
    fn writing_modes_parse_and_invalid_values_fail_closed_to_literal() {
        assert_eq!(
            parse_writing_mode("writing_mode = \"smart\"\n"),
            WritingModeLoad::Known(WritingMode::Smart)
        );
        assert_eq!(
            parse_writing_mode("writing_mode = \"literal\"\n"),
            WritingModeLoad::Known(WritingMode::Literal)
        );
        assert_eq!(
            resolve_writing_mode(parse_writing_mode("writing_mode = \"future\"\n")),
            WritingMode::Literal
        );
        assert_eq!(
            resolve_writing_mode(parse_writing_mode("writing_mode = maybe\n")),
            WritingMode::Literal
        );
        assert_eq!(
            resolve_writing_mode(WritingModeLoad::FailClosed),
            WritingMode::Literal
        );
        assert_eq!(
            resolve_writing_mode(WritingModeLoad::Known(WritingMode::Smart)),
            WritingMode::Smart
        );
        assert_eq!(
            resolve_writing_mode(WritingModeLoad::Known(WritingMode::Literal)),
            WritingMode::Literal
        );
    }

    #[test]
    fn single_quoted_writing_modes_are_honored() {
        assert_eq!(
            parse_writing_mode("writing_mode = 'smart'\n"),
            WritingModeLoad::Known(WritingMode::Smart)
        );
        assert_eq!(
            parse_writing_mode("writing_mode = 'literal'\n"),
            WritingModeLoad::Known(WritingMode::Literal)
        );
    }

    #[test]
    fn an_unreadable_config_fails_closed_to_literal_for_writing_mode() {
        // Invalid UTF-8 is not NotFound: the reader emits a diagnostic and
        // fail-closes to Literal rather than inventing the Smart default.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, [0xff, 0xfe, 0x00, 0x42]).unwrap();
        assert_eq!(read_writing_mode(&path), WritingModeLoad::FailClosed);
        assert_eq!(
            resolve_writing_mode(read_writing_mode(&path)),
            WritingMode::Literal
        );
    }

    #[test]
    fn writing_then_reading_writing_mode_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("voisu").join("config.toml");
        write_writing_mode(&path, WritingMode::Literal).unwrap();
        assert_eq!(
            read_writing_mode(&path),
            WritingModeLoad::Known(WritingMode::Literal)
        );
        write_writing_mode(&path, WritingMode::Smart).unwrap();
        assert_eq!(
            read_writing_mode(&path),
            WritingModeLoad::Known(WritingMode::Smart)
        );
    }

    #[test]
    fn a_table_scoped_writing_mode_is_not_read_as_the_root_setting() {
        assert_eq!(
            parse_writing_mode("[other]\nwriting_mode = \"literal\"\n"),
            WritingModeLoad::Missing
        );
        assert_eq!(
            resolve_writing_mode(parse_writing_mode(
                "[other]\nwriting_mode = \"literal\"\n"
            )),
            WritingMode::Smart
        );
    }

    #[test]
    fn a_root_writing_mode_before_a_table_is_honoured() {
        assert_eq!(
            parse_writing_mode("writing_mode = \"literal\"\n[other]\nx = 1\n"),
            WritingModeLoad::Known(WritingMode::Literal)
        );
    }

    #[test]
    fn writing_mode_is_copy_and_stable_as_a_recording_snapshot() {
        // A Recording snapshots Writing Mode before work begins; the value is
        // Copy so later config changes cannot mutate the held snapshot.
        let snapshot = WritingMode::Literal;
        let held_during_recording = snapshot;
        // Simulated mid-recording "config change" cannot rewrite the local.
        let _later_config = WritingMode::Smart;
        assert_eq!(held_during_recording, WritingMode::Literal);
        assert_eq!(snapshot, WritingMode::Literal);
        assert_eq!(held_during_recording, snapshot);
    }

    #[test]
    fn rendering_policy_defaults_to_adaptive_when_nothing_is_persisted() {
        assert_eq!(DEFAULT_RENDERING_POLICY, RenderingPolicy::Adaptive);
        assert_eq!(RenderingPolicy::default(), RenderingPolicy::Adaptive);
        assert_eq!(
            resolve_rendering_policy(RenderingPolicyLoad::Missing),
            RenderingPolicy::Adaptive
        );
        assert_eq!(
            resolve_rendering_policy(parse_rendering_policy("other_key = true\n")),
            RenderingPolicy::Adaptive
        );
        assert_eq!(
            read_rendering_policy(Path::new("/nonexistent/voisu/config.toml")),
            RenderingPolicyLoad::Missing
        );
    }

    #[test]
    fn dpr_rollout_gate_defaults_off_and_requires_an_explicit_true_value() {
        for value in ["", "0", "false", "yes", "adaptive", "garbage"] {
            assert!(!parse_dpr_enablement(value), "unexpected enablement: {value}");
        }
        for value in ["1", "true", " TRUE "] {
            assert!(parse_dpr_enablement(value), "expected enablement: {value}");
        }
    }

    #[test]
    fn qwen_format_gate_defaults_off_and_shares_the_explicit_true_parser() {
        assert!(!DEFAULT_QWEN_FORMAT_ENABLED);
        assert_eq!(ENABLE_QWEN_FORMAT_ENV, "VOISU_ENABLE_QWEN_FORMAT");
        assert!(!parse_optional_dpr_enablement(None));
        for value in ["", "0", "false", "yes", "qwen", "garbage"] {
            assert!(
                !parse_dpr_enablement(value),
                "qwen format must stay off for {value:?}"
            );
        }
        for value in ["1", "true", " TRUE "] {
            assert!(parse_dpr_enablement(value), "expected enablement: {value}");
        }
    }

    #[test]
    fn qwen_format_gate_is_independent_of_the_dpr_pipeline_gate() {
        assert_ne!(ENABLE_DPR_ENV, ENABLE_QWEN_FORMAT_ENV);
        assert_eq!(ENABLE_DPR_ENV, "VOISU_ENABLE_DPR");
        assert_eq!(ENABLE_QWEN_FORMAT_ENV, "VOISU_ENABLE_QWEN_FORMAT");

        let parse_gates = |dpr: Option<&str>, qwen: Option<&str>| {
            (
                parse_optional_dpr_enablement(dpr),
                parse_optional_dpr_enablement(qwen),
            )
        };

        assert_eq!(parse_gates(Some("1"), None), (true, false));
        assert_eq!(parse_gates(None, Some("true")), (false, true));
        assert_eq!(parse_gates(Some("1"), Some("0")), (true, false));
        assert_eq!(parse_gates(None, None), (false, false));
        assert_eq!(parse_gates(Some("1"), Some("true")), (true, true));
    }

    #[test]
    fn qwen_format_env_name_is_the_public_rollback_switch() {
        assert_eq!(ENABLE_QWEN_FORMAT_ENV, "VOISU_ENABLE_QWEN_FORMAT");
        for rollback in ["", "0", "false", "FALSE", "off", "2", "yes"] {
            assert!(
                !parse_dpr_enablement(rollback),
                "rollback value {rollback:?} must disable the formatter"
            );
        }
    }

    #[test]
    fn rendering_policies_parse_and_invalid_values_fail_closed_to_natural() {
        assert_eq!(
            parse_rendering_policy("rendering_policy = \"natural\"\n"),
            RenderingPolicyLoad::Known(RenderingPolicy::Natural)
        );
        assert_eq!(
            parse_rendering_policy("rendering_policy = \"adaptive\"\n"),
            RenderingPolicyLoad::Known(RenderingPolicy::Adaptive)
        );
        assert_eq!(
            parse_rendering_policy("rendering_policy = \"structured\"\n"),
            RenderingPolicyLoad::Known(RenderingPolicy::Structured)
        );
        // Unknown present values emit a bounded diagnostic (stderr) and still
        // resolve to Natural — diagnostic content is not asserted.
        assert_eq!(
            parse_rendering_policy("rendering_policy = \"future\"\n"),
            RenderingPolicyLoad::FailClosed
        );
        assert_eq!(
            resolve_rendering_policy(parse_rendering_policy(
                "rendering_policy = \"future\"\n"
            )),
            RenderingPolicy::Natural
        );
        assert_eq!(
            resolve_rendering_policy(parse_rendering_policy("rendering_policy = maybe\n")),
            RenderingPolicy::Natural
        );
        assert_eq!(
            resolve_rendering_policy(RenderingPolicyLoad::FailClosed),
            RenderingPolicy::Natural
        );
        assert_eq!(
            resolve_rendering_policy(RenderingPolicyLoad::Known(RenderingPolicy::Structured)),
            RenderingPolicy::Structured
        );
    }

    #[test]
    fn fail_closed_rendering_policy_always_resolves_to_natural() {
        // Spec §12 / DAG T0: FailClosed (unreadable or unknown value) → Natural.
        // Bounded diagnostic is emitted on the load path; resolution is what we assert.
        assert_eq!(
            resolve_rendering_policy(RenderingPolicyLoad::FailClosed),
            RenderingPolicy::Natural
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "rendering_policy = \"not-a-policy\"\n").unwrap();
        assert_eq!(
            read_rendering_policy(&path),
            RenderingPolicyLoad::FailClosed
        );
        assert_eq!(
            resolve_rendering_policy(read_rendering_policy(&path)),
            RenderingPolicy::Natural
        );
    }

    #[test]
    fn single_quoted_rendering_policies_are_honored() {
        assert_eq!(
            parse_rendering_policy("rendering_policy = 'natural'\n"),
            RenderingPolicyLoad::Known(RenderingPolicy::Natural)
        );
        assert_eq!(
            parse_rendering_policy("rendering_policy = 'structured'\n"),
            RenderingPolicyLoad::Known(RenderingPolicy::Structured)
        );
    }

    #[test]
    fn an_unreadable_config_fails_closed_to_natural_for_rendering_policy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, [0xff, 0xfe, 0x00, 0x42]).unwrap();
        assert_eq!(
            read_rendering_policy(&path),
            RenderingPolicyLoad::FailClosed
        );
        assert_eq!(
            resolve_rendering_policy(read_rendering_policy(&path)),
            RenderingPolicy::Natural
        );
    }

    #[test]
    fn writing_then_reading_rendering_policy_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("voisu").join("config.toml");
        write_rendering_policy(&path, RenderingPolicy::Natural).unwrap();
        assert_eq!(
            read_rendering_policy(&path),
            RenderingPolicyLoad::Known(RenderingPolicy::Natural)
        );
        write_rendering_policy(&path, RenderingPolicy::Structured).unwrap();
        assert_eq!(
            read_rendering_policy(&path),
            RenderingPolicyLoad::Known(RenderingPolicy::Structured)
        );
        write_rendering_policy(&path, RenderingPolicy::Adaptive).unwrap();
        assert_eq!(
            read_rendering_policy(&path),
            RenderingPolicyLoad::Known(RenderingPolicy::Adaptive)
        );
    }

    #[test]
    fn a_table_scoped_rendering_policy_is_not_read_as_the_root_setting() {
        assert_eq!(
            parse_rendering_policy("[other]\nrendering_policy = \"natural\"\n"),
            RenderingPolicyLoad::Missing
        );
        assert_eq!(
            resolve_rendering_policy(parse_rendering_policy(
                "[other]\nrendering_policy = \"natural\"\n"
            )),
            RenderingPolicy::Adaptive
        );
    }

    #[test]
    fn a_root_rendering_policy_before_a_table_is_honoured() {
        assert_eq!(
            parse_rendering_policy("rendering_policy = \"natural\"\n[other]\nx = 1\n"),
            RenderingPolicyLoad::Known(RenderingPolicy::Natural)
        );
    }

    #[test]
    fn rendering_policy_snapshot_stays_stable_when_config_file_changes() {
        // Real resolution: snapshot Natural from disk, rewrite config to
        // Structured, held snapshot must stay Natural while a fresh resolve
        // returns Structured (daemon wire of the snapshot is T5).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("voisu").join("config.toml");
        write_rendering_policy(&path, RenderingPolicy::Natural).unwrap();

        let snapshot =
            resolve_rendering_policy(read_rendering_policy(&path));
        assert_eq!(snapshot, RenderingPolicy::Natural);

        write_rendering_policy(&path, RenderingPolicy::Structured).unwrap();

        let held_during_recording = snapshot;
        assert_eq!(held_during_recording, RenderingPolicy::Natural);
        assert_eq!(
            resolve_rendering_policy(read_rendering_policy(&path)),
            RenderingPolicy::Structured
        );
        assert_eq!(
            read_rendering_policy(&path),
            RenderingPolicyLoad::Known(RenderingPolicy::Structured)
        );
    }

    #[test]
    fn setting_rendering_policy_preserves_the_other_root_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("voisu").join("config.toml");

        write_setting(&path, false).unwrap();
        write_delivery_mode(&path, DeliveryMode::Clipboard).unwrap();
        write_writing_mode(&path, WritingMode::Literal).unwrap();
        write_rendering_policy(&path, RenderingPolicy::Structured).unwrap();

        write_rendering_policy(&path, RenderingPolicy::Natural).unwrap();
        assert_eq!(read_setting(&path), Some(false));
        assert_eq!(read_delivery_mode(&path), Some(DeliveryMode::Clipboard));
        assert_eq!(
            read_writing_mode(&path),
            WritingModeLoad::Known(WritingMode::Literal)
        );
        assert_eq!(
            read_rendering_policy(&path),
            RenderingPolicyLoad::Known(RenderingPolicy::Natural)
        );

        write_setting(&path, true).unwrap();
        assert_eq!(read_setting(&path), Some(true));
        assert_eq!(
            read_rendering_policy(&path),
            RenderingPolicyLoad::Known(RenderingPolicy::Natural)
        );

        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.matches(DEEPGRAM_ENABLED_KEY).count(), 1, "{contents}");
        assert_eq!(contents.matches(DELIVERY_MODE_KEY).count(), 1, "{contents}");
        assert_eq!(contents.matches(WRITING_MODE_KEY).count(), 1, "{contents}");
        assert_eq!(
            contents.matches(RENDERING_POLICY_KEY).count(),
            1,
            "{contents}"
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
