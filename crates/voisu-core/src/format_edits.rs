//! Small formatting-edit contract for Developer Prompt Rendering.
//!
//! The formatting cloud job may propose only localized edits. The host parses,
//! validates, and applies them. There is no field for a free-form polished
//! string, and applying this contract never treats model prose as Delivery
//! text. Reconciliation stays on its own prompt and schema.
//!
//! Structural rejects: invalid JSON, stale fingerprints, unknown kinds,
//! overlapping ranges, or unanchored `before` text. After compose, the
//! formatting path keeps protected facts, heading-cue, prompt-artifact/outro,
//! and empty/summary gates. It does **not** require every rendered word to
//! appear in a Source Transcript — that invented-content rule stays on the
//! #139 derivation path only.

use serde_json::{Map, Value};

use crate::prompt_rendering::{RenderingPolicy, CLOSED_STRUCTURED_LABELS};
use crate::{is_command_shaped, is_text_sha256_fingerprint, text_sha256_fingerprint};

/// Contract id for diagnostics and later pipeline wiring.
pub const FORMAT_EDIT_CONTRACT_ID: &str = "voisu-dpr-format-edits-v1";
/// Envelope `version` accepted by this contract.
pub const FORMAT_EDIT_CONTRACT_VERSION: &str = "1";

/// Max raw candidate JSON body.
pub const MAX_FORMAT_EDIT_RESPONSE_BYTES: usize = 65_536;
/// Max JSON nesting depth for a candidate body.
pub const MAX_FORMAT_EDIT_JSON_DEPTH: usize = 8;
/// Max JSON nodes (objects/arrays/scalars) in a candidate body.
pub const MAX_FORMAT_EDIT_JSON_NODES: usize = 4_096;
/// Max `edits[]` entries.
pub const MAX_FORMAT_EDITS: usize = 64;
/// Max UTF-8 bytes per `before` / `after` field.
pub const MAX_FORMAT_EDIT_FIELD_UTF8_BYTES: usize = 2_048;

/// Closed formatting kinds. Unsupported strings fail closed.
pub const CLOSED_FORMAT_EDIT_KINDS: &[&str] = &[
    "punctuation",
    "casing",
    "whitespace_layout",
    "filler_removal",
    "clear_backtrack_removal",
    "quote_conversion",
    "structure",
    "bounded_wording",
];

/// One closed formatting operation the host can apply.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormatEditKind {
    Punctuation,
    Casing,
    WhitespaceLayout,
    FillerRemoval,
    ClearBacktrackRemoval,
    QuoteConversion,
    Structure,
    BoundedWording,
}

impl FormatEditKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Punctuation => "punctuation",
            Self::Casing => "casing",
            Self::WhitespaceLayout => "whitespace_layout",
            Self::FillerRemoval => "filler_removal",
            Self::ClearBacktrackRemoval => "clear_backtrack_removal",
            Self::QuoteConversion => "quote_conversion",
            Self::Structure => "structure",
            Self::BoundedWording => "bounded_wording",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "punctuation" => Some(Self::Punctuation),
            "casing" => Some(Self::Casing),
            "whitespace_layout" => Some(Self::WhitespaceLayout),
            "filler_removal" => Some(Self::FillerRemoval),
            "clear_backtrack_removal" => Some(Self::ClearBacktrackRemoval),
            "quote_conversion" => Some(Self::QuoteConversion),
            "structure" => Some(Self::Structure),
            "bounded_wording" => Some(Self::BoundedWording),
            _ => None,
        }
    }
}

/// Closed failure reasons. Any of these reject the whole candidate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormatEditErrorCode {
    Malformed,
    Oversize,
    Stale,
    UnknownKind,
    Unsorted,
    SpanOutOfBounds,
    SpanNotCharBoundary,
    AnchorMismatch,
    Overlap,
    Protected,
    HeadingWithoutCue,
    PromptArtifact,
    EmptyOrSummary,
}

impl FormatEditErrorCode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Malformed => "E_MALFORMED",
            Self::Oversize => "E_OVERSIZE",
            Self::Stale => "E_STALE",
            Self::UnknownKind => "E_UNKNOWN_KIND",
            Self::Unsorted => "E_UNSORTED",
            Self::SpanOutOfBounds => "E_SPAN_OUT_OF_BOUNDS",
            Self::SpanNotCharBoundary => "E_SPAN_NOT_CHAR_BOUNDARY",
            Self::AnchorMismatch => "E_ANCHOR_MISMATCH",
            Self::Overlap => "E_OVERLAP",
            Self::Protected => "E_PROTECTED",
            Self::HeadingWithoutCue => "E_HEADING_WITHOUT_CUE",
            Self::PromptArtifact => "E_PROMPT_ARTIFACT",
            Self::EmptyOrSummary => "E_EMPTY_OR_SUMMARY",
        }
    }
}

/// One host-applied replacement against the validated base.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FormatEdit {
    pub start_utf8: usize,
    pub end_utf8: usize,
    pub before: String,
    pub after: String,
    pub kind: FormatEditKind,
}

/// Parsed formatting candidate. Apply is a separate host step.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FormatEditCandidate {
    pub version: String,
    pub base_fingerprint: String,
    pub edits: Vec<FormatEdit>,
}

/// Host composition of a formatting candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FormatEditOutcome {
    pub accepted: bool,
    pub rendered: String,
    pub error: Option<FormatEditErrorCode>,
}

/// Host-owned safety context for the formatting apply path.
///
/// Protected tokens are extra facts the caller already extracted (dictionary
/// names, etc.). Mechanical families are also recognized from the base.
#[derive(Clone, Copy, Debug)]
pub struct FormatEditSafety<'a> {
    pub protected_tokens: &'a [&'a str],
    pub policy: RenderingPolicy,
}

impl Default for FormatEditSafety<'static> {
    fn default() -> Self {
        Self {
            protected_tokens: &[],
            policy: RenderingPolicy::Adaptive,
        }
    }
}

impl FormatEditOutcome {
    #[must_use]
    fn accepted(rendered: String) -> Self {
        Self {
            accepted: true,
            rendered,
            error: None,
        }
    }

    #[must_use]
    fn rejected(base: &str, error: FormatEditErrorCode) -> Self {
        Self {
            accepted: false,
            rendered: base.to_owned(),
            error: Some(error),
        }
    }
}

/// Parse untrusted formatting JSON. Does not apply edits and does not take
/// Delivery ownership of any model string.
pub fn parse_format_edit_candidate_json(raw: &[u8]) -> Result<FormatEditCandidate, FormatEditErrorCode> {
    if raw.len() > MAX_FORMAT_EDIT_RESPONSE_BYTES {
        return Err(FormatEditErrorCode::Oversize);
    }
    let value: Value = serde_json::from_slice(raw).map_err(|_| FormatEditErrorCode::Malformed)?;
    let (depth, nodes) = json_shape(&value);
    if depth > MAX_FORMAT_EDIT_JSON_DEPTH || nodes > MAX_FORMAT_EDIT_JSON_NODES {
        return Err(FormatEditErrorCode::Oversize);
    }
    parse_candidate(&value)
}

/// Validate and apply a parsed candidate against the host-owned base.
///
/// The rendered string is always composed here. A rejected candidate returns
/// the unchanged base so the caller can fall back to the local baseline.
#[must_use]
pub fn apply_format_edits(base: &str, candidate: &FormatEditCandidate) -> FormatEditOutcome {
    apply_format_edits_with(base, candidate, &FormatEditSafety::default())
}

/// Like [`apply_format_edits`], with caller policy and extra protected facts.
#[must_use]
pub fn apply_format_edits_with(
    base: &str,
    candidate: &FormatEditCandidate,
    safety: &FormatEditSafety<'_>,
) -> FormatEditOutcome {
    if candidate.version != FORMAT_EDIT_CONTRACT_VERSION {
        return FormatEditOutcome::rejected(base, FormatEditErrorCode::Malformed);
    }
    if !is_text_sha256_fingerprint(&candidate.base_fingerprint)
        || candidate.base_fingerprint != text_sha256_fingerprint(base)
    {
        return FormatEditOutcome::rejected(base, FormatEditErrorCode::Stale);
    }
    if let Err(error) = validate_edits(base, &candidate.edits) {
        return FormatEditOutcome::rejected(base, error);
    }
    let rendered = compose_edits(base, &candidate.edits);
    if let Err(error) = validate_format_render(base, &rendered, safety) {
        return FormatEditOutcome::rejected(base, error);
    }
    FormatEditOutcome::accepted(rendered)
}

/// Parse, validate, and apply one raw formatting body against the host base.
#[must_use]
pub fn apply_format_edit_candidate_json(base: &str, raw: &[u8]) -> FormatEditOutcome {
    apply_format_edit_candidate_json_with(base, raw, &FormatEditSafety::default())
}

/// Like [`apply_format_edit_candidate_json`], with caller safety context.
#[must_use]
pub fn apply_format_edit_candidate_json_with(
    base: &str,
    raw: &[u8],
    safety: &FormatEditSafety<'_>,
) -> FormatEditOutcome {
    match parse_format_edit_candidate_json(raw) {
        Ok(candidate) => apply_format_edits_with(base, &candidate, safety),
        Err(error) => FormatEditOutcome::rejected(base, error),
    }
}

fn parse_candidate(value: &Value) -> Result<FormatEditCandidate, FormatEditErrorCode> {
    let object = value.as_object().ok_or(FormatEditErrorCode::Malformed)?;
    if !has_exact_keys(object, &["version", "base_fingerprint", "edits"]) {
        return Err(FormatEditErrorCode::Malformed);
    }
    let version = object
        .get("version")
        .and_then(Value::as_str)
        .ok_or(FormatEditErrorCode::Malformed)?;
    if version != FORMAT_EDIT_CONTRACT_VERSION {
        return Err(FormatEditErrorCode::Malformed);
    }
    let base_fingerprint = object
        .get("base_fingerprint")
        .and_then(Value::as_str)
        .ok_or(FormatEditErrorCode::Malformed)?;
    if !is_text_sha256_fingerprint(base_fingerprint) {
        return Err(FormatEditErrorCode::Malformed);
    }
    let raw_edits = object
        .get("edits")
        .and_then(Value::as_array)
        .ok_or(FormatEditErrorCode::Malformed)?;
    if raw_edits.len() > MAX_FORMAT_EDITS {
        return Err(FormatEditErrorCode::Oversize);
    }
    let mut edits = Vec::with_capacity(raw_edits.len());
    for raw in raw_edits {
        edits.push(parse_edit(raw)?);
    }
    if !edits.windows(2).all(|pair| {
        (pair[0].start_utf8, pair[0].end_utf8) <= (pair[1].start_utf8, pair[1].end_utf8)
    }) {
        return Err(FormatEditErrorCode::Unsorted);
    }
    Ok(FormatEditCandidate {
        version: version.to_owned(),
        base_fingerprint: base_fingerprint.to_owned(),
        edits,
    })
}

fn parse_edit(value: &Value) -> Result<FormatEdit, FormatEditErrorCode> {
    let object = value.as_object().ok_or(FormatEditErrorCode::Malformed)?;
    if !has_exact_keys(
        object,
        &["start_utf8", "end_utf8", "before", "after", "kind"],
    ) {
        return Err(FormatEditErrorCode::Malformed);
    }
    let kind = object
        .get("kind")
        .and_then(Value::as_str)
        .ok_or(FormatEditErrorCode::Malformed)?;
    let Some(kind) = FormatEditKind::parse(kind) else {
        return Err(FormatEditErrorCode::UnknownKind);
    };
    let before = object
        .get("before")
        .and_then(Value::as_str)
        .ok_or(FormatEditErrorCode::Malformed)?;
    let after = object
        .get("after")
        .and_then(Value::as_str)
        .ok_or(FormatEditErrorCode::Malformed)?;
    if before.len() > MAX_FORMAT_EDIT_FIELD_UTF8_BYTES
        || after.len() > MAX_FORMAT_EDIT_FIELD_UTF8_BYTES
    {
        return Err(FormatEditErrorCode::Oversize);
    }
    let start = object
        .get("start_utf8")
        .and_then(as_usize)
        .ok_or(FormatEditErrorCode::Malformed)?;
    let end = object
        .get("end_utf8")
        .and_then(as_usize)
        .ok_or(FormatEditErrorCode::Malformed)?;
    Ok(FormatEdit {
        start_utf8: start,
        end_utf8: end,
        before: before.to_owned(),
        after: after.to_owned(),
        kind,
    })
}

fn validate_edits(base: &str, edits: &[FormatEdit]) -> Result<(), FormatEditErrorCode> {
    for (index, edit) in edits.iter().enumerate() {
        if edit.start_utf8 > edit.end_utf8 || edit.end_utf8 > base.len() {
            return Err(FormatEditErrorCode::SpanOutOfBounds);
        }
        if !base.is_char_boundary(edit.start_utf8) || !base.is_char_boundary(edit.end_utf8) {
            return Err(FormatEditErrorCode::SpanNotCharBoundary);
        }
        if base.get(edit.start_utf8..edit.end_utf8) != Some(edit.before.as_str()) {
            return Err(FormatEditErrorCode::AnchorMismatch);
        }
        for other in &edits[index + 1..] {
            if ranges_overlap(
                (edit.start_utf8, edit.end_utf8),
                (other.start_utf8, other.end_utf8),
            ) {
                return Err(FormatEditErrorCode::Overlap);
            }
        }
    }
    Ok(())
}

fn compose_edits(base: &str, edits: &[FormatEdit]) -> String {
    let mut rendered = base.to_owned();
    let mut replacements: Vec<(usize, usize, &str)> = edits
        .iter()
        .map(|edit| (edit.start_utf8, edit.end_utf8, edit.after.as_str()))
        .collect();
    replacements.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
    for (start, end, after) in replacements {
        rendered.replace_range(start..end, after);
    }
    rendered
}

fn ranges_overlap(left: (usize, usize), right: (usize, usize)) -> bool {
    left.0 < right.1 && right.0 < left.1
}

fn has_exact_keys(object: &Map<String, Value>, expected: &[&str]) -> bool {
    object.len() == expected.len() && expected.iter().all(|key| object.contains_key(*key))
}

fn as_usize(value: &Value) -> Option<usize> {
    value.as_u64().and_then(|number| usize::try_from(number).ok())
}

fn json_shape(value: &Value) -> (usize, usize) {
    match value {
        Value::Array(values) => {
            let mut depth = 1usize;
            let mut nodes = 1usize;
            for child in values {
                let (child_depth, child_nodes) = json_shape(child);
                depth = depth.max(child_depth.saturating_add(1));
                nodes = nodes.saturating_add(child_nodes);
            }
            (depth, nodes)
        }
        Value::Object(values) => {
            let mut depth = 1usize;
            let mut nodes = 1usize;
            for child in values.values() {
                let (child_depth, child_nodes) = json_shape(child);
                depth = depth.max(child_depth.saturating_add(1));
                nodes = nodes.saturating_add(child_nodes);
            }
            (depth, nodes)
        }
        _ => (0, 1),
    }
}

const NEGATIONS: &[&str] = &[
    "no", "not", "never", "cannot", "can't", "cant", "don't", "dont", "won't", "wont", "isn't",
    "isnt", "aren't", "arent", "ain't", "aint",
];

const WEEKDAYS: &[&str] = &[
    "monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday",
];

const MONTHS: &[&str] = &[
    "january", "february", "march", "april", "may", "june", "july", "august", "september",
    "october", "november", "december",
];

const SECTION_CUES: &[(&[&str], &str)] = &[
    (&["acceptance", "criteria"], "Acceptance Criteria"),
    (&["requirements"], "Requirements"),
    (&["constraints"], "Constraints"),
    (&["context"], "Context"),
    (&["steps"], "Steps"),
    (&["files"], "Files"),
    (&["notes"], "Notes"),
    (&["goal"], "Goal"),
];

const PROMPT_ARTIFACTS: &[&str] = &[
    "ignore previous instructions",
    "ignore all instructions",
    "system:",
    "assistant:",
    "<|system|>",
    "<|assistant|>",
    "### instruction",
];

const HALLUCINATED_OUTROS: &[&str] = &[
    "thank you for watching",
    "thanks for watching",
    "like and subscribe",
    "subtitles by",
    "transcribed by",
];

const SUMMARY_SOURCE_WORD_FLOOR: usize = 12;

fn validate_format_render(
    base: &str,
    rendered: &str,
    safety: &FormatEditSafety<'_>,
) -> Result<(), FormatEditErrorCode> {
    if is_empty_or_large_summary(base, rendered) {
        return Err(FormatEditErrorCode::EmptyOrSummary);
    }
    if introduces_prompt_artifact_or_outro(base, rendered) {
        return Err(FormatEditErrorCode::PromptArtifact);
    }
    if introduces_heading_without_cue(base, rendered, safety.policy) {
        return Err(FormatEditErrorCode::HeadingWithoutCue);
    }
    if mutates_protected_fact(base, rendered, safety.protected_tokens) {
        return Err(FormatEditErrorCode::Protected);
    }
    Ok(())
}

fn is_empty_or_large_summary(base: &str, rendered: &str) -> bool {
    if !base.trim().is_empty() && rendered.trim().is_empty() {
        return true;
    }
    let source_words = lexical_word_count(base);
    let rendered_words = lexical_word_count(rendered);
    source_words >= SUMMARY_SOURCE_WORD_FLOOR && rendered_words.saturating_mul(2) < source_words
}

fn introduces_prompt_artifact_or_outro(base: &str, rendered: &str) -> bool {
    let base_lower = base.to_ascii_lowercase();
    let rendered_lower = rendered.to_ascii_lowercase();
    let new_artifact = PROMPT_ARTIFACTS.iter().any(|marker| {
        rendered_lower.contains(marker) && !base_lower.contains(marker)
    });
    new_artifact || (has_anchored_outro(rendered) && !has_anchored_outro(base))
}

fn has_anchored_outro(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    let tail = lower.trim_end_matches(|character: char| !character.is_alphanumeric());
    let outro = final_sentence(&lower);
    HALLUCINATED_OUTROS
        .iter()
        .any(|suffix| outro.starts_with(suffix) || tail.ends_with(suffix))
}

fn final_sentence(text: &str) -> &str {
    let trimmed = text.trim_end_matches(|character: char| !character.is_alphanumeric());
    let after_terminator = trimmed
        .char_indices()
        .filter(|&(index, character)| {
            matches!(character, '.' | '?' | '!')
                && trimmed[index + character.len_utf8()..]
                    .chars()
                    .next()
                    .is_some_and(char::is_whitespace)
        })
        .map(|(index, character)| index + character.len_utf8())
        .next_back()
        .unwrap_or(0);
    trimmed[after_terminator..].trim_start_matches(|character: char| !character.is_alphanumeric())
}

fn introduces_heading_without_cue(base: &str, rendered: &str, policy: RenderingPolicy) -> bool {
    let source_headings = structural_headings(base);
    let licensed = licensed_heading_labels(base);
    for heading in structural_headings(rendered) {
        if source_headings
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&heading))
        {
            continue;
        }
        if policy == RenderingPolicy::Natural {
            return true;
        }
        let Some(canonical) = CLOSED_STRUCTURED_LABELS
            .iter()
            .copied()
            .find(|label| *label == heading)
        else {
            return true;
        };
        if !licensed.iter().any(|label| *label == canonical) {
            return true;
        }
    }
    false
}

fn licensed_heading_labels(source: &str) -> Vec<&'static str> {
    let tokens = lexical_words(source);
    let mut licensed = Vec::new();
    for &(phrase, label) in SECTION_CUES {
        if tokens.windows(phrase.len()).any(|window| window == phrase) {
            licensed.push(label);
        }
    }
    licensed
}

fn structural_headings(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        let Some((label, rest)) = trimmed.split_once(':') else {
            continue;
        };
        let label = label.trim();
        if label.is_empty()
            || !label
                .chars()
                .next()
                .is_some_and(|character| character.is_ascii_alphabetic())
            || !label
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == ' ')
        {
            continue;
        }
        let closed = CLOSED_STRUCTURED_LABELS
            .iter()
            .any(|known| known.eq_ignore_ascii_case(label));
        if closed || rest.trim().is_empty() || is_title_case(label) {
            found.push(label.to_owned());
        }
    }
    found
}

fn is_title_case(value: &str) -> bool {
    if value.is_empty() || !value.chars().any(|character| character.is_alphabetic()) {
        return false;
    }
    let mut word_start = true;
    let mut saw_cased = false;
    for character in value.chars() {
        if !character.is_alphabetic() {
            word_start = true;
            continue;
        }
        if word_start {
            if !character.is_uppercase() {
                return false;
            }
            saw_cased = true;
            word_start = false;
        } else if character.is_uppercase() {
            return false;
        }
    }
    saw_cased
}

fn mutates_protected_fact(base: &str, rendered: &str, extra: &[&str]) -> bool {
    host_protected_facts(base, extra)
        .into_iter()
        .any(|fact| !fact.is_empty() && !protected_fact_survives(rendered, &fact))
}

fn protected_fact_survives(rendered: &str, fact: &str) -> bool {
    if rendered.contains(fact) {
        return true;
    }
    let lower = fact.to_ascii_lowercase();
    let case_fold = NEGATIONS.contains(&lower.as_str())
        || WEEKDAYS.contains(&lower.as_str())
        || MONTHS.contains(&lower.as_str());
    case_fold && rendered.to_ascii_lowercase().contains(&lower)
}

fn host_protected_facts(base: &str, extra: &[&str]) -> Vec<String> {
    let mut facts = Vec::new();
    push_unique_fact(&mut facts, collect_quoted_interiors(base));
    let mut sentence_initial = true;
    for raw in base.split_whitespace() {
        let token = trim_token_edges(raw);
        if token.is_empty() {
            continue;
        }
        if is_protected_token(token, sentence_initial) {
            push_unique_fact(&mut facts, [token.to_owned()]);
        }
        sentence_initial = raw.ends_with(['.', '?', '!']);
    }
    if is_command_shaped(base) {
        for raw in base.split_whitespace() {
            let token = trim_token_edges(raw);
            if looks_like_command_atom(token) {
                push_unique_fact(&mut facts, [token.to_owned()]);
            }
        }
    }
    for token in extra {
        if base.contains(*token) {
            push_unique_fact(&mut facts, [(*token).to_owned()]);
        }
    }
    facts
}

fn is_protected_token(token: &str, sentence_initial: bool) -> bool {
    let lower = token.to_ascii_lowercase();
    NEGATIONS.contains(&lower.as_str())
        || WEEKDAYS.contains(&lower.as_str())
        || MONTHS.contains(&lower.as_str())
        || is_technical_token(token)
        || is_time_token(token)
        || is_proper_name(token, sentence_initial)
}

fn is_technical_token(token: &str) -> bool {
    token.bytes().any(|byte| byte.is_ascii_digit())
        || token.contains("://")
        || token.contains('/')
        || token.contains('\\')
        || token.starts_with('-')
        || token.contains('_')
        || token.contains("::")
        || token.contains('@')
        || token.contains(['(', ')', '[', ']', '{', '}', '='])
}

fn is_time_token(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    let stripped = lower.trim_end_matches(['.', ',', ';']);
    matches!(stripped.chars().last(), Some('a' | 'p'))
        && stripped.len() > 2
        && stripped.as_bytes()[stripped.len() - 2] == b'm'
        && stripped[..stripped.len() - 2]
            .chars()
            .all(|character| character.is_ascii_digit() || character == ':')
}

fn is_proper_name(token: &str, sentence_initial: bool) -> bool {
    if sentence_initial || token.chars().count() < 2 {
        return false;
    }
    if CLOSED_STRUCTURED_LABELS
        .iter()
        .any(|label| label.eq_ignore_ascii_case(token))
    {
        return false;
    }
    let mut chars = token.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_uppercase()
        && first.is_alphabetic()
        && chars.all(|character| character.is_lowercase() || character == '-')
}

fn looks_like_command_atom(token: &str) -> bool {
    is_technical_token(token)
        || matches!(
            token.to_ascii_lowercase().as_str(),
            "cargo"
                | "npm"
                | "pnpm"
                | "yarn"
                | "git"
                | "docker"
                | "kubectl"
                | "make"
                | "python"
                | "python3"
                | "pip"
                | "curl"
                | "ssh"
                | "scp"
                | "go"
                | "bazel"
                | "ninja"
                | "test"
                | "build"
                | "clippy"
                | "fmt"
        )
}

fn collect_quoted_interiors(text: &str) -> Vec<String> {
    let mut interiors = Vec::new();
    let lower = text.to_ascii_lowercase();
    let mut search_from = 0;
    while let Some(open_relative) = lower[search_from..].find("quote ") {
        let interior_start = search_from + open_relative + "quote ".len();
        let Some(close_relative) = lower[interior_start..].find(" unquote") else {
            break;
        };
        let interior_end = interior_start + close_relative;
        if interior_start < interior_end {
            interiors.push(text[interior_start..interior_end].to_owned());
        }
        search_from = interior_end + " unquote".len();
    }
    push_paired_quote_interiors(text, '"', &mut interiors);
    push_paired_quote_interiors(text, '\'', &mut interiors);
    interiors
}

fn push_paired_quote_interiors(text: &str, delimiter: char, interiors: &mut Vec<String>) {
    let mut start = None;
    for (index, character) in text.char_indices() {
        if character != delimiter {
            continue;
        }
        match start {
            None => start = Some(index + character.len_utf8()),
            Some(open) => {
                if open < index {
                    interiors.push(text[open..index].to_owned());
                }
                start = None;
            }
        }
    }
}

fn push_unique_fact(facts: &mut Vec<String>, incoming: impl IntoIterator<Item = String>) {
    for fact in incoming {
        if !fact.is_empty() && !facts.iter().any(|existing| existing == &fact) {
            facts.push(fact);
        }
    }
}

fn trim_token_edges(token: &str) -> &str {
    token.trim_matches(|character: char| {
        matches!(
            character,
            ',' | ';' | '!' | '?' | '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}'
        )
    })
}

fn lexical_words(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric() && character != '\'')
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn lexical_word_count(text: &str) -> usize {
    lexical_words(text).len()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn candidate_json(base: &str, edits: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "version": FORMAT_EDIT_CONTRACT_VERSION,
            "base_fingerprint": text_sha256_fingerprint(base),
            "edits": edits,
        }))
        .expect("candidate JSON")
    }

    fn edit(start: usize, end: usize, before: &str, after: &str, kind: &str) -> Value {
        json!({
            "start_utf8": start,
            "end_utf8": end,
            "before": before,
            "after": after,
            "kind": kind,
        })
    }

    #[test]
    fn closed_kinds_match_parser() {
        assert_eq!(CLOSED_FORMAT_EDIT_KINDS.len(), 8);
        for kind in CLOSED_FORMAT_EDIT_KINDS {
            let parsed = FormatEditKind::parse(kind).expect(kind);
            assert_eq!(parsed.as_str(), *kind);
        }
        assert!(FormatEditKind::parse("keep").is_none());
        assert!(FormatEditKind::parse("derivation").is_none());
    }

    #[test]
    fn host_applies_structure_edit_and_never_takes_model_prose() {
        let base = "goal ship the rust parser";
        let raw = candidate_json(
            base,
            json!([edit(0, 4, "goal", "Goal:\n", "structure")]),
        );
        let parsed = parse_format_edit_candidate_json(&raw).expect("parse");
        assert_eq!(parsed.version, "1");
        assert_eq!(parsed.edits.len(), 1);
        assert_eq!(parsed.edits[0].kind, FormatEditKind::Structure);

        let outcome = apply_format_edits(base, &parsed);
        assert!(outcome.accepted);
        assert_eq!(outcome.rendered, "Goal:\n ship the rust parser");
        assert!(outcome.error.is_none());

        let prose = json!({
            "version": "1",
            "base_fingerprint": text_sha256_fingerprint(base),
            "rendered": "Goal: invent a new API and ship it",
        });
        let rejected = apply_format_edit_candidate_json(base, prose.to_string().as_bytes());
        assert!(!rejected.accepted);
        assert_eq!(rejected.error, Some(FormatEditErrorCode::Malformed));
        assert_eq!(rejected.rendered, base);
    }

    #[test]
    fn applies_closed_kinds_without_overlapping() {
        let base = "um the goal is quote hello unquote";
        let um_end = 2;
        let goal_start = base.find("goal").unwrap();
        let quote_start = base.find("quote hello unquote").unwrap();
        let quote_end = quote_start + "quote hello unquote".len();
        let raw = candidate_json(
            base,
            json!([
                edit(0, um_end, "um", "", "filler_removal"),
                edit(goal_start, goal_start + 4, "goal", "Goal", "casing"),
                edit(
                    quote_start,
                    quote_end,
                    "quote hello unquote",
                    "\"hello\"",
                    "quote_conversion"
                ),
            ]),
        );
        let outcome = apply_format_edit_candidate_json(base, &raw);
        assert!(outcome.accepted, "{outcome:?}");
        assert_eq!(outcome.rendered, " the Goal is \"hello\"");
    }

    #[test]
    fn empty_edits_are_host_identity() {
        let base = "leave this wording";
        let outcome = apply_format_edit_candidate_json(base, &candidate_json(base, json!([])));
        assert!(outcome.accepted);
        assert_eq!(outcome.rendered, base);
    }

    #[test]
    fn stale_fingerprint_and_unknown_kind_reject_the_candidate() {
        let base = "goal";
        let stale = json!({
            "version": "1",
            "base_fingerprint": format!("sha256:{}", "0".repeat(64)),
            "edits": [edit(0, 4, "goal", "Goal:\n", "structure")],
        });
        let outcome = apply_format_edit_candidate_json(base, stale.to_string().as_bytes());
        assert!(!outcome.accepted);
        assert_eq!(outcome.error, Some(FormatEditErrorCode::Stale));
        assert_eq!(outcome.rendered, base);

        let unknown = apply_format_edit_candidate_json(
            base,
            &candidate_json(base, json!([edit(0, 4, "goal", "Goal", "rewrite")])),
        );
        assert_eq!(unknown.error, Some(FormatEditErrorCode::UnknownKind));
        assert_eq!(unknown.rendered, base);
    }

    #[test]
    fn overlapping_unsorted_and_unanchored_edits_fail_closed() {
        let base = "goal notes";
        let overlap = apply_format_edit_candidate_json(
            base,
            &candidate_json(
                base,
                json!([
                    edit(0, 4, "goal", "Goal", "casing"),
                    edit(2, 6, "al n", "AL N", "casing"),
                ]),
            ),
        );
        assert_eq!(overlap.error, Some(FormatEditErrorCode::Overlap));

        let unsorted = apply_format_edit_candidate_json(
            base,
            &candidate_json(
                base,
                json!([
                    edit(5, 10, "notes", "Notes", "casing"),
                    edit(0, 4, "goal", "Goal", "casing"),
                ]),
            ),
        );
        assert_eq!(unsorted.error, Some(FormatEditErrorCode::Unsorted));

        let mismatch = apply_format_edit_candidate_json(
            base,
            &candidate_json(base, json!([edit(0, 4, "GOAL", "Goal", "casing")])),
        );
        assert_eq!(mismatch.error, Some(FormatEditErrorCode::AnchorMismatch));
    }

    #[test]
    fn malformed_and_oversize_bodies_fail_closed() {
        let base = "goal";
        assert_eq!(
            apply_format_edit_candidate_json(base, b"not json").error,
            Some(FormatEditErrorCode::Malformed)
        );
        assert_eq!(
            apply_format_edit_candidate_json(base, &vec![b' '; MAX_FORMAT_EDIT_RESPONSE_BYTES + 1])
                .error,
            Some(FormatEditErrorCode::Oversize)
        );

        let missing_kind = json!({
            "version": "1",
            "base_fingerprint": text_sha256_fingerprint(base),
            "edits": [{
                "start_utf8": 0,
                "end_utf8": 4,
                "before": "goal",
                "after": "Goal",
            }],
        });
        assert_eq!(
            apply_format_edit_candidate_json(base, missing_kind.to_string().as_bytes()).error,
            Some(FormatEditErrorCode::Malformed)
        );

        let out_of_bounds = apply_format_edit_candidate_json(
            base,
            &candidate_json(base, json!([edit(0, 99, "goal", "Goal", "casing")])),
        );
        assert_eq!(out_of_bounds.error, Some(FormatEditErrorCode::SpanOutOfBounds));

        let mid_char = "gøal";
        let cut = apply_format_edit_candidate_json(
            mid_char,
            &candidate_json(mid_char, json!([edit(1, 2, "ø", "o", "bounded_wording")])),
        );
        assert_eq!(cut.error, Some(FormatEditErrorCode::SpanNotCharBoundary));
    }

    #[test]
    fn punctuation_and_layout_kinds_compose_locally() {
        let punct = apply_format_edit_candidate_json(
            "ship it",
            &candidate_json("ship it", json!([edit(7, 7, "", ".", "punctuation")])),
        );
        assert_eq!(punct.rendered, "ship it.");

        let layout = apply_format_edit_candidate_json(
            "one two",
            &candidate_json(
                "one two",
                json!([edit(3, 4, " ", "\n", "whitespace_layout")]),
            ),
        );
        assert_eq!(layout.rendered, "one\ntwo");
    }

    #[test]
    fn filler_removal_cannot_delete_negation() {
        let base = "do not deploy";
        let outcome = apply_format_edit_candidate_json(
            base,
            &candidate_json(base, json!([edit(3, 6, "not", "", "filler_removal")])),
        );
        assert_eq!(outcome.error, Some(FormatEditErrorCode::Protected));
        assert_eq!(outcome.rendered, base);
    }

    #[test]
    fn filler_removal_cannot_delete_url_path_or_name() {
        let cases = [
            ("open https://example.test/a now", "https://example.test/a"),
            (
                "edit crates/voisu-core/src/lib.rs today",
                "crates/voisu-core/src/lib.rs",
            ),
            ("ask Alice to review it", "Alice"),
        ];
        for (base, protected) in cases {
            let start = base.find(protected).expect("protected fact in base");
            let outcome = apply_format_edit_candidate_json(
                base,
                &candidate_json(
                    base,
                    json!([edit(
                        start,
                        start + protected.len(),
                        protected,
                        "",
                        "filler_removal"
                    )]),
                ),
            );
            assert_eq!(
                outcome.error,
                Some(FormatEditErrorCode::Protected),
                "expected protected reject for {protected:?}: {outcome:?}"
            );
            assert_eq!(outcome.rendered, base);
        }
    }

    #[test]
    fn kind_labeled_backtrack_cannot_delete_negation() {
        let base = "send X no wait Y";
        let start = base.find("X no wait ").expect("backtrack in base");
        let outcome = apply_format_edit_candidate_json(
            base,
            &candidate_json(
                base,
                json!([edit(
                    start,
                    start + "X no wait ".len(),
                    "X no wait ",
                    "",
                    "clear_backtrack_removal"
                )]),
            ),
        );
        assert_eq!(outcome.error, Some(FormatEditErrorCode::Protected));
        assert_eq!(outcome.rendered, base);
    }

    #[test]
    fn formatting_may_introduce_words_absent_from_the_source() {
        let base = "pls ship the rust parser";
        let outcome = apply_format_edit_candidate_json(
            base,
            &candidate_json(base, json!([edit(0, 3, "pls", "Please", "bounded_wording")])),
        );
        assert!(outcome.accepted, "{outcome:?}");
        assert_eq!(outcome.rendered, "Please ship the rust parser");
        assert!(
            !base.to_ascii_lowercase().contains("please"),
            "the new wording must not already be in the source"
        );
    }

    #[test]
    fn protected_facts_must_survive_formatting() {
        let cases = [
            (
                "ask Alice to review it",
                4,
                9,
                "Alice",
                "Alicia",
                "bounded_wording",
            ),
            (
                "do not deploy tonight",
                3,
                6,
                "not",
                "",
                "bounded_wording",
            ),
            (
                "open https://example.test/a now",
                5,
                27,
                "https://example.test/a",
                "https://evil.test/a",
                "bounded_wording",
            ),
            (
                "edit crates/voisu-core/src/lib.rs today",
                5,
                33,
                "crates/voisu-core/src/lib.rs",
                "crates/voisu-core/src/main.rs",
                "bounded_wording",
            ),
            (
                "meet at 3pm tomorrow",
                8,
                11,
                "3pm",
                "4pm",
                "bounded_wording",
            ),
            (
                "ship on 2026-08-16 please",
                8,
                18,
                "2026-08-16",
                "2026-08-17",
                "bounded_wording",
            ),
            (
                "say quote leave this unquote now",
                10,
                20,
                "leave this",
                "change this",
                "quote_conversion",
            ),
        ];
        for (base, start, end, before, after, kind) in cases {
            let outcome = apply_format_edit_candidate_json(
                base,
                &candidate_json(base, json!([edit(start, end, before, after, kind)])),
            );
            assert_eq!(
                outcome.error,
                Some(FormatEditErrorCode::Protected),
                "expected protected reject for {before:?} in {base:?}: {outcome:?}"
            );
            assert_eq!(outcome.rendered, base);
        }

        let weekday = "meet wednesday morning";
        let cased_weekday = apply_format_edit_candidate_json(
            weekday,
            &candidate_json(
                weekday,
                json!([edit(5, 14, "wednesday", "Wednesday", "casing")]),
            ),
        );
        assert!(cased_weekday.accepted, "{cased_weekday:?}");
        assert_eq!(cased_weekday.rendered, "meet Wednesday morning");

        let command = "run cargo test --workspace";
        let flag_start = command.find("--workspace").unwrap();
        let mutated = apply_format_edit_candidate_json(
            command,
            &candidate_json(
                command,
                json!([edit(
                    flag_start,
                    flag_start + "--workspace".len(),
                    "--workspace",
                    "--all",
                    "bounded_wording"
                )]),
            ),
        );
        assert_eq!(mutated.error, Some(FormatEditErrorCode::Protected));
        assert_eq!(mutated.rendered, command);
    }

    #[test]
    fn caller_supplied_name_is_protected_even_when_not_title_case() {
        let base = "restart voisu-daemon after the change";
        let start = base.find("voisu-daemon").unwrap();
        let outcome = apply_format_edits_with(
            base,
            &parse_format_edit_candidate_json(&candidate_json(
                base,
                json!([edit(
                    start,
                    start + "voisu-daemon".len(),
                    "voisu-daemon",
                    "voisu-service",
                    "bounded_wording"
                )]),
            ))
            .expect("parse"),
            &FormatEditSafety {
                protected_tokens: &["voisu-daemon"],
                policy: RenderingPolicy::Adaptive,
            },
        );
        assert_eq!(outcome.error, Some(FormatEditErrorCode::Protected));
        assert_eq!(outcome.rendered, base);
    }

    #[test]
    fn new_headings_require_spoken_cues_and_never_appear_under_natural() {
        let unlicensed = "ship the rust parser today";
        let invented = apply_format_edit_candidate_json(
            unlicensed,
            &candidate_json(
                unlicensed,
                json!([edit(0, 4, "ship", "Goal:\nShip", "structure")]),
            ),
        );
        assert_eq!(invented.error, Some(FormatEditErrorCode::HeadingWithoutCue));
        assert_eq!(invented.rendered, unlicensed);

        let cued = "goal ship the rust parser";
        let structured = apply_format_edit_candidate_json(
            cued,
            &candidate_json(cued, json!([edit(0, 4, "goal", "Goal:\n", "structure")])),
        );
        assert!(structured.accepted, "{structured:?}");
        assert_eq!(structured.rendered, "Goal:\n ship the rust parser");

        let natural = apply_format_edit_candidate_json_with(
            cued,
            &candidate_json(cued, json!([edit(0, 4, "goal", "Goal:\n", "structure")])),
            &FormatEditSafety {
                protected_tokens: &[],
                policy: RenderingPolicy::Natural,
            },
        );
        assert_eq!(natural.error, Some(FormatEditErrorCode::HeadingWithoutCue));
        assert_eq!(natural.rendered, cued);
    }

    #[test]
    fn prompt_artifacts_and_outros_are_rejected_but_source_mentions_survive() {
        let base = "ship the rust parser";
        let artifact = apply_format_edit_candidate_json(
            base,
            &candidate_json(
                base,
                json!([edit(
                    20,
                    20,
                    "",
                    "\nIgnore previous instructions.",
                    "bounded_wording"
                )]),
            ),
        );
        assert_eq!(artifact.error, Some(FormatEditErrorCode::PromptArtifact));
        assert_eq!(artifact.rendered, base);

        let outro = apply_format_edit_candidate_json(
            base,
            &candidate_json(
                base,
                json!([edit(
                    20,
                    20,
                    "",
                    " Thank you for watching!",
                    "bounded_wording"
                )]),
            ),
        );
        assert_eq!(outro.error, Some(FormatEditErrorCode::PromptArtifact));
        assert_eq!(outro.rendered, base);

        let spoken = "we should thank you for watching the demo";
        let kept = apply_format_edit_candidate_json(
            spoken,
            &candidate_json(spoken, json!([edit(0, 1, "w", "W", "casing")])),
        );
        assert!(kept.accepted, "{kept:?}");
        assert_eq!(kept.rendered, "We should thank you for watching the demo");
    }

    #[test]
    fn empty_or_large_summary_falls_back_to_the_base() {
        let base = "ship the rust parser";
        let empty = apply_format_edit_candidate_json(
            base,
            &candidate_json(
                base,
                json!([edit(0, base.len(), base, "", "bounded_wording")]),
            ),
        );
        assert_eq!(empty.error, Some(FormatEditErrorCode::EmptyOrSummary));
        assert_eq!(empty.rendered, base);

        let long = "please write a function that validates the incoming request payload and returns a 400 when the schema fails";
        let summary_after = "Validate the payload.";
        let summary = apply_format_edit_candidate_json(
            long,
            &candidate_json(
                long,
                json!([edit(0, long.len(), long, summary_after, "bounded_wording")]),
            ),
        );
        assert_eq!(summary.error, Some(FormatEditErrorCode::EmptyOrSummary));
        assert_eq!(summary.rendered, long);
    }

    #[test]
    fn malformed_and_unsupported_kinds_still_reject_to_the_base() {
        let base = "goal ship the rust parser";
        assert_eq!(
            apply_format_edit_candidate_json(base, b"{").error,
            Some(FormatEditErrorCode::Malformed)
        );
        assert_eq!(
            apply_format_edit_candidate_json(
                base,
                &candidate_json(base, json!([edit(0, 4, "goal", "Goal", "rewrite")])),
            )
            .error,
            Some(FormatEditErrorCode::UnknownKind)
        );
    }
}
