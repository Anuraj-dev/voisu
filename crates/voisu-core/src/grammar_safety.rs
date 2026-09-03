//! Production Minimal Grammar candidate parser, validator, and composer (SW4 / #100).
//!
//! Provider JSON can propose only localized grammar patches. It cannot carry a
//! rendered transcript or construct the sealed [`FormattingBaseline`]. Any
//! malformed, stale, unsafe, or unmappable edit rejects the whole candidate and
//! preserves the fresh local Formatting baseline (B1-A/B2-A/B3-A).

use std::collections::BTreeSet;
use std::fmt;

use serde::Deserialize;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};

use crate::{
    FormattingBaseline, MAX_VALIDATED_TRANSCRIPT_UTF8_BYTES, SourceAnchor, SourceSpan,
    is_text_sha256_fingerprint, parse_formatting_commands, text_sha256_fingerprint,
};

pub const MAX_GRAMMAR_RESPONSE_BYTES: usize = 65_536;
pub const MAX_GRAMMAR_JSON_DEPTH: usize = 8;
pub const MAX_GRAMMAR_JSON_NODES: usize = 4_096;
pub const MAX_GRAMMAR_EDITS: usize = 32;
pub const MAX_GRAMMAR_EDIT_FIELD_UTF8_BYTES: usize = 256;
pub const MAX_GRAMMAR_DIAGNOSTIC_UTF8_BYTES: usize = 128;

const PROMPT_MARKERS: &[&str] = &[
    "ignore previous instructions",
    "system prompt",
    "developer message",
    "you are chatgpt",
    "system:",
    "user:",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GrammarOutcome {
    Both,
    GrammarOnly,
    FormattingOnly,
    Unchanged,
}

impl GrammarOutcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Both => "both",
            Self::GrammarOnly => "grammar_only",
            Self::FormattingOnly => "formatting_only",
            Self::Unchanged => "unchanged",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum GrammarErrorCode {
    FormattingIdentity,
    FormattingDerivation,
    Malformed,
    Oversize,
    StaleGrammar,
    Unsorted,
    SpanOutOfBounds,
    SpanNotCharBoundary,
    NotTokenBoundary,
    AnchorMismatch,
    ProtectedSpan,
    UnknownRule,
    RuleContext,
    Unmappable,
    Overlap,
}

impl GrammarErrorCode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FormattingIdentity => "E_FORMATTING_IDENTITY",
            Self::FormattingDerivation => "E_FORMATTING_DERIVATION",
            Self::Malformed => "E_MALFORMED",
            Self::Oversize => "E_OVERSIZE",
            Self::StaleGrammar => "E_STALE_GRAMMAR",
            Self::Unsorted => "E_UNSORTED",
            Self::SpanOutOfBounds => "E_SPAN_OUT_OF_BOUNDS",
            Self::SpanNotCharBoundary => "E_SPAN_NOT_CHAR_BOUNDARY",
            Self::NotTokenBoundary => "E_NOT_TOKEN_BOUNDARY",
            Self::AnchorMismatch => "E_ANCHOR_MISMATCH",
            Self::ProtectedSpan => "E_PROTECTED_SPAN",
            Self::UnknownRule => "E_UNKNOWN_RULE",
            Self::RuleContext => "E_RULE_CONTEXT",
            Self::Unmappable => "E_UNMAPPABLE",
            Self::Overlap => "E_OVERLAP",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrammarDiagnostic {
    pub code: GrammarErrorCode,
    pub message: String,
    pub edit_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrammarSafetyResult {
    pub outcome: GrammarOutcome,
    pub rendered: String,
    pub diagnostics: Vec<GrammarDiagnostic>,
}

impl GrammarSafetyResult {
    #[must_use]
    pub fn error_codes(&self) -> Vec<&'static str> {
        self.diagnostics
            .iter()
            .map(|diagnostic| diagnostic.code.as_str())
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct GrammarSafetyOptions<'a> {
    pub dictionary_terms: &'a [&'a str],
    pub protected_names: &'a [&'a str],
}

#[derive(Clone, Debug)]
struct GrammarCandidate {
    base_version: String,
    base_fingerprint: String,
    edits: Vec<GrammarEdit>,
}

#[derive(Clone, Debug)]
struct GrammarEdit {
    id: String,
    rule_id: String,
    start_utf8: usize,
    end_utf8: usize,
    before: String,
    after: String,
}

#[derive(Clone, Copy, Debug)]
struct Token<'a> {
    start: usize,
    end: usize,
    text: &'a str,
}

/// Parse, validate, and compose one raw provider candidate.
///
/// The only content inputs are the immutable English Validated Transcript and
/// its formatter-owned baseline. The raw body is rejected before parsing when
/// it exceeds the production response bound.
#[must_use]
pub fn apply_grammar_candidate_json(
    validated: &str,
    version: &str,
    baseline: &FormattingBaseline,
    candidate_json: &[u8],
    options: GrammarSafetyOptions<'_>,
) -> GrammarSafetyResult {
    if baseline.base_version() != version
        || baseline.base_fingerprint() != text_sha256_fingerprint(validated)
    {
        return identity_failure(
            validated,
            GrammarErrorCode::FormattingIdentity,
            "baseline identity differs from Validated Transcript",
        );
    }
    if !baseline.verify_derivation_digest() {
        return identity_failure(
            validated,
            GrammarErrorCode::FormattingDerivation,
            "baseline derivation digest is invalid",
        );
    }

    if validated.len() > MAX_VALIDATED_TRANSCRIPT_UTF8_BYTES {
        return baseline_failure(
            validated,
            baseline,
            GrammarErrorCode::Oversize,
            "Validated Transcript exceeds grammar bound",
        );
    }
    if candidate_json.len() > MAX_GRAMMAR_RESPONSE_BYTES {
        return baseline_failure(
            validated,
            baseline,
            GrammarErrorCode::Oversize,
            "grammar response exceeds raw body bound",
        );
    }

    let value = match serde_json::from_slice::<UniqueValue>(candidate_json) {
        Ok(value) => value.0,
        Err(_) => {
            return baseline_failure(
                validated,
                baseline,
                GrammarErrorCode::Malformed,
                "grammar response is not valid bounded JSON",
            );
        }
    };
    let (depth, nodes) = json_shape(&value);
    if depth > MAX_GRAMMAR_JSON_DEPTH || nodes > MAX_GRAMMAR_JSON_NODES {
        return baseline_failure(
            validated,
            baseline,
            GrammarErrorCode::Oversize,
            "grammar JSON exceeds decoded structure bound",
        );
    }

    let Some((base_version, base_fingerprint, raw_edits)) = parse_envelope(&value) else {
        return baseline_failure(
            validated,
            baseline,
            GrammarErrorCode::Malformed,
            "grammar candidate envelope is malformed",
        );
    };
    if base_version != version || base_fingerprint != baseline.base_fingerprint() {
        return baseline_failure(
            validated,
            baseline,
            GrammarErrorCode::StaleGrammar,
            "grammar candidate identity is stale",
        );
    }

    let candidate = match parse_edits(base_version, base_fingerprint, raw_edits) {
        Ok(candidate) => candidate,
        Err((code, message)) => {
            return baseline_failure(validated, baseline, code, message);
        }
    };
    validate_and_compose(validated, baseline, &candidate, options)
}

struct UniqueValue(Value);

impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(UniqueValueVisitor).map(Self)
    }
}

struct UniqueValueVisitor;

impl<'de> Visitor<'de> for UniqueValueVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(Value::String(value))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element::<UniqueValue>()? {
            values.push(value.0);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = Map::new();
        while let Some(key) = object.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(de::Error::custom("duplicate JSON object key"));
            }
            let value = object.next_value::<UniqueValue>()?;
            values.insert(key, value.0);
        }
        Ok(Value::Object(values))
    }
}

fn parse_envelope(value: &Value) -> Option<(&str, &str, &[Value])> {
    let object = value.as_object()?;
    if !has_exact_keys(object, &["base_version", "base_fingerprint", "edits"]) {
        return None;
    }
    let version = object.get("base_version")?.as_str()?;
    let fingerprint = object.get("base_fingerprint")?.as_str()?;
    let edits = object.get("edits")?.as_array()?;
    if version.is_empty() || !is_text_sha256_fingerprint(fingerprint) {
        return None;
    }
    Some((version, fingerprint, edits))
}

fn parse_edits(
    base_version: &str,
    base_fingerprint: &str,
    raw_edits: &[Value],
) -> Result<GrammarCandidate, (GrammarErrorCode, &'static str)> {
    if raw_edits.len() > MAX_GRAMMAR_EDITS {
        return Err((
            GrammarErrorCode::Oversize,
            "grammar edit count exceeds bound",
        ));
    }
    let mut edits = Vec::with_capacity(raw_edits.len());
    for raw in raw_edits {
        let Some(object) = raw.as_object() else {
            return Err((GrammarErrorCode::Malformed, "grammar edit is not an object"));
        };
        if !has_exact_keys(
            object,
            &["id", "rule_id", "start_utf8", "end_utf8", "before", "after"],
        ) {
            return Err((
                GrammarErrorCode::Malformed,
                "grammar edit keys are not exact",
            ));
        }
        let (Some(id), Some(rule_id), Some(before), Some(after)) = (
            object.get("id").and_then(Value::as_str),
            object.get("rule_id").and_then(Value::as_str),
            object.get("before").and_then(Value::as_str),
            object.get("after").and_then(Value::as_str),
        ) else {
            return Err((
                GrammarErrorCode::Malformed,
                "grammar edit string field is invalid",
            ));
        };
        if id.is_empty() || rule_id.is_empty() {
            return Err((
                GrammarErrorCode::Malformed,
                "grammar edit identifier is empty",
            ));
        }
        if [id, rule_id, before, after]
            .iter()
            .any(|field| field.len() > MAX_GRAMMAR_EDIT_FIELD_UTF8_BYTES)
        {
            return Err((
                GrammarErrorCode::Oversize,
                "grammar edit field exceeds bound",
            ));
        }
        let (Some(start), Some(end)) = (
            object.get("start_utf8").and_then(Value::as_u64),
            object.get("end_utf8").and_then(Value::as_u64),
        ) else {
            return Err((
                GrammarErrorCode::Malformed,
                "grammar edit offset is invalid",
            ));
        };
        let (Ok(start_utf8), Ok(end_utf8)) = (usize::try_from(start), usize::try_from(end)) else {
            return Err((
                GrammarErrorCode::Oversize,
                "grammar edit offset exceeds platform bound",
            ));
        };
        edits.push(GrammarEdit {
            id: id.to_owned(),
            rule_id: rule_id.to_owned(),
            start_utf8,
            end_utf8,
            before: before.to_owned(),
            after: after.to_owned(),
        });
    }

    if !edits.windows(2).all(|pair| {
        (pair[0].start_utf8, pair[0].end_utf8) <= (pair[1].start_utf8, pair[1].end_utf8)
    }) {
        return Err((
            GrammarErrorCode::Unsorted,
            "grammar edits are not source ordered",
        ));
    }
    Ok(GrammarCandidate {
        base_version: base_version.to_owned(),
        base_fingerprint: base_fingerprint.to_owned(),
        edits,
    })
}

fn validate_and_compose(
    validated: &str,
    baseline: &FormattingBaseline,
    candidate: &GrammarCandidate,
    options: GrammarSafetyOptions<'_>,
) -> GrammarSafetyResult {
    debug_assert_eq!(candidate.base_version, baseline.base_version());
    debug_assert_eq!(candidate.base_fingerprint, baseline.base_fingerprint());
    let tokens = word_tokens(validated);
    let protected = protected_ranges(validated, baseline, options, &tokens);
    let mut diagnostics = Vec::new();
    let mut seen = BTreeSet::new();
    let mut accepted: Vec<(&GrammarEdit, SourceAnchor)> = Vec::new();

    for edit in &candidate.edits {
        if edit.start_utf8 > edit.end_utf8 || edit.end_utf8 > validated.len() {
            push_diagnostic(
                &mut diagnostics,
                &mut seen,
                GrammarErrorCode::SpanOutOfBounds,
                "grammar range is outside Validated Transcript",
                &edit.id,
            );
            continue;
        }
        if !validated.is_char_boundary(edit.start_utf8)
            || !validated.is_char_boundary(edit.end_utf8)
        {
            push_diagnostic(
                &mut diagnostics,
                &mut seen,
                GrammarErrorCode::SpanNotCharBoundary,
                "grammar range cuts a UTF-8 scalar",
                &edit.id,
            );
            continue;
        }

        let token_index = tokens
            .iter()
            .position(|token| token.start == edit.start_utf8 && token.end == edit.end_utf8);
        if token_index.is_none() || edit.start_utf8 == edit.end_utf8 {
            push_diagnostic(
                &mut diagnostics,
                &mut seen,
                GrammarErrorCode::NotTokenBoundary,
                "grammar edit must replace exactly one lexical token",
                &edit.id,
            );
        }
        let anchored = validated.get(edit.start_utf8..edit.end_utf8) == Some(edit.before.as_str());
        if !anchored {
            push_diagnostic(
                &mut diagnostics,
                &mut seen,
                GrammarErrorCode::AnchorMismatch,
                "grammar before field does not match source range",
                &edit.id,
            );
        }
        if edit.start_utf8 < edit.end_utf8
            && protected
                .iter()
                .any(|range| overlaps((edit.start_utf8, edit.end_utf8), *range))
        {
            push_diagnostic(
                &mut diagnostics,
                &mut seen,
                GrammarErrorCode::ProtectedSpan,
                "grammar edit intersects protected content",
                &edit.id,
            );
        }

        let known_rule = is_known_rule(&edit.rule_id);
        let rule_ok = token_index.is_some() && known_rule && rule_matches(validated, &tokens, edit);
        if !known_rule {
            push_diagnostic(
                &mut diagnostics,
                &mut seen,
                GrammarErrorCode::UnknownRule,
                "grammar rule is not in the closed catalog",
                &edit.id,
            );
        } else if !rule_ok {
            push_diagnostic(
                &mut diagnostics,
                &mut seen,
                GrammarErrorCode::RuleContext,
                "grammar rule context predicate failed",
                &edit.id,
            );
        }

        let anchor = baseline.anchor_for_source(SourceSpan::new(edit.start_utf8, edit.end_utf8));
        if token_index.is_some() && anchored && rule_ok && anchor.is_none() {
            push_diagnostic(
                &mut diagnostics,
                &mut seen,
                GrammarErrorCode::Unmappable,
                "formatter emitted no source anchor",
                &edit.id,
            );
        }
        if token_index.is_some() && anchored && rule_ok {
            if let Some(anchor) = anchor {
                let mapped = baseline
                    .rendered()
                    .get(anchor.rendered_start..anchor.rendered_end);
                if mapped.is_none_or(|text| !text.eq_ignore_ascii_case(&edit.before)) {
                    push_diagnostic(
                        &mut diagnostics,
                        &mut seen,
                        GrammarErrorCode::Unmappable,
                        "formatter anchor no longer names the source token",
                        &edit.id,
                    );
                } else {
                    accepted.push((edit, anchor));
                }
            }
        }
    }

    for (index, left) in candidate.edits.iter().enumerate() {
        for right in &candidate.edits[index + 1..] {
            if overlaps(
                (left.start_utf8, left.end_utf8),
                (right.start_utf8, right.end_utf8),
            ) {
                push_diagnostic(
                    &mut diagnostics,
                    &mut seen,
                    GrammarErrorCode::Overlap,
                    "grammar ranges overlap or duplicate",
                    &right.id,
                );
            }
        }
    }
    for (index, (_, left)) in accepted.iter().enumerate() {
        for (_, right) in &accepted[index + 1..] {
            if overlaps(
                (left.rendered_start, left.rendered_end),
                (right.rendered_start, right.rendered_end),
            ) {
                push_diagnostic(
                    &mut diagnostics,
                    &mut seen,
                    GrammarErrorCode::Unmappable,
                    "formatter anchors overlap",
                    "",
                );
            }
        }
    }

    if !diagnostics.is_empty() || accepted.is_empty() {
        return result(validated, baseline.rendered(), false, diagnostics);
    }

    let mut rendered = baseline.rendered().to_owned();
    let mut replacements: Vec<(usize, usize, String)> = accepted
        .into_iter()
        .map(|(edit, anchor)| {
            let existing = &rendered[anchor.rendered_start..anchor.rendered_end];
            (
                anchor.rendered_start,
                anchor.rendered_end,
                preserve_formatter_case(existing, &edit.after),
            )
        })
        .collect();
    replacements.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
    for (start, end, replacement) in replacements {
        rendered.replace_range(start..end, &replacement);
    }
    result(validated, &rendered, true, Vec::new())
}

fn is_known_rule(rule: &str) -> bool {
    matches!(
        rule,
        "G_THERE_IS_PLURAL_QUANTITY" | "G_LETS_MEET_CONTRACTION" | "G_DIDNT_APOSTROPHE"
    )
}

fn rule_matches(base: &str, tokens: &[Token<'_>], edit: &GrammarEdit) -> bool {
    let Some(index) = tokens
        .iter()
        .position(|token| token.start == edit.start_utf8 && token.end == edit.end_utf8)
    else {
        return false;
    };
    match edit.rule_id.as_str() {
        "G_THERE_IS_PLURAL_QUANTITY" => {
            if edit.before != "is"
                || edit.after != "are"
                || index == 0
                || index + 2 >= tokens.len()
                || parse_formatting_commands(base).has_command_span()
                || tokens.windows(2).any(|pair| {
                    pair[0].text.eq_ignore_ascii_case("new")
                        && pair[1].text.eq_ignore_ascii_case("line")
                })
            {
                return false;
            }
            let previous = tokens[index - 1];
            let quantity = tokens[index + 1];
            let noun = tokens[index + 2];
            previous.text.eq_ignore_ascii_case("there")
                && noun.text.eq_ignore_ascii_case("issues")
                && is_plural_quantity(quantity.text)
                && ascii_spaces(base, previous.end, tokens[index].start)
                && ascii_spaces(base, tokens[index].end, quantity.start)
                && ascii_spaces(base, quantity.end, noun.start)
        }
        "G_LETS_MEET_CONTRACTION" => {
            edit.before == "lets"
                && edit.after == "let's"
                && index == 0
                && base[..edit.start_utf8].bytes().all(|byte| byte == b' ')
                && tokens
                    .get(1)
                    .is_some_and(|token| token.text.eq_ignore_ascii_case("meet"))
                && ascii_spaces(base, tokens[0].end, tokens[1].start)
                && !parse_formatting_commands(base).has_command_span()
        }
        "G_DIDNT_APOSTROPHE" => edit.before == "didnt" && edit.after == "didn't",
        _ => false,
    }
}

fn is_plural_quantity(text: &str) -> bool {
    matches!(
        text.to_ascii_lowercase().as_str(),
        "two"
            | "three"
            | "four"
            | "five"
            | "six"
            | "seven"
            | "eight"
            | "nine"
            | "ten"
            | "eleven"
            | "twelve"
            | "2"
            | "3"
            | "4"
            | "5"
            | "6"
            | "7"
            | "8"
            | "9"
            | "10"
            | "11"
            | "12"
    )
}

fn ascii_spaces(text: &str, start: usize, end: usize) -> bool {
    start < end && text[start..end].bytes().all(|byte| byte == b' ')
}

fn protected_ranges(
    text: &str,
    baseline: &FormattingBaseline,
    options: GrammarSafetyOptions<'_>,
    tokens: &[Token<'_>],
) -> Vec<(usize, usize)> {
    let mut ranges: Vec<(usize, usize)> = baseline
        .protected_source_ranges()
        .iter()
        .map(|range| (range.start, range.end))
        .collect();

    for token in tokens {
        let lower = token.text.to_lowercase();
        if options
            .dictionary_terms
            .iter()
            .chain(options.protected_names.iter())
            .any(|term| term.to_lowercase() == lower)
            || matches!(lower.as_str(), "not" | "no" | "never")
            || lower.ends_with("n't")
            || lower.ends_with("n’t")
        {
            ranges.push((token.start, token.end));
        }
    }
    for phrase in options
        .dictionary_terms
        .iter()
        .chain(options.protected_names.iter())
    {
        add_ascii_phrase_ranges(text, phrase, &mut ranges);
    }
    add_email_and_identifier_ranges(text, &mut ranges);
    let lower = text.to_ascii_lowercase();
    if PROMPT_MARKERS.iter().any(|marker| lower.contains(marker)) {
        ranges.push((0, text.len()));
    }
    normalize_ranges(ranges)
}

fn add_ascii_phrase_ranges(text: &str, phrase: &str, ranges: &mut Vec<(usize, usize)>) {
    if phrase.is_empty() || !phrase.is_ascii() {
        return;
    }
    let lower = text.to_ascii_lowercase();
    let needle = phrase.to_ascii_lowercase();
    let mut search = 0usize;
    while let Some(relative) = lower[search..].find(&needle) {
        let start = search + relative;
        let end = start + needle.len();
        let before_word = start > 0
            && text[..start]
                .chars()
                .next_back()
                .is_some_and(|ch| ch.is_alphanumeric() || ch == '_');
        let after_word = text[end..]
            .chars()
            .next()
            .is_some_and(|ch| ch.is_alphanumeric() || ch == '_');
        if !before_word && !after_word {
            ranges.push((start, end));
        }
        search = end.max(start + 1);
    }
}

fn add_email_and_identifier_ranges(text: &str, ranges: &mut Vec<(usize, usize)>) {
    let mut start = 0usize;
    for segment in text.split_inclusive(char::is_whitespace) {
        let raw = segment.trim_end_matches(char::is_whitespace);
        let leading = raw.len() - raw.trim_start_matches(['(', '[', '{', '"', '\'']).len();
        let token = raw[leading..].trim_end_matches(['.', ',', ';', ':', ')', ']', '}', '"', '\'']);
        let token_start = start + leading;
        let token_end = token_start + token.len();
        let email = token
            .split_once('@')
            .is_some_and(|(left, right)| !left.is_empty() && right.contains('.'));
        let identifier = token.contains('_')
            || token
                .chars()
                .zip(token.chars().skip(1))
                .any(|(left, right)| left.is_ascii_lowercase() && right.is_ascii_uppercase())
            || (token.len() >= 2
                && token.chars().any(|ch| ch.is_ascii_alphabetic())
                && token
                    .chars()
                    .filter(|ch| ch.is_ascii_alphabetic())
                    .all(|ch| ch.is_ascii_uppercase()))
            || token
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
                && token.contains(['-', '.']);
        if token_start < token_end && (email || identifier) {
            ranges.push((token_start, token_end));
        }
        start += segment.len();
    }
}

fn word_tokens(text: &str) -> Vec<Token<'_>> {
    let mut tokens = Vec::new();
    let mut index = 0usize;
    while index < text.len() {
        let Some(ch) = text[index..].chars().next() else {
            break;
        };
        if !ch.is_alphanumeric() {
            index += ch.len_utf8();
            continue;
        }
        let start = index;
        index += ch.len_utf8();
        while index < text.len() {
            let Some(next) = text[index..].chars().next() else {
                break;
            };
            if next.is_alphanumeric() {
                index += next.len_utf8();
                continue;
            }
            if matches!(next, '\'' | '’')
                && text[index + next.len_utf8()..]
                    .chars()
                    .next()
                    .is_some_and(char::is_alphanumeric)
            {
                index += next.len_utf8();
                continue;
            }
            break;
        }
        tokens.push(Token {
            start,
            end: index,
            text: &text[start..index],
        });
    }
    tokens
}

fn preserve_formatter_case(existing: &str, replacement: &str) -> String {
    if existing.chars().next().is_some_and(char::is_uppercase)
        && replacement.chars().next().is_some_and(char::is_lowercase)
    {
        let mut chars = replacement.chars();
        if let Some(first) = chars.next() {
            return first.to_uppercase().collect::<String>() + chars.as_str();
        }
    }
    replacement.to_owned()
}

fn result(
    validated: &str,
    rendered: &str,
    grammar_applied: bool,
    diagnostics: Vec<GrammarDiagnostic>,
) -> GrammarSafetyResult {
    let formatting_applied = rendered != validated;
    let outcome = match (grammar_applied, formatting_applied) {
        (true, true) => GrammarOutcome::Both,
        (true, false) => GrammarOutcome::GrammarOnly,
        (false, true) => GrammarOutcome::FormattingOnly,
        (false, false) => GrammarOutcome::Unchanged,
    };
    GrammarSafetyResult {
        outcome,
        rendered: rendered.to_owned(),
        diagnostics,
    }
}

fn identity_failure(
    validated: &str,
    code: GrammarErrorCode,
    message: &'static str,
) -> GrammarSafetyResult {
    result(
        validated,
        validated,
        false,
        vec![diagnostic(code, message, "")],
    )
}

fn baseline_failure(
    validated: &str,
    baseline: &FormattingBaseline,
    code: GrammarErrorCode,
    message: &'static str,
) -> GrammarSafetyResult {
    result(
        validated,
        baseline.rendered(),
        false,
        vec![diagnostic(code, message, "")],
    )
}

fn push_diagnostic(
    diagnostics: &mut Vec<GrammarDiagnostic>,
    seen: &mut BTreeSet<GrammarErrorCode>,
    code: GrammarErrorCode,
    message: &'static str,
    edit_id: &str,
) {
    if seen.insert(code) {
        diagnostics.push(diagnostic(code, message, edit_id));
    }
}

fn diagnostic(code: GrammarErrorCode, message: &'static str, edit_id: &str) -> GrammarDiagnostic {
    GrammarDiagnostic {
        code,
        message: clamp_utf8(message, MAX_GRAMMAR_DIAGNOSTIC_UTF8_BYTES),
        edit_id: clamp_utf8(
            &scrub_untrusted_diagnostic(edit_id),
            MAX_GRAMMAR_DIAGNOSTIC_UTF8_BYTES,
        ),
    }
}

fn scrub_untrusted_diagnostic(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    if lower.contains("gsk_")
        || lower.contains("bearer ")
        || lower.contains("api_key")
        || lower.contains("api-key")
    {
        "[REDACTED]".to_owned()
    } else {
        text.to_owned()
    }
}

fn clamp_utf8(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut boundary = max;
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    text[..boundary].to_owned()
}

fn has_exact_keys(object: &Map<String, Value>, expected: &[&str]) -> bool {
    object.len() == expected.len() && expected.iter().all(|key| object.contains_key(*key))
}

fn json_shape(value: &Value) -> (usize, usize) {
    match value {
        Value::Array(values) => {
            let mut depth: usize = 1;
            let mut nodes: usize = 1;
            for child in values {
                let (child_depth, child_nodes) = json_shape(child);
                depth = depth.max(child_depth.saturating_add(1));
                nodes = nodes.saturating_add(child_nodes);
            }
            (depth, nodes)
        }
        Value::Object(values) => {
            let mut depth: usize = 1;
            let mut nodes: usize = 1;
            for child in values.values() {
                let (child_depth, child_nodes) = json_shape(child);
                depth = depth.max(child_depth.saturating_add(1));
                nodes = nodes.saturating_add(child_nodes);
            }
            (depth, nodes)
        }
        _ => (1, 1),
    }
}

fn overlaps(left: (usize, usize), right: (usize, usize)) -> bool {
    left.0 < right.1 && right.0 < left.1
}

fn normalize_ranges(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    ranges.retain(|(start, end)| start < end);
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in ranges {
        if let Some(last) = merged.last_mut() {
            if start <= last.1 {
                last.1 = last.1.max(end);
                continue;
            }
        }
        merged.push((start, end));
    }
    merged
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;
    use serde_json::json;

    use super::*;
    use crate::{FormatOptions, format_validated_for_grammar};

    const SAFETY_CORPUS: &str =
        include_str!("../../../docs/research/smart-writing-edit-safety-corpus-2026-08-09.json");
    const SPEC_CONSTANTS: &str =
        include_str!("../../../docs/research/smart-writing-spec-constants-2026-08-09.json");

    #[derive(Deserialize)]
    struct Corpus {
        fixtures: Vec<Fixture>,
    }

    #[derive(Deserialize)]
    struct Fixture {
        id: String,
        base: Base,
        grammar_candidate: Value,
        protected_names: Vec<String>,
        dictionary_terms: Vec<String>,
        expected: Expected,
    }

    #[derive(Deserialize)]
    struct Base {
        text: String,
        version: String,
    }

    #[derive(Deserialize)]
    struct Expected {
        decision: String,
        rendered: String,
        error_codes: Vec<String>,
    }

    fn baseline(text: &str, dictionary: &[&str], names: &[&str]) -> FormattingBaseline {
        format_validated_for_grammar(
            text,
            FormatOptions {
                dictionary,
                protected_names: names,
                ..FormatOptions::default()
            },
        )
    }

    fn candidate(text: &str, edits: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "base_version": "validated-en-v1",
            "base_fingerprint": text_sha256_fingerprint(text),
            "edits": edits,
        }))
        .unwrap()
    }

    fn edit(text: &str, needle: &str, rule: &str, after: &str) -> Value {
        let start = text.find(needle).unwrap();
        json!({
            "id": "g1",
            "rule_id": rule,
            "start_utf8": start,
            "end_utf8": start + needle.len(),
            "before": needle,
            "after": after,
        })
    }

    fn apply_value(
        text: &str,
        value: Value,
        dictionary: &[&str],
        names: &[&str],
    ) -> GrammarSafetyResult {
        let baseline = baseline(text, dictionary, names);
        apply_grammar_candidate_json(
            text,
            "validated-en-v1",
            &baseline,
            &serde_json::to_vec(&value).unwrap(),
            GrammarSafetyOptions {
                dictionary_terms: dictionary,
                protected_names: names,
            },
        )
    }

    #[test]
    fn corpus_all_18_decisions_rendered_text_and_error_order_are_exact() {
        let corpus: Corpus = serde_json::from_str(SAFETY_CORPUS).unwrap();
        assert_eq!(corpus.fixtures.len(), 18);
        let mut failures = Vec::new();
        for fixture in corpus.fixtures {
            let dictionary: Vec<&str> = fixture
                .dictionary_terms
                .iter()
                .map(String::as_str)
                .collect();
            let names: Vec<&str> = fixture.protected_names.iter().map(String::as_str).collect();
            let baseline = baseline(&fixture.base.text, &dictionary, &names);
            let raw = serde_json::to_vec(&fixture.grammar_candidate).unwrap();
            let actual = apply_grammar_candidate_json(
                &fixture.base.text,
                &fixture.base.version,
                &baseline,
                &raw,
                GrammarSafetyOptions {
                    dictionary_terms: &dictionary,
                    protected_names: &names,
                },
            );
            let codes: Vec<String> = actual
                .error_codes()
                .into_iter()
                .map(str::to_owned)
                .collect();
            if actual.outcome.as_str() != fixture.expected.decision
                || actual.rendered != fixture.expected.rendered
                || codes != fixture.expected.error_codes
            {
                failures.push(format!(
                    "{}: got ({}, {:?}, {:?}); expected ({}, {:?}, {:?})",
                    fixture.id,
                    actual.outcome.as_str(),
                    actual.rendered,
                    codes,
                    fixture.expected.decision,
                    fixture.expected.rendered,
                    fixture.expected.error_codes,
                ));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn production_limits_and_release_counts_match_the_spec_manifest() {
        let manifest: Value = serde_json::from_str(SPEC_CONSTANTS).unwrap();
        let limits = &manifest["limits"];
        assert_eq!(
            limits["MAX_GRAMMAR_RESPONSE_BYTES"],
            MAX_GRAMMAR_RESPONSE_BYTES
        );
        assert_eq!(limits["MAX_GRAMMAR_JSON_DEPTH"], MAX_GRAMMAR_JSON_DEPTH);
        assert_eq!(limits["MAX_GRAMMAR_JSON_NODES"], MAX_GRAMMAR_JSON_NODES);
        assert_eq!(limits["MAX_GRAMMAR_EDITS"], MAX_GRAMMAR_EDITS);
        assert_eq!(
            limits["MAX_GRAMMAR_EDIT_FIELD_UTF8_BYTES"],
            MAX_GRAMMAR_EDIT_FIELD_UTF8_BYTES
        );
        assert_eq!(
            limits["MAX_GRAMMAR_DIAGNOSTIC_UTF8_BYTES"],
            MAX_GRAMMAR_DIAGNOSTIC_UTF8_BYTES
        );
        let release = &manifest["release_thresholds"];
        assert_eq!(release["SAFETY_FIXTURES_REQUIRED"], 18);
        assert_eq!(release["SAFETY_FIXTURES_EXACT_PASS_REQUIRED"], 18);
        assert_eq!(release["ADVERSARIAL_CASES_REQUIRED"], 56);
        assert_eq!(release["DETERMINISM_HASH_SEEDS_REQUIRED"], 3);
    }

    #[test]
    fn raw_and_decoded_release_bounds_fail_closed() {
        let text = "lets meet tomorrow";
        let baseline = baseline(text, &[], &[]);
        let oversized = vec![b' '; MAX_GRAMMAR_RESPONSE_BYTES + 1];
        let result = apply_grammar_candidate_json(
            text,
            "validated-en-v1",
            &baseline,
            &oversized,
            GrammarSafetyOptions::default(),
        );
        assert_eq!(result.error_codes(), ["E_OVERSIZE"]);

        let deep = format!(
            "{{\"base_version\":\"validated-en-v1\",\"base_fingerprint\":\"{}\",\"edits\":[{}]}}",
            text_sha256_fingerprint(text),
            "[".repeat(MAX_GRAMMAR_JSON_DEPTH) + &"]".repeat(MAX_GRAMMAR_JSON_DEPTH)
        );
        let result = apply_grammar_candidate_json(
            text,
            "validated-en-v1",
            &baseline,
            deep.as_bytes(),
            GrammarSafetyOptions::default(),
        );
        assert_eq!(result.error_codes(), ["E_OVERSIZE"]);
    }

    #[test]
    fn closed_rules_reject_tabs_commands_and_false_contexts() {
        let cases = [
            (
                "there\tis two issues",
                6,
                8,
                "G_THERE_IS_PLURAL_QUANTITY",
                "is",
                "are",
            ),
            (
                "there is two issues command period",
                6,
                8,
                "G_THERE_IS_PLURAL_QUANTITY",
                "is",
                "are",
            ),
            (
                "the app lets users export",
                8,
                12,
                "G_LETS_MEET_CONTRACTION",
                "lets",
                "let's",
            ),
        ];
        for (text, start, end, rule, before, after) in cases {
            let baseline = baseline(text, &[], &[]);
            let raw = candidate(
                text,
                json!([{"id":"g1","rule_id":rule,"start_utf8":start,"end_utf8":end,"before":before,"after":after}]),
            );
            let result = apply_grammar_candidate_json(
                text,
                "validated-en-v1",
                &baseline,
                &raw,
                GrammarSafetyOptions::default(),
            );
            assert!(result.error_codes().contains(&"E_RULE_CONTEXT"), "{text:?}");
            assert_eq!(result.rendered, baseline.rendered());
        }
    }

    #[test]
    fn provider_json_has_no_baseline_or_whole_render_authority() {
        let text = "do not transfer money";
        let baseline = baseline(text, &[], &[]);
        let raw = serde_json::to_vec(&json!({
            "base_version": "validated-en-v1",
            "base_fingerprint": text_sha256_fingerprint(text),
            "edits": [],
            "rendered": "transfer money now",
        }))
        .unwrap();
        let result = apply_grammar_candidate_json(
            text,
            "validated-en-v1",
            &baseline,
            &raw,
            GrammarSafetyOptions::default(),
        );
        assert_eq!(result.error_codes(), ["E_MALFORMED"]);
        assert_eq!(result.rendered, baseline.rendered());

        let duplicate = format!(
            "{{\"base_version\":\"validated-en-v1\",\"base_version\":\"validated-en-v1\",\"base_fingerprint\":\"{}\",\"edits\":[]}}",
            text_sha256_fingerprint(text)
        );
        let result = apply_grammar_candidate_json(
            text,
            "validated-en-v1",
            &baseline,
            duplicate.as_bytes(),
            GrammarSafetyOptions::default(),
        );
        assert_eq!(result.error_codes(), ["E_MALFORMED"]);
    }

    #[test]
    fn all_release_limits_are_exact_at_and_over_the_boundary() {
        let text = "i didnt work";
        let base_capability = baseline(text, &[], &[]);
        let valid = candidate(
            text,
            json!([edit(text, "didnt", "G_DIDNT_APOSTROPHE", "didn't")]),
        );

        let mut exact_raw = valid.clone();
        exact_raw.resize(MAX_GRAMMAR_RESPONSE_BYTES, b' ');
        let exact = apply_grammar_candidate_json(
            text,
            "validated-en-v1",
            &base_capability,
            &exact_raw,
            GrammarSafetyOptions::default(),
        );
        assert!(exact.diagnostics.is_empty());
        exact_raw.push(b' ');
        let over = apply_grammar_candidate_json(
            text,
            "validated-en-v1",
            &base_capability,
            &exact_raw,
            GrammarSafetyOptions::default(),
        );
        assert_eq!(over.error_codes(), ["E_OVERSIZE"]);

        let field_exact = "x".repeat(MAX_GRAMMAR_EDIT_FIELD_UTF8_BYTES);
        let field_over = "x".repeat(MAX_GRAMMAR_EDIT_FIELD_UTF8_BYTES + 1);
        for (field, expected) in [(field_exact, None), (field_over, Some("E_OVERSIZE"))] {
            let raw = candidate(
                text,
                json!([{
                    "id": field,
                    "rule_id": "G_DIDNT_APOSTROPHE",
                    "start_utf8": 2,
                    "end_utf8": 7,
                    "before": "didnt",
                    "after": "didn't",
                }]),
            );
            let result = apply_grammar_candidate_json(
                text,
                "validated-en-v1",
                &base_capability,
                &raw,
                GrammarSafetyOptions::default(),
            );
            assert_eq!(result.error_codes().first().copied(), expected);
        }

        let one_edit = edit(text, "didnt", "G_DIDNT_APOSTROPHE", "didn't");
        for (count, expected) in [
            (MAX_GRAMMAR_EDITS, Some("E_OVERLAP")),
            (MAX_GRAMMAR_EDITS + 1, Some("E_OVERSIZE")),
        ] {
            let raw = candidate(text, Value::Array(vec![one_edit.clone(); count]));
            let result = apply_grammar_candidate_json(
                text,
                "validated-en-v1",
                &base_capability,
                &raw,
                GrammarSafetyOptions::default(),
            );
            assert_eq!(result.error_codes().first().copied(), expected);
        }

        let at_input = "x".repeat(MAX_VALIDATED_TRANSCRIPT_UTF8_BYTES);
        let at_baseline = baseline(&at_input, &[], &[]);
        let at = apply_grammar_candidate_json(
            &at_input,
            "validated-en-v1",
            &at_baseline,
            &candidate(&at_input, json!([])),
            GrammarSafetyOptions::default(),
        );
        assert!(at.diagnostics.is_empty());
        let over_input = "x".repeat(MAX_VALIDATED_TRANSCRIPT_UTF8_BYTES + 1);
        let over_baseline = baseline(&over_input, &[], &[]);
        let over = apply_grammar_candidate_json(
            &over_input,
            "validated-en-v1",
            &over_baseline,
            &candidate(&over_input, json!([])),
            GrammarSafetyOptions::default(),
        );
        assert_eq!(over.error_codes(), ["E_OVERSIZE"]);

        let many_nodes = json!({
            "base_version": "validated-en-v1",
            "base_fingerprint": text_sha256_fingerprint(text),
            "edits": vec![Value::Null; MAX_GRAMMAR_JSON_NODES],
        });
        let result = apply_value(text, many_nodes, &[], &[]);
        assert_eq!(result.error_codes(), ["E_OVERSIZE"]);

        let depth_at =
            (0..MAX_GRAMMAR_JSON_DEPTH - 1).fold(Value::Null, |value, _| Value::Array(vec![value]));
        assert_eq!(json_shape(&depth_at).0, MAX_GRAMMAR_JSON_DEPTH);
        let depth_over = Value::Array(vec![depth_at]);
        assert_eq!(json_shape(&depth_over).0, MAX_GRAMMAR_JSON_DEPTH + 1);
        let nodes_at = Value::Array(vec![Value::Null; MAX_GRAMMAR_JSON_NODES - 1]);
        let nodes_over = Value::Array(vec![Value::Null; MAX_GRAMMAR_JSON_NODES]);
        assert_eq!(json_shape(&nodes_at).1, MAX_GRAMMAR_JSON_NODES);
        assert_eq!(json_shape(&nodes_over).1, MAX_GRAMMAR_JSON_NODES + 1);
    }

    fn adversary_results(seed: usize) -> Vec<(&'static str, bool)> {
        let mut cases = Vec::new();
        let text = "i didnt work";
        let baseline = baseline(text, &[], &[]);
        let fp = text_sha256_fingerprint(text);
        let valid_edit = edit(text, "didnt", "G_DIDNT_APOSTROPHE", "didn't");

        let mut record = |name, condition| cases.push((name, condition));
        let raw_result = |raw: &[u8]| {
            apply_grammar_candidate_json(
                text,
                "validated-en-v1",
                &baseline,
                raw,
                GrammarSafetyOptions::default(),
            )
        };

        record(
            "A01_INVALID_JSON",
            raw_result(b"{").error_codes() == ["E_MALFORMED"],
        );
        record(
            "A02_TOP_ARRAY",
            apply_value(text, json!([]), &[], &[]).error_codes() == ["E_MALFORMED"],
        );
        record("A03_EXTRA_TOP", apply_value(text, json!({"base_version":"validated-en-v1","base_fingerprint":fp,"edits":[],"rendered":"owned"}), &[], &[]).error_codes() == ["E_MALFORMED"]);
        record(
            "A04_MISSING_TOP",
            apply_value(
                text,
                json!({"base_version":"validated-en-v1","base_fingerprint":fp}),
                &[],
                &[],
            )
            .error_codes()
                == ["E_MALFORMED"],
        );
        record(
            "A05_EMPTY_VERSION",
            apply_value(
                text,
                json!({"base_version":"","base_fingerprint":fp,"edits":[]}),
                &[],
                &[],
            )
            .error_codes()
                == ["E_MALFORMED"],
        );
        record("A06_BAD_FINGERPRINT", apply_value(text, json!({"base_version":"validated-en-v1","base_fingerprint":"sha256:ABC","edits":[]}), &[], &[]).error_codes() == ["E_MALFORMED"]);
        record(
            "A07_EDITS_NOT_ARRAY",
            apply_value(
                text,
                json!({"base_version":"validated-en-v1","base_fingerprint":fp,"edits":{}}),
                &[],
                &[],
            )
            .error_codes()
                == ["E_MALFORMED"],
        );
        record(
            "A08_EDIT_NOT_OBJECT",
            apply_value(
                text,
                json!({"base_version":"validated-en-v1","base_fingerprint":fp,"edits":[null]}),
                &[],
                &[],
            )
            .error_codes()
                == ["E_MALFORMED"],
        );
        record("A09_EDIT_EXTRA_KEY", apply_value(text, json!({"base_version":"validated-en-v1","base_fingerprint":fp,"edits":[{"id":"g","rule_id":"G_DIDNT_APOSTROPHE","start_utf8":2,"end_utf8":7,"before":"didnt","after":"didn't","rendered":"x"}]}), &[], &[]).error_codes() == ["E_MALFORMED"]);
        record("A10_EDIT_MISSING_KEY", apply_value(text, json!({"base_version":"validated-en-v1","base_fingerprint":fp,"edits":[{"id":"g"}]}), &[], &[]).error_codes() == ["E_MALFORMED"]);
        record("A11_ID_WRONG_TYPE", apply_value(text, json!({"base_version":"validated-en-v1","base_fingerprint":fp,"edits":[{"id":1,"rule_id":"G_DIDNT_APOSTROPHE","start_utf8":2,"end_utf8":7,"before":"didnt","after":"didn't"}]}), &[], &[]).error_codes() == ["E_MALFORMED"]);
        record("A12_EMPTY_ID", apply_value(text, json!({"base_version":"validated-en-v1","base_fingerprint":fp,"edits":[{"id":"","rule_id":"G_DIDNT_APOSTROPHE","start_utf8":2,"end_utf8":7,"before":"didnt","after":"didn't"}]}), &[], &[]).error_codes() == ["E_MALFORMED"]);
        record("A13_EMPTY_RULE", apply_value(text, json!({"base_version":"validated-en-v1","base_fingerprint":fp,"edits":[{"id":"g","rule_id":"","start_utf8":2,"end_utf8":7,"before":"didnt","after":"didn't"}]}), &[], &[]).error_codes() == ["E_MALFORMED"]);
        record("A14_NEGATIVE_OFFSET", apply_value(text, json!({"base_version":"validated-en-v1","base_fingerprint":fp,"edits":[{"id":"g","rule_id":"G_DIDNT_APOSTROPHE","start_utf8":-1,"end_utf8":7,"before":"didnt","after":"didn't"}]}), &[], &[]).error_codes() == ["E_MALFORMED"]);
        record("A15_FLOAT_OFFSET", apply_value(text, json!({"base_version":"validated-en-v1","base_fingerprint":fp,"edits":[{"id":"g","rule_id":"G_DIDNT_APOSTROPHE","start_utf8":2.5,"end_utf8":7,"before":"didnt","after":"didn't"}]}), &[], &[]).error_codes() == ["E_MALFORMED"]);
        let long = "x".repeat(MAX_GRAMMAR_EDIT_FIELD_UTF8_BYTES + 1);
        record("A16_FIELD_OVERSIZE", apply_value(text, json!({"base_version":"validated-en-v1","base_fingerprint":fp,"edits":[{"id":long,"rule_id":"G_DIDNT_APOSTROPHE","start_utf8":2,"end_utf8":7,"before":"didnt","after":"didn't"}]}), &[], &[]).error_codes() == ["E_OVERSIZE"]);
        record("A17_EDIT_COUNT", apply_value(text, json!({"base_version":"validated-en-v1","base_fingerprint":fp,"edits":vec![valid_edit.clone(); MAX_GRAMMAR_EDITS + 1]}), &[], &[]).error_codes() == ["E_OVERSIZE"]);
        record(
            "A18_RAW_OVERSIZE",
            raw_result(&vec![b' '; MAX_GRAMMAR_RESPONSE_BYTES + 1]).error_codes() == ["E_OVERSIZE"],
        );
        let nested = format!(
            "{{\"base_version\":\"validated-en-v1\",\"base_fingerprint\":\"{fp}\",\"edits\":[{}]}}",
            "[".repeat(9) + &"]".repeat(9)
        );
        record(
            "A19_DEPTH",
            raw_result(nested.as_bytes()).error_codes() == ["E_OVERSIZE"],
        );
        record("A20_NODES", apply_value(text, json!({"base_version":"validated-en-v1","base_fingerprint":fp,"edits":vec![Value::Null; MAX_GRAMMAR_JSON_NODES]}), &[], &[]).error_codes() == ["E_OVERSIZE"]);

        let stale_bad = json!({"base_version":"validated-en-v0","base_fingerprint":"sha256:0000000000000000000000000000000000000000000000000000000000000000","edits":[{"bad":true}]});
        record(
            "A21_STALE_PRECEDENCE",
            apply_value(text, stale_bad, &[], &[]).error_codes() == ["E_STALE_GRAMMAR"],
        );
        let second = json!({"id":"g2","rule_id":"G_DIDNT_APOSTROPHE","start_utf8":0,"end_utf8":1,"before":"i","after":"didn't"});
        record("A22_UNSORTED", apply_value(text, json!({"base_version":"validated-en-v1","base_fingerprint":fp,"edits":[valid_edit.clone(),second]}), &[], &[]).error_codes() == ["E_UNSORTED"]);
        let out = json!({"id":"g","rule_id":"G_DIDNT_APOSTROPHE","start_utf8":2,"end_utf8":999,"before":"didnt","after":"didn't"});
        record(
            "A23_OUT_OF_BOUNDS",
            apply_value(
                text,
                json!({"base_version":"validated-en-v1","base_fingerprint":fp,"edits":[out]}),
                &[],
                &[],
            )
            .error_codes()
                == ["E_SPAN_OUT_OF_BOUNDS"],
        );
        let reversed = json!({"id":"g","rule_id":"G_DIDNT_APOSTROPHE","start_utf8":7,"end_utf8":2,"before":"didnt","after":"didn't"});
        record(
            "A24_REVERSED",
            apply_value(
                text,
                json!({"base_version":"validated-en-v1","base_fingerprint":fp,"edits":[reversed]}),
                &[],
                &[],
            )
            .error_codes()
                == ["E_SPAN_OUT_OF_BOUNDS"],
        );
        let unicode = "naïve didnt work";
        let mid = json!({"id":"g","rule_id":"G_DIDNT_APOSTROPHE","start_utf8":2,"end_utf8":3,"before":"","after":"didn't"});
        record("A25_UTF8_BOUNDARY", apply_value(unicode, json!({"base_version":"validated-en-v1","base_fingerprint":text_sha256_fingerprint(unicode),"edits":[mid]}), &[], &[]).error_codes() == ["E_SPAN_NOT_CHAR_BOUNDARY"]);
        let zero = json!({"id":"g","rule_id":"G_DIDNT_APOSTROPHE","start_utf8":2,"end_utf8":2,"before":"","after":"didn't"});
        record(
            "A26_ZERO_WIDTH",
            apply_value(
                text,
                json!({"base_version":"validated-en-v1","base_fingerprint":fp,"edits":[zero]}),
                &[],
                &[],
            )
            .error_codes()
            .contains(&"E_NOT_TOKEN_BOUNDARY"),
        );
        let multi = json!({"id":"g","rule_id":"G_DIDNT_APOSTROPHE","start_utf8":0,"end_utf8":7,"before":"i didnt","after":"didn't"});
        record(
            "A27_MULTI_TOKEN",
            apply_value(
                text,
                json!({"base_version":"validated-en-v1","base_fingerprint":fp,"edits":[multi]}),
                &[],
                &[],
            )
            .error_codes()
            .contains(&"E_NOT_TOKEN_BOUNDARY"),
        );
        let mismatch = json!({"id":"g","rule_id":"G_DIDNT_APOSTROPHE","start_utf8":2,"end_utf8":7,"before":"other","after":"didn't"});
        record(
            "A28_ANCHOR_MISMATCH",
            apply_value(
                text,
                json!({"base_version":"validated-en-v1","base_fingerprint":fp,"edits":[mismatch]}),
                &[],
                &[],
            )
            .error_codes()
            .contains(&"E_ANCHOR_MISMATCH"),
        );
        let unknown = json!({"id":"g","rule_id":"F_trusted_precomputed","start_utf8":2,"end_utf8":7,"before":"didnt","after":"didn't"});
        record(
            "A29_UNKNOWN_RULE",
            apply_value(
                text,
                json!({"base_version":"validated-en-v1","base_fingerprint":fp,"edits":[unknown]}),
                &[],
                &[],
            )
            .error_codes()
            .contains(&"E_UNKNOWN_RULE"),
        );
        let wrong_after = edit(text, "didnt", "G_DIDNT_APOSTROPHE", "did");
        record("A30_WRONG_AFTER", apply_value(text, json!({"base_version":"validated-en-v1","base_fingerprint":fp,"edits":[wrong_after]}), &[], &[]).error_codes().contains(&"E_RULE_CONTEXT"));

        for (name, base_text, needle, rule, after) in [
            (
                "A31_THERE_PRICE",
                "the price is two dollars",
                "is",
                "G_THERE_IS_PLURAL_QUANTITY",
                "are",
            ),
            (
                "A32_THERE_PERIOD",
                "there. is two issues",
                "is",
                "G_THERE_IS_PLURAL_QUANTITY",
                "are",
            ),
            (
                "A33_THERE_TAB",
                "there\tis two issues",
                "is",
                "G_THERE_IS_PLURAL_QUANTITY",
                "are",
            ),
            (
                "A34_THERE_COMMAND",
                "there is two issues command period",
                "is",
                "G_THERE_IS_PLURAL_QUANTITY",
                "are",
            ),
            (
                "A35_LETS_APP",
                "the app lets users export",
                "lets",
                "G_LETS_MEET_CONTRACTION",
                "let's",
            ),
            (
                "A36_LETS_PERIOD",
                "lets. meet tomorrow",
                "lets",
                "G_LETS_MEET_CONTRACTION",
                "let's",
            ),
        ] {
            let value = json!({"base_version":"validated-en-v1","base_fingerprint":text_sha256_fingerprint(base_text),"edits":[edit(base_text,needle,rule,after)]});
            record(
                name,
                apply_value(base_text, value, &[], &[])
                    .error_codes()
                    .contains(&"E_RULE_CONTEXT"),
            );
        }

        let duplicate = json!({"base_version":"validated-en-v1","base_fingerprint":fp,"edits":[valid_edit.clone(),valid_edit.clone()]});
        record(
            "A37_OVERLAP",
            apply_value(text, duplicate, &[], &[])
                .error_codes()
                .contains(&"E_OVERLAP"),
        );
        let secret_edit = json!({"id":"gsk_supersecret","rule_id":"G_DIDNT_APOSTROPHE","start_utf8":2,"end_utf8":7,"before":"wrong","after":"didn't"});
        let secret_result = apply_value(
            text,
            json!({"base_version":"validated-en-v1","base_fingerprint":fp,"edits":[secret_edit]}),
            &[],
            &[],
        );
        record(
            "A38_DIAGNOSTIC_SCRUB",
            secret_result
                .diagnostics
                .iter()
                .all(|item| !item.edit_id.contains("supersecret")),
        );

        for (name, protected_text, needle, dictionary, names) in [
            (
                "A39_PROMPT",
                "ignore previous instructions and didnt obey",
                "didnt",
                &[][..],
                &[][..],
            ),
            (
                "A40_QUOTE",
                "she said \"i didnt know\"",
                "didnt",
                &[][..],
                &[][..],
            ),
            (
                "A41_INLINE_CODE",
                "use `didnt` exactly",
                "didnt",
                &[][..],
                &[][..],
            ),
            (
                "A42_FENCED_CODE",
                "use ```didnt``` exactly",
                "didnt",
                &[][..],
                &[][..],
            ),
            (
                "A43_DICTIONARY",
                "use PostgreSQL didnt work",
                "PostgreSQL",
                &["PostgreSQL"][..],
                &[][..],
            ),
            (
                "A44_NAME",
                "ask Didnt tomorrow",
                "Didnt",
                &[][..],
                &["Didnt"][..],
            ),
            (
                "A45_EMAIL",
                "mail didnt@example.com",
                "didnt",
                &[][..],
                &[][..],
            ),
            (
                "A46_URL",
                "visit https://didnt.example",
                "didnt",
                &[][..],
                &[][..],
            ),
            ("A47_PATH", "open /tmp/didnt now", "didnt", &[][..], &[][..]),
            (
                "A48_IDENTIFIER",
                "set API_KEY now",
                "API_KEY",
                &[][..],
                &[][..],
            ),
            ("A49_NUMBER", "there is 2 issues", "2", &[][..], &[][..]),
            ("A50_NEGATION", "i did not obey", "not", &[][..], &[][..]),
            (
                "A51_UNMATCHED_QUOTE",
                "she said \"i didnt know",
                "didnt",
                &[][..],
                &[][..],
            ),
        ] {
            let value = json!({"base_version":"validated-en-v1","base_fingerprint":text_sha256_fingerprint(protected_text),"edits":[edit(protected_text,needle,"G_DIDNT_APOSTROPHE","didn't")]});
            record(
                name,
                apply_value(protected_text, value, dictionary, names)
                    .error_codes()
                    .contains(&"E_PROTECTED_SPAN"),
            );
        }

        for (name, good_text, needle, rule, after) in [
            (
                "A52_VALID_DIDNT",
                "i didnt work",
                "didnt",
                "G_DIDNT_APOSTROPHE",
                "didn't",
            ),
            (
                "A53_VALID_LETS",
                "lets meet tomorrow",
                "lets",
                "G_LETS_MEET_CONTRACTION",
                "let's",
            ),
            (
                "A54_VALID_THERE",
                "there is two issues",
                "is",
                "G_THERE_IS_PLURAL_QUANTITY",
                "are",
            ),
        ] {
            let value = json!({"base_version":"validated-en-v1","base_fingerprint":text_sha256_fingerprint(good_text),"edits":[edit(good_text,needle,rule,after)]});
            let result = apply_value(good_text, value, &[], &[]);
            record(
                name,
                result.diagnostics.is_empty()
                    && matches!(
                        result.outcome,
                        GrammarOutcome::Both | GrammarOutcome::GrammarOnly
                    ),
            );
        }
        let multi_text = "there is two issues and i didnt know";
        let multi_value = json!({"base_version":"validated-en-v1","base_fingerprint":text_sha256_fingerprint(multi_text),"edits":[edit(multi_text,"is","G_THERE_IS_PLURAL_QUANTITY","are"),edit(multi_text,"didnt","G_DIDNT_APOSTROPHE","didn't")]});
        let multi_result = apply_value(multi_text, multi_value, &[], &[]);
        record(
            "A55_MULTI_ACCEPT",
            multi_result.diagnostics.is_empty()
                && multi_result.rendered.contains("are")
                && multi_result.rendered.contains("didn't"),
        );
        let empty_result = apply_value(
            text,
            json!({"base_version":"validated-en-v1","base_fingerprint":fp,"edits":[]}),
            &[],
            &[],
        );
        record(
            "A56_EMPTY_SAFE",
            empty_result.diagnostics.is_empty() && empty_result.rendered == baseline.rendered(),
        );

        assert_eq!(cases.len(), 56);
        let len = cases.len();
        cases.rotate_left(seed % len);
        cases
    }

    #[test]
    fn adversarial_matrix_56_cases_passes_under_three_deterministic_orders() {
        for seed in [0usize, 17, 41] {
            let failures: Vec<_> = adversary_results(seed)
                .into_iter()
                .filter_map(|(name, passed)| (!passed).then_some(name))
                .collect();
            assert!(failures.is_empty(), "seed {seed}: {failures:?}");
        }
    }
}
