//! Pure-local weighted intent routing for Developer Prompt Rendering (DPR-T2 / #157).
//!
//! Ports the #141 research prototype (`developer-prompt-rendering-intent-routing-prototype-2026-08-11.py`)
//! without re-deriving weights, thresholds, or rule order. No network, timers, I/O, or randomness
//! on the decision path.

use std::collections::{BTreeMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::prompt_rendering::{
    CloudRequest, RenderingPolicy, RenderingRoute, TimingCertainty,
};

// ---------------------------------------------------------------------------
// Constants — fixed catalogs; must match #141 corpus weights/thresholds
// ---------------------------------------------------------------------------

/// Inclusive complexity score at which Adaptive/Structured may open a cloud attempt.
pub const COMPLEXITY_CLOUD_THRESHOLD: i32 = 24;

/// Word-count ceiling for the messaging short-form local bias.
pub const MESSAGING_SHORT_WORDS: usize = 30;

/// Word-count ceiling for the browser short-form local bias.
pub const BROWSER_SHORT_WORDS: usize = 25;

/// Minimum distinct section cues required before length assists apply.
pub const SECTION_CUES_FOR_LENGTH_ASSIST: usize = 2;

/// Explicit integer weights for complexity signals (prototype `DEFAULT_WEIGHTS`).
pub mod weights {
    pub const SECTION_GOAL: i32 = 12;
    pub const SECTION_CONTEXT: i32 = 12;
    pub const SECTION_REQUIREMENTS: i32 = 12;
    pub const SECTION_CONSTRAINTS: i32 = 12;
    pub const SECTION_STEPS: i32 = 12;
    pub const SECTION_ACCEPTANCE_CRITERIA: i32 = 14;
    pub const SECTION_FILES: i32 = 10;
    pub const SECTION_NOTES: i32 = 10;
    pub const WORDS_GE_40: i32 = 4;
    pub const WORDS_GE_80: i32 = 6;
    pub const SURFACE_CODING_AGENT_SECTIONS: i32 = 4;
    pub const SURFACE_GUI_AGENT_SECTIONS: i32 = 3;
    pub const SURFACE_MESSAGING_SHORT: i32 = -6;
    pub const SURFACE_BROWSER_SHORT: i32 = -4;
    pub const PROCESS_CODING_BOOST: i32 = 2;
    pub const TIMING_CLEAR_PAUSE: i32 = 0;
    pub const TIMING_UNCERTAIN_PAUSE: i32 = 0;
}

fn weight_for_signal(signal: &str) -> i32 {
    match signal {
        "section_goal" => weights::SECTION_GOAL,
        "section_context" => weights::SECTION_CONTEXT,
        "section_requirements" => weights::SECTION_REQUIREMENTS,
        "section_constraints" => weights::SECTION_CONSTRAINTS,
        "section_steps" => weights::SECTION_STEPS,
        "section_acceptance_criteria" => weights::SECTION_ACCEPTANCE_CRITERIA,
        "section_files" => weights::SECTION_FILES,
        "section_notes" => weights::SECTION_NOTES,
        "words_ge_40" => weights::WORDS_GE_40,
        "words_ge_80" => weights::WORDS_GE_80,
        "surface_coding_agent_sections" => weights::SURFACE_CODING_AGENT_SECTIONS,
        "surface_gui_agent_sections" => weights::SURFACE_GUI_AGENT_SECTIONS,
        "surface_messaging_short" => weights::SURFACE_MESSAGING_SHORT,
        "surface_browser_short" => weights::SURFACE_BROWSER_SHORT,
        "process_coding_boost" => weights::PROCESS_CODING_BOOST,
        "timing_clear_pause" => weights::TIMING_CLEAR_PAUSE,
        "timing_uncertain_pause" => weights::TIMING_UNCERTAIN_PAUSE,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Observation / decision types
// ---------------------------------------------------------------------------

/// Dual-STT agreement class (aligned with #138 / #141).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderState {
    ExactAgreement,
    PunctuationOnlyAgreement,
    SafeComplementary,
    ProtectedTokenDisagreement,
    SemanticDisagreement,
    SingleProvider,
}

impl ProviderState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExactAgreement => "exact_agreement",
            Self::PunctuationOnlyAgreement => "punctuation_only_agreement",
            Self::SafeComplementary => "safe_complementary",
            Self::ProtectedTokenDisagreement => "protected_token_disagreement",
            Self::SemanticDisagreement => "semantic_disagreement",
            Self::SingleProvider => "single_provider",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "exact_agreement" => Some(Self::ExactAgreement),
            "punctuation_only_agreement" => Some(Self::PunctuationOnlyAgreement),
            "safe_complementary" => Some(Self::SafeComplementary),
            "protected_token_disagreement" => Some(Self::ProtectedTokenDisagreement),
            "semantic_disagreement" => Some(Self::SemanticDisagreement),
            "single_provider" => Some(Self::SingleProvider),
            _ => None,
        }
    }

    fn is_dispute(self) -> bool {
        matches!(
            self,
            Self::ProtectedTokenDisagreement | Self::SemanticDisagreement
        )
    }
}

/// Optional focus-surface class already known to the daemon.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceHint {
    Shell,
    Terminal,
    CodingAgent,
    GuiAgent,
    Messaging,
    Browser,
    Unknown,
}

impl SurfaceHint {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Terminal => "terminal",
            Self::CodingAgent => "coding_agent",
            Self::GuiAgent => "gui_agent",
            Self::Messaging => "messaging",
            Self::Browser => "browser",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "shell" => Some(Self::Shell),
            "terminal" => Some(Self::Terminal),
            "coding_agent" => Some(Self::CodingAgent),
            "gui_agent" => Some(Self::GuiAgent),
            "messaging" => Some(Self::Messaging),
            "browser" => Some(Self::Browser),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

/// Process-class catalog for optional process hints.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessClass {
    Shell,
    Terminal,
    CodingAgent,
    GuiAgent,
    Messaging,
    Browser,
    Unknown,
}

impl ProcessClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Terminal => "terminal",
            Self::CodingAgent => "coding_agent",
            Self::GuiAgent => "gui_agent",
            Self::Messaging => "messaging",
            Self::Browser => "browser",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "shell" => Some(Self::Shell),
            "terminal" => Some(Self::Terminal),
            "coding_agent" => Some(Self::CodingAgent),
            "gui_agent" => Some(Self::GuiAgent),
            "messaging" => Some(Self::Messaging),
            "browser" => Some(Self::Browser),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

/// Optional focused-process metadata already known to the daemon.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProcessHint {
    pub class: ProcessClass,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Optional pause timing; weight toward cloud is always 0.
///
/// [`TimingCertainty`] is shared from [`crate::prompt_rendering`] so T1 and T2
/// do not define competing crate-root names.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TimingHint {
    pub certainty: TimingCertainty,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_pause_ms: Option<u64>,
}

/// First-match ordered rule id (reproducible diagnostics).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum RuleId {
    #[serde(rename = "R_DISPUTE_CLOUD")]
    DisputeCloud,
    #[serde(rename = "R_DISPUTE_POLICY_FORBID")]
    DisputePolicyForbid,
    #[serde(rename = "R_LITERAL_PREFORMATTED")]
    LiteralPreformatted,
    #[serde(rename = "R_LITERAL_COMMAND")]
    LiteralCommand,
    #[serde(rename = "R_NATURAL_LOCAL")]
    NaturalLocal,
    #[serde(rename = "R_COMPLEX_CLOUD")]
    ComplexCloud,
    #[serde(rename = "R_DEFAULT_LOCAL")]
    DefaultLocal,
}

impl RuleId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DisputeCloud => "R_DISPUTE_CLOUD",
            Self::DisputePolicyForbid => "R_DISPUTE_POLICY_FORBID",
            Self::LiteralPreformatted => "R_LITERAL_PREFORMATTED",
            Self::LiteralCommand => "R_LITERAL_COMMAND",
            Self::NaturalLocal => "R_NATURAL_LOCAL",
            Self::ComplexCloud => "R_COMPLEX_CLOUD",
            Self::DefaultLocal => "R_DEFAULT_LOCAL",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "R_DISPUTE_CLOUD" => Some(Self::DisputeCloud),
            "R_DISPUTE_POLICY_FORBID" => Some(Self::DisputePolicyForbid),
            "R_LITERAL_PREFORMATTED" => Some(Self::LiteralPreformatted),
            "R_LITERAL_COMMAND" => Some(Self::LiteralCommand),
            "R_NATURAL_LOCAL" => Some(Self::NaturalLocal),
            "R_COMPLEX_CLOUD" => Some(Self::ComplexCloud),
            "R_DEFAULT_LOCAL" => Some(Self::DefaultLocal),
            _ => None,
        }
    }
}

/// One diagnostic contribution that built the complexity score.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ScoreContribution {
    pub signal: String,
    pub weight: i32,
    pub detail: String,
}

/// Pure-local routing inputs. Missing optional fields degrade to speech-only.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IntentObservation {
    pub policy: RenderingPolicy,
    pub primary_text: String,
    pub provider_state: ProviderState,
    pub surface_hint: Option<SurfaceHint>,
    pub process_hint: Option<ProcessHint>,
    pub timing: Option<TimingHint>,
}

/// Pure-local routing decision. No secrets; safe for diagnostics.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RoutingDecision {
    pub route: RenderingRoute,
    pub cloud_request: CloudRequest,
    pub rule_id: RuleId,
    pub complexity_score: i32,
    pub contributions: Vec<ScoreContribution>,
    pub surface_degraded: bool,
    pub section_cue_count: usize,
}

// ---------------------------------------------------------------------------
// Token / shape catalogs
// ---------------------------------------------------------------------------

static WORD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z0-9_./:-]+").expect("WORD_RE"));
static FLAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^--?[A-Za-z0-9][\w-]*$").expect("FLAG_RE"));
static DOUBLE_DASH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^--[\w-]+(?:=.*)?$").expect("DOUBLE_DASH_RE"));
static NUMBERED_LINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*\d+[\.)]\s+\S").expect("NUMBERED_LINE_RE"));
static BULLET_LINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*[-*]\s+\S").expect("BULLET_LINE_RE"));
static FILE_EXT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^[A-Za-z0-9_.-]+\.(?:rs|py|ts|tsx|js|jsx|sh|go|toml|json|ya?ml|md|txt|lock|so|a|o)$",
    )
    .expect("FILE_EXT_RE")
});
/// Bazel-style `//target` tokens (absolute `//` paths with path-ish body).
static BAZEL_TARGET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^//[A-Za-z0-9_./:@+-]+$").expect("BAZEL_TARGET_RE")
});

fn section_header_re(phrase: &str) -> Regex {
    let esc = regex::escape(phrase).replace(' ', r"\s+");
    let pattern = format!(
        r"(?i)(?:(?:^|[\n.!?]\s*){esc}\b|(?:^|[\n.!?]\s*|,\s*){esc}\s*:|(?:^|[\n.!?]\s*)(?:the\s+)?{esc}\s+is\b)"
    );
    Regex::new(&pattern).unwrap_or_else(|e| panic!("section header re for {phrase}: {e}"))
}

struct SectionCue {
    signal_id: &'static str,
    phrase: &'static str,
    pattern: Regex,
    strength: Strength,
    tokens: &'static [&'static str],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Strength {
    Strong,
    Weak,
}

static SECTION_CUES: LazyLock<Vec<SectionCue>> = LazyLock::new(|| {
    vec![
        SectionCue {
            signal_id: "section_acceptance_criteria",
            phrase: "acceptance criteria",
            pattern: section_header_re("acceptance criteria"),
            strength: Strength::Strong,
            tokens: &["acceptance", "criteria"],
        },
        SectionCue {
            signal_id: "section_goal",
            phrase: "goal",
            pattern: section_header_re("goal"),
            strength: Strength::Strong,
            tokens: &["goal"],
        },
        SectionCue {
            signal_id: "section_context",
            phrase: "context",
            pattern: section_header_re("context"),
            strength: Strength::Strong,
            tokens: &["context"],
        },
        SectionCue {
            signal_id: "section_requirements",
            phrase: "requirements",
            pattern: section_header_re("requirements"),
            strength: Strength::Strong,
            tokens: &["requirements"],
        },
        SectionCue {
            signal_id: "section_constraints",
            phrase: "constraints",
            pattern: section_header_re("constraints"),
            strength: Strength::Strong,
            tokens: &["constraints"],
        },
        SectionCue {
            signal_id: "section_steps",
            phrase: "steps",
            pattern: section_header_re("steps"),
            strength: Strength::Weak,
            tokens: &["steps"],
        },
        SectionCue {
            signal_id: "section_files",
            phrase: "files",
            pattern: section_header_re("files"),
            strength: Strength::Weak,
            tokens: &["files"],
        },
        SectionCue {
            signal_id: "section_notes",
            phrase: "notes",
            pattern: section_header_re("notes"),
            strength: Strength::Weak,
            tokens: &["notes"],
        },
    ]
});

static STRONG_SECTION_LEAD_TOKENS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        "goal",
        "context",
        "requirements",
        "constraints",
        "acceptance",
    ])
});

static DETERMINER_TOKENS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        "the", "a", "an", "my", "your", "our", "some", "any", "these", "those", "this", "that",
    ])
});

static COMPOUND_LEFT_TOKENS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        "release",
        "next",
        "open",
        "send",
        "share",
        "project",
        "business",
        "user",
        "team",
        "main",
        "overall",
        "primary",
        "broader",
        "historical",
        "social",
        "local",
        "global",
        "market",
        "product",
    ])
});

static RUNNER_TOKENS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        "run", "cargo", "npm", "pnpm", "yarn", "git", "docker", "kubectl", "make", "python",
        "python3", "pip", "curl", "ssh", "scp", "go", "bazel", "ninja",
    ])
});

/// Everyday English second tokens after a leading runner — not CLI.
/// Covers "make sure…", "go ahead…", "run this/by…" and similar prose (R2).
static PROSE_RUNNER_SECONDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        "sure", "certain", "sense", "clear", "it", "this", "that", "me", "us", "him", "her",
        "them", "my", "your", "our", "a", "an", "the", "ahead", "for", "back", "on", "through",
        "away", "home", "there", "here", "to", "and", "with", "get", "by", "into", "out", "over",
        "some", "any", "when", "if", "while", "before", "after",
    ])
});

static KNOWN_CLI_SUBCOMMANDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        "test", "build", "run", "check", "clippy", "fmt", "bench", "doc", "install", "clean",
        "status", "commit", "push", "pull", "clone", "diff", "log", "add", "checkout", "branch",
        "merge", "rebase", "fetch", "exec", "ps", "images", "compose", "apply", "get", "describe",
        "logs", "delete", "create", "scale", "rollout", "config", "init", "start", "stop",
        "restart", "up", "down", "serve", "dev", "publish", "pack", "login", "logout", "whoami",
        "version", "help", "mod", "env", "list", "info", "search", "uninstall", "update",
        "upgrade", "remove", "sync", "lock", "audit", "outdated", "workspace", "package",
        "target", "release", "debug", "all", "dist", "deploy", "vet", "generate", "tool", "work",
        "tidy", "vendor", "query", "coverage", "nextest",
    ])
});

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn tokenize(text: &str) -> Vec<String> {
    WORD_RE
        .find_iter(text)
        .map(|m| m.as_str().to_string())
        .collect()
}

fn word_count(text: &str) -> usize {
    tokenize(text).len()
}

fn is_preformatted(text: &str) -> bool {
    if !text.contains('\n') {
        return false;
    }
    let mut numbered = 0usize;
    let mut bullets = 0usize;
    for line in text.lines() {
        if NUMBERED_LINE_RE.is_match(line) {
            numbered += 1;
        }
        if BULLET_LINE_RE.is_match(line) {
            bullets += 1;
        }
    }
    numbered >= 2 || bullets >= 2
}

/// True for absolute/relative/home/bazel paths or file-with-extension tokens.
///
/// Ports prototype `PATH_LIKE_RE` without lookaround (default `regex` crate
/// does not support `(?!…)`).
fn is_path_like(token: &str) -> bool {
    if FILE_EXT_RE.is_match(token) {
        return true;
    }
    // `~` or `~/…`
    if token == "~" || token.starts_with("~/") {
        return true;
    }
    // Bazel `//target…`
    if BAZEL_TARGET_RE.is_match(token) {
        return true;
    }
    // Absolute `/…` but not `//…` (handled above)
    if token.starts_with('/') && !token.starts_with("//") {
        return true;
    }
    // Relative `./…` or `../…`
    if token.starts_with("./") || token.starts_with("../") {
        return true;
    }
    // Slash-containing path tokens (`foo/bar`, `crates/voisu-core/src/auth.rs`)
    if let Some(idx) = token.find('/') {
        if idx > 0 && idx + 1 < token.len() {
            return true;
        }
    }
    false
}

fn has_cli_flag(tokens: &[String]) -> bool {
    tokens
        .iter()
        .any(|t| FLAG_RE.is_match(t) || DOUBLE_DASH_RE.is_match(t))
}

fn runner_follow_is_cli(follow: &str, follow_raw: &str) -> bool {
    if PROSE_RUNNER_SECONDS.contains(follow) {
        return false;
    }
    if FLAG_RE.is_match(follow_raw) || DOUBLE_DASH_RE.is_match(follow_raw) {
        return true;
    }
    if is_path_like(follow_raw) {
        return true;
    }
    if KNOWN_CLI_SUBCOMMANDS.contains(follow) {
        return true;
    }
    if RUNNER_TOKENS.contains(follow) {
        return true;
    }
    false
}

/// True when speech looks like a real CLI invocation, not everyday English.
///
/// Runner tokens only count with **positive** CLI evidence. Everyday seconds
/// after runners (`make sure…`, `go ahead…`, `run this/by…`, `make dinner…`)
/// are rejected (residual R2).
pub fn is_command_shaped(text: &str) -> bool {
    let tokens = tokenize(text);
    if tokens.is_empty() {
        return false;
    }
    let lower: Vec<String> = tokens.iter().map(|t| t.to_ascii_lowercase()).collect();

    if has_cli_flag(&tokens) {
        return true;
    }

    let has_path = tokens.iter().any(|t| is_path_like(t));

    // Arm A: leading runner with positive CLI continuation (or bare runner).
    if RUNNER_TOKENS.contains(lower[0].as_str()) {
        if lower.len() == 1 {
            return true;
        }
        if PROSE_RUNNER_SECONDS.contains(lower[1].as_str()) && !has_path {
            return false;
        }
        if runner_follow_is_cli(&lower[1], &tokens[1]) {
            return true;
        }
        if has_path {
            return true;
        }
        return false;
    }

    // Arm B: runner in first three tokens with path / subcommand / nested-CLI shape.
    for (i, tok) in lower.iter().take(3).enumerate() {
        if !RUNNER_TOKENS.contains(tok.as_str()) {
            continue;
        }
        if has_path {
            return true;
        }
        if i + 1 < lower.len() && runner_follow_is_cli(&lower[i + 1], &tokens[i + 1]) {
            return true;
        }
    }
    false
}

fn has_strong_cli_evidence(text: &str) -> bool {
    tokenize(text).iter().any(|t| DOUBLE_DASH_RE.is_match(t))
}

fn command_anchor_ok(
    surface_hint: Option<SurfaceHint>,
    process_hint: Option<&ProcessHint>,
    text: &str,
) -> bool {
    if matches!(
        surface_hint,
        Some(SurfaceHint::Shell | SurfaceHint::Terminal)
    ) {
        return true;
    }
    if let Some(p) = process_hint {
        if matches!(p.class, ProcessClass::Shell | ProcessClass::Terminal) {
            return true;
        }
    }
    has_strong_cli_evidence(text)
}

/// (signal_id, phrase, strength, detail_kind) for each distinct cue hit.
///
/// Uses [`BTreeMap`] keyed by `signal_id` so multi-cue contribution order is
/// deterministic across runs (HashMap iteration order is not stable).
fn collect_section_cue_hits(primary_text: &str) -> Vec<(String, String, Strength, &'static str)> {
    // signal_id -> (phrase, strength, detail_kind); BTreeMap keeps signal_id order.
    let mut found: BTreeMap<String, (String, Strength, &'static str)> = BTreeMap::new();

    for cue in SECTION_CUES.iter() {
        if cue.pattern.is_match(primary_text) {
            found.insert(
                cue.signal_id.to_string(),
                (cue.phrase.to_string(), cue.strength, "header"),
            );
        }
    }

    let tokens = tokenize(primary_text);
    let lower: Vec<String> = tokens.iter().map(|t| t.to_ascii_lowercase()).collect();
    let stream_mode = !lower.is_empty() && STRONG_SECTION_LEAD_TOKENS.contains(lower[0].as_str());
    if stream_mode {
        let mut i = 0usize;
        while i < lower.len() {
            let mut matched = false;
            for cue in SECTION_CUES.iter() {
                let n = cue.tokens.len();
                if i + n > lower.len() {
                    continue;
                }
                let window: Vec<&str> = lower[i..i + n].iter().map(|s| s.as_str()).collect();
                if window.as_slice() != cue.tokens {
                    continue;
                }
                // Mid-stream introducers must introduce following content.
                // Token-0 (or multi-word starting at 0) may stand alone.
                if i > 0 && i + n >= lower.len() {
                    matched = true;
                    i += n;
                    break;
                }
                let prev = if i > 0 { lower[i - 1].as_str() } else { "" };
                if DETERMINER_TOKENS.contains(prev) || COMPOUND_LEFT_TOKENS.contains(prev) {
                    matched = true;
                    i += n;
                    break;
                }
                found.entry(cue.signal_id.to_string()).or_insert_with(|| {
                    (
                        cue.tokens.join(" "),
                        cue.strength,
                        "stream",
                    )
                });
                matched = true;
                i += n;
                break;
            }
            if !matched {
                i += 1;
            }
        }
    }

    found
        .into_iter()
        .map(|(signal_id, (phrase, strength, kind))| (signal_id, phrase, strength, kind))
        .collect()
}

fn score_complexity(
    primary_text: &str,
    surface_hint: Option<SurfaceHint>,
    process_hint: Option<&ProcessHint>,
    timing: Option<&TimingHint>,
) -> (i32, Vec<ScoreContribution>, usize) {
    let mut contributions: Vec<ScoreContribution> = Vec::new();
    let mut score: i32 = 0;
    let mut section_hits: usize = 0;

    let hits = collect_section_cue_hits(primary_text);
    let strong_hits: Vec<_> = hits
        .iter()
        .filter(|(_, _, s, _)| *s == Strength::Strong)
        .collect();
    let weak_hits: Vec<_> = hits
        .iter()
        .filter(|(_, _, s, _)| *s == Strength::Weak)
        .collect();

    for (signal_id, phrase, _, kind) in &strong_hits {
        let weight = weight_for_signal(signal_id);
        score += weight;
        section_hits += 1;
        contributions.push(ScoreContribution {
            signal: signal_id.clone(),
            weight,
            detail: format!("matched strong section cue '{phrase}' ({kind})"),
        });
    }

    if !strong_hits.is_empty() {
        for (signal_id, phrase, _, kind) in &weak_hits {
            let weight = weight_for_signal(signal_id);
            score += weight;
            section_hits += 1;
            contributions.push(ScoreContribution {
                signal: signal_id.clone(),
                weight,
                detail: format!(
                    "matched weak section cue '{phrase}' with strong multi-section evidence ({kind})"
                ),
            });
        }
    }

    let wc = word_count(primary_text);
    if section_hits >= SECTION_CUES_FOR_LENGTH_ASSIST {
        if wc >= 80 {
            let weight = weights::WORDS_GE_80;
            score += weight;
            contributions.push(ScoreContribution {
                signal: "words_ge_80".to_string(),
                weight,
                detail: format!("word_count={wc}"),
            });
        }
        if wc >= 40 {
            let weight = weights::WORDS_GE_40;
            score += weight;
            contributions.push(ScoreContribution {
                signal: "words_ge_40".to_string(),
                weight,
                detail: format!("word_count={wc}"),
            });
        }
    }

    if surface_hint == Some(SurfaceHint::CodingAgent) && section_hits >= 1 {
        let weight = weights::SURFACE_CODING_AGENT_SECTIONS;
        score += weight;
        contributions.push(ScoreContribution {
            signal: "surface_coding_agent_sections".to_string(),
            weight,
            detail: "coding_agent with section cues".to_string(),
        });
    }
    if surface_hint == Some(SurfaceHint::GuiAgent) && section_hits >= 1 {
        let weight = weights::SURFACE_GUI_AGENT_SECTIONS;
        score += weight;
        contributions.push(ScoreContribution {
            signal: "surface_gui_agent_sections".to_string(),
            weight,
            detail: "gui_agent with section cues".to_string(),
        });
    }
    if surface_hint == Some(SurfaceHint::Messaging)
        && wc < MESSAGING_SHORT_WORDS
        && section_hits == 0
    {
        let weight = weights::SURFACE_MESSAGING_SHORT;
        score += weight;
        contributions.push(ScoreContribution {
            signal: "surface_messaging_short".to_string(),
            weight,
            detail: format!("messaging short word_count={wc}"),
        });
    }
    if surface_hint == Some(SurfaceHint::Browser) && wc < BROWSER_SHORT_WORDS && section_hits == 0
    {
        let weight = weights::SURFACE_BROWSER_SHORT;
        score += weight;
        contributions.push(ScoreContribution {
            signal: "surface_browser_short".to_string(),
            weight,
            detail: format!("browser short word_count={wc}"),
        });
    }

    if let Some(p) = process_hint {
        if matches!(p.class, ProcessClass::CodingAgent | ProcessClass::GuiAgent)
            && section_hits >= 1
        {
            let weight = weights::PROCESS_CODING_BOOST;
            score += weight;
            contributions.push(ScoreContribution {
                signal: "process_coding_boost".to_string(),
                weight,
                detail: format!("process.class={}", p.class.as_str()),
            });
        }
    }

    if let Some(t) = timing {
        match t.certainty {
            TimingCertainty::Clear => {
                let weight = weights::TIMING_CLEAR_PAUSE;
                contributions.push(ScoreContribution {
                    signal: "timing_clear_pause".to_string(),
                    weight,
                    detail: format!(
                        "max_pause_ms={}",
                        t.max_pause_ms
                            .map(|ms| ms.to_string())
                            .unwrap_or_else(|| "None".to_string())
                    ),
                });
                score += weight;
            }
            TimingCertainty::Uncertain => {
                let weight = weights::TIMING_UNCERTAIN_PAUSE;
                contributions.push(ScoreContribution {
                    signal: "timing_uncertain_pause".to_string(),
                    weight,
                    detail: format!(
                        "max_pause_ms={}",
                        t.max_pause_ms
                            .map(|ms| ms.to_string())
                            .unwrap_or_else(|| "None".to_string())
                    ),
                });
                score += weight;
            }
        }
    }

    if score < 0 {
        contributions.push(ScoreContribution {
            signal: "score_floor".to_string(),
            weight: -score,
            detail: "clamped complexity score to 0".to_string(),
        });
        score = 0;
    }

    (score, contributions, section_hits)
}

fn decision(
    route: RenderingRoute,
    cloud_request: CloudRequest,
    rule_id: RuleId,
    score: i32,
    contributions: Vec<ScoreContribution>,
    surface_degraded: bool,
    section_hits: usize,
) -> RoutingDecision {
    RoutingDecision {
        route,
        cloud_request,
        rule_id,
        complexity_score: score,
        contributions,
        surface_degraded,
        section_cue_count: section_hits,
    }
}

// ---------------------------------------------------------------------------
// Public pure router
// ---------------------------------------------------------------------------

/// Pure-local routing decision. No network, no sleep, no randomness, no I/O.
///
/// Ordered first-match rules (prototype):
/// 1. Dispute + non-Natural → optional cloud
/// 2. Dispute + Natural → local, cloud forbidden
/// 3. Preformatted multi-line list → literal identity
/// 4. Command-shaped + anchor → literal identity
/// 5. Natural policy → local
/// 6. Complexity ≥ threshold → policy cloud table
/// 7. Default → local
pub fn route_intent(observation: &IntentObservation) -> RoutingDecision {
    let surface_degraded =
        observation.surface_hint.is_none() && observation.process_hint.is_none();

    let (score, contributions, section_hits) = score_complexity(
        &observation.primary_text,
        observation.surface_hint,
        observation.process_hint.as_ref(),
        observation.timing.as_ref(),
    );

    // Dispute cloud eligibility before literal preformatted/command so Adaptive/
    // Structured protected-token / semantic disagreement on a list or CLI still
    // opens cloud. Natural still forbids cloud.
    if observation.provider_state.is_dispute() {
        if observation.policy == RenderingPolicy::Natural {
            return decision(
                RenderingRoute::DeterministicLocal,
                CloudRequest::NotAllowed,
                RuleId::DisputePolicyForbid,
                score,
                contributions,
                surface_degraded,
                section_hits,
            );
        }
        return decision(
            RenderingRoute::LocalWithOptionalCloud,
            CloudRequest::Allowed,
            RuleId::DisputeCloud,
            score,
            contributions,
            surface_degraded,
            section_hits,
        );
    }

    if is_preformatted(&observation.primary_text) {
        return decision(
            RenderingRoute::LiteralIdentity,
            CloudRequest::NotAllowed,
            RuleId::LiteralPreformatted,
            score,
            contributions,
            surface_degraded,
            section_hits,
        );
    }

    if is_command_shaped(&observation.primary_text)
        && command_anchor_ok(
            observation.surface_hint,
            observation.process_hint.as_ref(),
            &observation.primary_text,
        )
    {
        return decision(
            RenderingRoute::LiteralIdentity,
            CloudRequest::NotAllowed,
            RuleId::LiteralCommand,
            score,
            contributions,
            surface_degraded,
            section_hits,
        );
    }

    if observation.policy == RenderingPolicy::Natural {
        return decision(
            RenderingRoute::DeterministicLocal,
            CloudRequest::NotAllowed,
            RuleId::NaturalLocal,
            score,
            contributions,
            surface_degraded,
            section_hits,
        );
    }

    if score >= COMPLEXITY_CLOUD_THRESHOLD {
        let cloud = if observation.policy == RenderingPolicy::Structured {
            CloudRequest::Required
        } else {
            CloudRequest::Allowed
        };
        return decision(
            RenderingRoute::LocalWithOptionalCloud,
            cloud,
            RuleId::ComplexCloud,
            score,
            contributions,
            surface_degraded,
            section_hits,
        );
    }

    decision(
        RenderingRoute::DeterministicLocal,
        CloudRequest::NotAllowed,
        RuleId::DefaultLocal,
        score,
        contributions,
        surface_degraded,
        section_hits,
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const CORPUS_JSON: &str = include_str!(
        "../../../docs/research/developer-prompt-rendering-intent-routing-corpus-2026-08-11.json"
    );

    fn parse_policy(v: &str) -> RenderingPolicy {
        RenderingPolicy::parse(v).unwrap_or_else(|| panic!("bad policy {v}"))
    }

    fn parse_provider_state(v: &str) -> ProviderState {
        ProviderState::parse(v).unwrap_or_else(|| panic!("bad provider_state {v}"))
    }

    fn observation_from_fixture(fx: &Value) -> IntentObservation {
        let surface_hint = match fx.get("surface_hint") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(
                SurfaceHint::parse(s).unwrap_or_else(|| panic!("bad surface_hint {s}")),
            ),
            other => panic!("bad surface_hint {other:?}"),
        };

        let process_hint = match fx.get("process_hint") {
            None | Some(Value::Null) => None,
            Some(obj) if obj.is_object() => {
                let class = obj
                    .get("class")
                    .and_then(|c| c.as_str())
                    .and_then(ProcessClass::parse)
                    .expect("process_hint.class");
                let name = obj
                    .get("name")
                    .and_then(|n| n.as_str())
                    .map(|s| s.to_string());
                Some(ProcessHint { class, name })
            }
            other => panic!("bad process_hint {other:?}"),
        };

        let timing = match fx.get("timing") {
            None | Some(Value::Null) => None,
            Some(obj) if obj.is_object() => {
                let certainty = obj
                    .get("certainty")
                    .and_then(|c| c.as_str())
                    .and_then(TimingCertainty::parse)
                    .expect("timing.certainty");
                let max_pause_ms = obj.get("max_pause_ms").and_then(|m| m.as_u64());
                Some(TimingHint {
                    certainty,
                    max_pause_ms,
                })
            }
            other => panic!("bad timing {other:?}"),
        };

        IntentObservation {
            policy: parse_policy(fx["policy"].as_str().expect("policy")),
            primary_text: fx["primary_text"]
                .as_str()
                .expect("primary_text")
                .to_string(),
            provider_state: parse_provider_state(
                fx["provider_state"].as_str().expect("provider_state"),
            ),
            surface_hint,
            process_hint,
            timing,
        }
    }

    fn load_corpus() -> Value {
        serde_json::from_str(CORPUS_JSON).expect("corpus JSON")
    }

    #[test]
    fn corpus_constants_match_product() {
        let corpus = load_corpus();
        assert_eq!(
            corpus["thresholds"]["complexity_cloud"].as_i64().unwrap(),
            i64::from(COMPLEXITY_CLOUD_THRESHOLD)
        );
        assert_eq!(
            corpus["weights"]["section_goal"].as_i64().unwrap(),
            i64::from(weights::SECTION_GOAL)
        );
        assert_eq!(
            corpus["weights"]["section_acceptance_criteria"]
                .as_i64()
                .unwrap(),
            i64::from(weights::SECTION_ACCEPTANCE_CRITERIA)
        );
        assert_eq!(
            corpus["weights"]["surface_messaging_short"]
                .as_i64()
                .unwrap(),
            i64::from(weights::SURFACE_MESSAGING_SHORT)
        );
    }

    #[test]
    fn promotes_all_iri_corpus_fixtures() {
        let corpus = load_corpus();
        let fixtures = corpus["fixtures"].as_array().expect("fixtures");
        assert_eq!(fixtures.len(), 40, "expected full #141 corpus (40 fixtures)");

        let mut seen_rules: HashSet<&'static str> = HashSet::new();

        for fx in fixtures {
            let id = fx["id"].as_str().unwrap();
            let obs = observation_from_fixture(fx);
            let decision = route_intent(&obs);
            let exp = &fx["expected"];

            assert_eq!(
                decision.route.as_str(),
                exp["route"].as_str().unwrap(),
                "{id}: route"
            );
            assert_eq!(
                decision.cloud_request.as_str(),
                exp["cloud_request"].as_str().unwrap(),
                "{id}: cloud_request"
            );
            assert_eq!(
                decision.rule_id.as_str(),
                exp["rule_id"].as_str().unwrap(),
                "{id}: rule_id"
            );

            let min = exp["min_complexity_score"].as_i64().unwrap() as i32;
            let max = exp["max_complexity_score"].as_i64().unwrap() as i32;
            assert!(
                decision.complexity_score >= min && decision.complexity_score <= max,
                "{id}: complexity_score {} not in [{min}, {max}]",
                decision.complexity_score
            );

            seen_rules.insert(decision.rule_id.as_str());

            // Natural never allows cloud.
            if obs.policy == RenderingPolicy::Natural {
                assert_eq!(
                    decision.cloud_request,
                    CloudRequest::NotAllowed,
                    "{id}: Natural must not allow cloud"
                );
                assert_ne!(
                    decision.route,
                    RenderingRoute::LocalWithOptionalCloud,
                    "{id}: Natural must not open optional cloud route"
                );
            }

            // literal_identity always pairs with not_allowed.
            if decision.route == RenderingRoute::LiteralIdentity {
                assert_eq!(decision.cloud_request, CloudRequest::NotAllowed);
            }
            if decision.route == RenderingRoute::DeterministicLocal {
                assert_eq!(decision.cloud_request, CloudRequest::NotAllowed);
            }
        }

        for required in [
            "R_DISPUTE_CLOUD",
            "R_DISPUTE_POLICY_FORBID",
            "R_LITERAL_PREFORMATTED",
            "R_LITERAL_COMMAND",
            "R_NATURAL_LOCAL",
            "R_COMPLEX_CLOUD",
            "R_DEFAULT_LOCAL",
        ] {
            assert!(
                seen_rules.contains(required),
                "missing rule_id coverage for {required}; seen={seen_rules:?}"
            );
        }
    }

    /// Residual R2: shell/terminal prose must not take literal_identity unless
    /// command-shaped. Adversarial cases from the #141 package + DAG acceptance.
    #[test]
    fn r2_shell_prose_stays_deterministic_local_not_literal() {
        let cases = [
            "make sure the service restarts after deploy",
            "go ahead and restart when you can",
            "run this by the team tomorrow",
            "make dinner later",
            "please restart the service when you can",
            "run errands tomorrow",
            "go shopping",
            "python is great",
        ];

        for text in cases {
            for surface in [Some(SurfaceHint::Shell), Some(SurfaceHint::Terminal)] {
                let obs = IntentObservation {
                    policy: RenderingPolicy::Adaptive,
                    primary_text: text.to_string(),
                    provider_state: ProviderState::SingleProvider,
                    surface_hint: surface,
                    process_hint: None,
                    timing: None,
                };
                let d = route_intent(&obs);
                assert_eq!(
                    d.route,
                    RenderingRoute::DeterministicLocal,
                    "R2 prose must not be literal: {text:?} surface={surface:?}"
                );
                assert_eq!(d.cloud_request, CloudRequest::NotAllowed);
                assert_eq!(d.rule_id, RuleId::DefaultLocal);
                assert!(
                    !is_command_shaped(text),
                    "R2 prose must not be command-shaped: {text:?}"
                );
            }
        }
    }

    #[test]
    fn true_cli_commands_still_literal_with_shell_anchor() {
        let cases = [
            "cargo test --package voisu-core",
            "make install",
            "go test ./...",
            "run cargo test --workspace -- --test-threads=4",
            "git status --short",
            "npm test -- --runInBand",
        ];
        for text in cases {
            let obs = IntentObservation {
                policy: RenderingPolicy::Adaptive,
                primary_text: text.to_string(),
                provider_state: ProviderState::SingleProvider,
                surface_hint: Some(SurfaceHint::Shell),
                process_hint: None,
                timing: None,
            };
            let d = route_intent(&obs);
            assert_eq!(
                d.route,
                RenderingRoute::LiteralIdentity,
                "CLI must stay literal: {text:?}"
            );
            assert_eq!(d.cloud_request, CloudRequest::NotAllowed);
            assert_eq!(d.rule_id, RuleId::LiteralCommand);
        }
    }

    #[test]
    fn natural_plus_protected_disagreement_is_local_no_cloud() {
        // N3: Natural forbids cloud even on dual-STT protected disagreement.
        let obs = IntentObservation {
            policy: RenderingPolicy::Natural,
            primary_text: "ship crate voisu-core version 0.19.1".to_string(),
            provider_state: ProviderState::ProtectedTokenDisagreement,
            surface_hint: None,
            process_hint: None,
            timing: None,
        };
        let d = route_intent(&obs);
        assert_eq!(d.route, RenderingRoute::DeterministicLocal);
        assert_eq!(d.cloud_request, CloudRequest::NotAllowed);
        assert_eq!(d.rule_id, RuleId::DisputePolicyForbid);
    }

    #[test]
    fn structured_complex_requires_cloud_attempt() {
        let obs = IntentObservation {
            policy: RenderingPolicy::Structured,
            primary_text: "goal ship the feature context CI is red requirements keep API stable"
                .to_string(),
            provider_state: ProviderState::SingleProvider,
            surface_hint: Some(SurfaceHint::CodingAgent),
            process_hint: None,
            timing: None,
        };
        let d = route_intent(&obs);
        assert_eq!(d.route, RenderingRoute::LocalWithOptionalCloud);
        assert_eq!(d.cloud_request, CloudRequest::Required);
        assert_eq!(d.rule_id, RuleId::ComplexCloud);
        assert!(d.complexity_score >= COMPLEXITY_CLOUD_THRESHOLD);
    }

    #[test]
    fn speech_only_path_works_without_surface_or_process() {
        let obs = IntentObservation {
            policy: RenderingPolicy::Adaptive,
            primary_text: "hey can you send the notes to the team".to_string(),
            provider_state: ProviderState::SingleProvider,
            surface_hint: None,
            process_hint: None,
            timing: None,
        };
        let d = route_intent(&obs);
        assert!(d.surface_degraded);
        assert_eq!(d.route, RenderingRoute::DeterministicLocal);
        assert_eq!(d.cloud_request, CloudRequest::NotAllowed);
    }

    #[test]
    fn ordinary_compound_prose_does_not_score_section_cues() {
        let obs = IntentObservation {
            policy: RenderingPolicy::Adaptive,
            primary_text:
                "the project goal depends on business context and the broader requirements"
                    .to_string(),
            provider_state: ProviderState::SingleProvider,
            surface_hint: None,
            process_hint: None,
            timing: None,
        };
        let d = route_intent(&obs);
        assert_eq!(d.route, RenderingRoute::DeterministicLocal);
        assert_eq!(d.complexity_score, 0);
        assert_eq!(d.section_cue_count, 0);
    }

    #[test]
    fn multi_cue_contribution_signal_order_is_deterministic() {
        // Header-shaped multi-section speech: several distinct section cues fire.
        let text = "Goal: ship the fix. Context: CI is red. Requirements: keep the API stable. Constraints: no new deps.";
        let obs = IntentObservation {
            policy: RenderingPolicy::Adaptive,
            primary_text: text.to_string(),
            provider_state: ProviderState::SingleProvider,
            surface_hint: None,
            process_hint: None,
            timing: None,
        };
        let a = route_intent(&obs);
        let b = route_intent(&obs);
        assert!(
            a.section_cue_count >= 2,
            "fixture must hit multi-cue path, got {}",
            a.section_cue_count
        );
        let signals_a: Vec<&str> = a.contributions.iter().map(|c| c.signal.as_str()).collect();
        let signals_b: Vec<&str> = b.contributions.iter().map(|c| c.signal.as_str()).collect();
        assert_eq!(
            signals_a, signals_b,
            "contribution signal order must be stable across runs"
        );
        // Section cue signals themselves are ordered by signal_id (BTreeMap).
        let section_signals: Vec<&str> = signals_a
            .iter()
            .copied()
            .filter(|s| s.starts_with("section_"))
            .collect();
        let mut sorted = section_signals.clone();
        sorted.sort_unstable();
        assert_eq!(
            section_signals, sorted,
            "section cue contributions must follow signal_id order"
        );
    }

    #[test]
    fn timing_alone_never_opens_cloud() {
        let obs = IntentObservation {
            policy: RenderingPolicy::Adaptive,
            primary_text: "send the update when you can".to_string(),
            provider_state: ProviderState::SingleProvider,
            surface_hint: None,
            process_hint: None,
            timing: Some(TimingHint {
                certainty: TimingCertainty::Clear,
                max_pause_ms: Some(900),
            }),
        };
        let d = route_intent(&obs);
        assert_eq!(d.route, RenderingRoute::DeterministicLocal);
        assert_eq!(d.cloud_request, CloudRequest::NotAllowed);
        assert_eq!(d.complexity_score, 0);
    }

    #[test]
    fn strong_double_dash_flags_without_surface_are_literal() {
        let obs = IntentObservation {
            policy: RenderingPolicy::Adaptive,
            primary_text: "cargo test --workspace -- --test-threads=4".to_string(),
            provider_state: ProviderState::SingleProvider,
            surface_hint: None,
            process_hint: None,
            timing: None,
        };
        let d = route_intent(&obs);
        assert_eq!(d.route, RenderingRoute::LiteralIdentity);
        assert_eq!(d.rule_id, RuleId::LiteralCommand);
        // No surface/process hints → speech-only path still decides via strong flags.
        assert!(d.surface_degraded);
    }

    #[test]
    fn rule_id_and_provider_state_wire_names_round_trip() {
        for rule in [
            RuleId::DisputeCloud,
            RuleId::DisputePolicyForbid,
            RuleId::LiteralPreformatted,
            RuleId::LiteralCommand,
            RuleId::NaturalLocal,
            RuleId::ComplexCloud,
            RuleId::DefaultLocal,
        ] {
            assert_eq!(RuleId::parse(rule.as_str()), Some(rule));
        }
        for state in [
            ProviderState::ExactAgreement,
            ProviderState::PunctuationOnlyAgreement,
            ProviderState::SafeComplementary,
            ProviderState::ProtectedTokenDisagreement,
            ProviderState::SemanticDisagreement,
            ProviderState::SingleProvider,
        ] {
            assert_eq!(ProviderState::parse(state.as_str()), Some(state));
        }
    }
}
