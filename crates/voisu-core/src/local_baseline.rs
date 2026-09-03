//! Deterministic local baseline organizer for Developer Prompt Rendering (DPR-T1 / #156).
//!
//! Always produces a Delivery-ready Natural-shaped (or Structured-labelled) baseline
//! from selected source text with **no network**, no cloud compose, and no grammar
//! rewrite catalog. Reuses closed cue *semantics* aligned with the formatting command
//! catalog and #138, but does **not** call Smart Writing / Minimal Grammar paths.
//!
//! # Residual R1 (multi-word deletion provenance)
//!
//! Local removals are fail-closed: only rule-justified, test-covered deletes fire.
//! Implemented removes in v1:
//! - leading clear fillers `um` / `uh` (whole tokens at the start only)
//! - clear backtrack `X no wait Y` → drop `X` and `no wait`, keep `Y` only when
//!   both sides are single alphabetic content tokens **and** X is not part of a
//!   multi-word left phrase (previous token is absent, non-content, or a closed
//!   function/boundary word). Multi-word lefts (`new york no wait london`) keep
//!   every word.
//!
//! Uncertain markers (`actually`, soft hedges) **preserve every word**. If a
//! candidate remove cannot be proven by these closed rules, all words stay.
//!
//! All `deterministic_local` / `literal_identity` fixtures with a defined
//! `local_baseline` are promoted as product corpus tests (see module tests).
//! DPR-33 enters this organizer through the T7-tested source-selection merge.

use crate::is_command_shaped;
use crate::prompt_rendering::{
    CLOSED_STRUCTURED_LABELS, RenderingPolicy, RenderingRoute, TimingCertainty,
};

/// Contract id bound into baseline metadata for diagnostics / later compose.
pub const LOCAL_BASELINE_CONTRACT_ID: &str = "voisu-dpr-local-baseline-v1:#156";

/// One clear or uncertain pause boundary supplied by the host/router.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PauseBoundary {
    /// Phrase immediately before the pause (matched case-insensitively as a
    /// contiguous token sequence inside the source).
    pub left_phrase: String,
    /// Phrase immediately after the pause.
    pub right_phrase: String,
    /// Observed pause length in milliseconds (informational; certainty decides layout).
    pub pause_ms: u32,
}

/// Optional timing evidence for paragraph layout.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalTiming {
    pub certainty: TimingCertainty,
    pub boundaries: Vec<PauseBoundary>,
}

/// Inputs for [`organize_local_baseline`].
///
/// All fields are owned/copied plain data so callers never panic on bad host
/// strings — unknown policy/route values are resolved upstream (T0); this API
/// accepts only typed enums.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalBaselineOptions {
    /// Rendering policy snapshotted at Recording start.
    pub policy: RenderingPolicy,
    /// Intent route for this utterance (`literal_identity` short-circuits).
    pub route: RenderingRoute,
    /// Optional pause timing for multi-paragraph layout.
    pub timing: Option<LocalTiming>,
}

impl Default for LocalBaselineOptions {
    fn default() -> Self {
        Self {
            policy: RenderingPolicy::Adaptive,
            route: RenderingRoute::DeterministicLocal,
            timing: None,
        }
    }
}

/// Sealed local baseline text ready for Delivery (or as cloud compose fallback).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalBaseline {
    rendered: String,
    contract: &'static str,
}

impl LocalBaseline {
    /// Stamp already-organized baseline text with [`LOCAL_BASELINE_CONTRACT_ID`].
    ///
    /// **Crate-internal only.** External callers (e.g. `voisu-app`) must use
    /// [`organize_local_baseline`] — the sole public constructor that produces
    /// a sealed baseline from selected source text. This helper exists for
    /// same-crate tests and compose-corpus fixtures that already hold organizer
    /// output; it must not mint a public baseline from arbitrary model prose.
    #[allow(dead_code)] // used by same-crate compose/local tests and corpus helpers
    #[must_use]
    pub(crate) fn from_organized_text(text: impl Into<String>) -> Self {
        Self {
            rendered: text.into(),
            contract: LOCAL_BASELINE_CONTRACT_ID,
        }
    }

    /// Final Transcript text for the local path.
    #[must_use]
    pub fn rendered(&self) -> &str {
        &self.rendered
    }

    /// Contract id for diagnostics.
    #[must_use]
    pub fn contract(&self) -> &str {
        self.contract
    }
}

/// Organize selected source text into a deterministic local baseline.
///
/// - `literal_identity` → spoken marks / quote cues only (no filler strip,
///   section organize, sentence casing, or terminal punctuation).
/// - otherwise → Natural-shaped organize; Structured policy may emit closed
///   section headers when speech supplies a supported cue.
///
/// Never panics on empty or odd Unicode input; never invents content words.
#[must_use]
pub fn organize_local_baseline(source: &str, options: &LocalBaselineOptions) -> LocalBaseline {
    let rendered = organize_impl(source, options);
    LocalBaseline {
        rendered,
        contract: LOCAL_BASELINE_CONTRACT_ID,
    }
}

/// True when leftover organized text still looks like a Goal or mixed
/// structured notes that may admit a formatting cloud call.
///
/// Uses intent-routing section-header cues (`goal is` / leading `goal` /
/// `goal:`), not mid-sentence token windows. Ordinary chat stays closed.
#[must_use]
pub fn leftover_admits_format_cloud(organized: &str) -> bool {
    crate::intent_routing::leftover_has_goal_or_mixed_section_headers(organized)
}

fn organize_impl(source: &str, options: &LocalBaselineOptions) -> String {
    if source.is_empty() {
        return String::new();
    }

    if is_preformatted_list(source) {
        return convert_preformatted_list(source);
    }

    if options.route == RenderingRoute::LiteralIdentity {
        if !source_has_spoken_cue(source) {
            return source.to_owned();
        }
        return apply_bare_spoken_cues(source);
    }

    let mut text = source.to_owned();

    // R1-safe removals first (operate on raw spoken words).
    text = strip_leading_clear_fillers(&text);
    text = apply_clear_backtrack(&text);

    // Spoken marks / quotes before section organize so Structured Goal
    // bodies still convert `dash dash` and `quote,`.
    let (cued, technical_converted) = apply_bare_spoken_cues_pass(&text);
    text = cued;

    // Spoken first/second/third at start or after a speech/pause boundary → numbered lines.
    if let Some(steps) = try_spoken_ordinal_steps(&text, options.timing.as_ref()) {
        return steps;
    }

    // Structural: multi-section / steps / Structured single-section labels.
    if let Some(structured) = try_section_organize(&text, options.policy) {
        return structured;
    }

    // Clear pause → paragraph break; uncertain → single stream.
    if let Some(timing) = options.timing.as_ref() {
        if timing.certainty == TimingCertainty::Clear {
            text = apply_clear_pause_breaks(&text, timing);
        }
    }

    // Polish surrounding prose. Skip casing/period only when the whole
    // utterance is a command, URL/path, or a single quoted span.
    if !skip_sentence_polish(&text, technical_converted) {
        text = apply_discourse_ok(&text);
        text = apply_vocative_hey(&text);
        // Unpaired bare `quote` (no `unquote`) leaves the following stream as words:
        // sentence-case the start, but do not weekday-capitalize inside that span (DPR-11).
        let weekday = !has_unpaired_quote_cue(source);
        text = apply_sentence_and_weekday_casing(&text, weekday);
        if !is_only_quoted_span(&text) {
            text = add_terminal_punctuation_dpr(&text);
        }
    }

    text
}

/// Skip sentence polish when a technical conversion produced a command, a
/// leading URL/path, or a quote-only span. Mixed prose still polishes.
fn skip_sentence_polish(text: &str, technical_converted: bool) -> bool {
    if is_only_quoted_span(text) {
        return true;
    }
    if !technical_converted {
        return false;
    }
    if is_command_shaped(text) {
        return true;
    }
    let first = text.split_whitespace().next().unwrap_or("");
    first.contains("://") || first.contains('/') || first.starts_with('-') || first.starts_with('.')
}

/// True when the whole utterance is one quoted span (`"leave this"`).
fn is_only_quoted_span(text: &str) -> bool {
    let t = text.trim();
    t.len() >= 2
        && t.starts_with('"')
        && t.ends_with('"')
        && t.bytes().filter(|&b| b == b'"').count() == 2
}

/// True when a bare `quote` token appears without a later `unquote`.
fn has_unpaired_quote_cue(text: &str) -> bool {
    let tokens = word_tokens(text);
    let mut open = false;
    for &(_, _, tok) in &tokens {
        let l = spoken_cue_token(tok);
        if l == "quote" {
            open = true;
        } else if l == "unquote" {
            open = false;
        }
    }
    open
}

// ─── Preformatted / identity helpers ─────────────────────────────────────────

fn is_preformatted_list(text: &str) -> bool {
    if !text.contains('\n') {
        return false;
    }
    let mut any = false;
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if starts_with_numbered_marker(t) || t.starts_with("- ") {
            any = true;
        } else {
            return false;
        }
    }
    any
}

fn convert_preformatted_list(source: &str) -> String {
    source
        .lines()
        .map(|line| {
            if line.trim().is_empty() || !source_has_spoken_cue(line) {
                line.to_owned()
            } else {
                apply_bare_spoken_cues(line)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn starts_with_numbered_marker(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0;
    if i >= bytes.len() || !bytes[i].is_ascii_digit() {
        return false;
    }
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    i + 1 < bytes.len() && bytes[i] == b'.' && bytes[i + 1] == b' '
}

// ─── R1 removals (fail closed) ───────────────────────────────────────────────

/// Leading whole-token fillers only. Mid-stream `um` / `uh` are content (DPR-23).
fn strip_leading_clear_fillers(text: &str) -> String {
    let tokens = word_tokens(text);
    if tokens.is_empty() {
        return text.to_owned();
    }
    let mut i = 0usize;
    while i < tokens.len() {
        let lower = ascii_lower(tokens[i].2);
        if lower == "um" || lower == "uh" {
            i += 1;
        } else {
            break;
        }
    }
    if i == 0 {
        return text.to_owned();
    }
    if i >= tokens.len() {
        // Entire utterance was fillers — keep identity (do not invent empty delete of all speech).
        return text.to_owned();
    }
    text[tokens[i].0..].trim_start().to_owned()
}

/// Clear correction: `… X no wait Y …` with single tokens X/Y → drop X and `no wait`.
///
/// Uncertain forms (`actually`, multi-token left sides, missing Y) keep every word.
///
/// Multi-word left provenance (R1): if the token immediately before X is itself a
/// content (non-function) word, X may be only the last token of a multi-word
/// phrase (e.g. `new york no wait london`). Partial delete is unsafe → fail closed.
fn apply_clear_backtrack(text: &str) -> String {
    let tokens = word_tokens(text);
    if tokens.len() < 4 {
        return text.to_owned();
    }
    // Find first `no` `wait` pair with one content token before and after.
    let mut i = 1usize;
    while i + 2 < tokens.len() {
        if ascii_lower(tokens[i].2) == "no" && ascii_lower(tokens[i + 1].2) == "wait" {
            let x = tokens[i - 1].2;
            let y = tokens[i + 2].2;
            // Fail closed: both sides must be single alphabetic content words.
            if is_content_word(x) && is_content_word(y) {
                // Multi-word left: previous content (non-function) word means X is
                // not a proven full correction span → preserve every word.
                if i >= 2 {
                    let prev = tokens[i - 2].2;
                    if is_content_word(prev) && !is_function_or_boundary_word(prev) {
                        return text.to_owned();
                    }
                }
                // Rebuild: tokens[..i-1] + tokens[i+2..]
                let mut out = String::new();
                for (k, &(s, e, _)) in tokens.iter().enumerate() {
                    if k == i - 1 || k == i || k == i + 1 {
                        continue;
                    }
                    if !out.is_empty() {
                        out.push(' ');
                    }
                    out.push_str(&text[s..e]);
                }
                return out;
            }
            // Provenance incomplete → keep all words.
            return text.to_owned();
        }
        i += 1;
    }
    text.to_owned()
}

fn is_content_word(tok: &str) -> bool {
    !tok.is_empty() && tok.chars().all(|c| c.is_ascii_alphabetic())
}

/// Closed-class / boundary words that do **not** form a multi-word left phrase
/// with the following content token X. Used only for R1 backtrack provenance:
/// `it friday no wait monday` is single-token left; `new york no wait london` is not.
fn is_function_or_boundary_word(tok: &str) -> bool {
    matches!(
        ascii_lower(tok).as_str(),
        "a" | "an"
            | "the"
            | "it"
            | "this"
            | "that"
            | "these"
            | "those"
            | "my"
            | "your"
            | "our"
            | "their"
            | "his"
            | "her"
            | "its"
            | "me"
            | "him"
            | "us"
            | "them"
            | "we"
            | "you"
            | "i"
            | "to"
            | "for"
            | "of"
            | "in"
            | "on"
            | "at"
            | "by"
            | "from"
            | "with"
            | "and"
            | "or"
            | "but"
            | "as"
            | "if"
            | "so"
            | "then"
            | "than"
            | "not"
            | "just"
            | "please"
            | "is"
            | "are"
            | "was"
            | "were"
            | "be"
            | "been"
            | "being"
            | "do"
            | "does"
            | "did"
            | "has"
            | "have"
            | "had"
            | "will"
            | "would"
            | "can"
            | "could"
            | "should"
            | "may"
            | "might"
            | "must"
            | "shall"
            | "um"
            | "uh"
    )
}

// ─── Section organize ────────────────────────────────────────────────────────

/// Closed section cues (longest match first). Wire label matches [`CLOSED_STRUCTURED_LABELS`].
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

const ORDINALS: &[(&str, u8)] = &[("one", 1), ("two", 2), ("three", 3), ("four", 4)];

/// Spoken step ordinals. v1 fires only on a full first/second/third sequence
/// at utterance start or after a credible speech boundary.
const SPOKEN_STEP_ORDINALS: &[(&str, u8)] = &[("first", 1), ("second", 2), ("third", 3)];

#[derive(Clone, Debug)]
struct SectionSpan {
    label: &'static str,
    /// Token index of the cue start.
    cue_start: usize,
    /// Token index after the cue (body start).
    body_start: usize,
}

fn try_section_organize(text: &str, policy: RenderingPolicy) -> Option<String> {
    let tokens = word_tokens(text);
    if tokens.is_empty() {
        return None;
    }
    let sections = find_section_spans(&tokens);
    if sections.is_empty() {
        return None;
    }

    let multi = sections.len() >= 2;
    let only_steps = sections.len() == 1 && sections[0].label == "Steps";
    let only_goal = sections.len() == 1 && sections[0].label == "Goal";
    let structured_single =
        policy == RenderingPolicy::Structured && sections.len() == 1 && !only_steps && !only_goal;

    // Adaptive/Natural with a single non-steps section: Natural-shaped (no headers).
    // A lone spoken Goal stays ordinary words — no local `Goal:` heading.
    if only_goal || (!multi && !only_steps && !structured_single) {
        return None;
    }

    // Steps-only (DPR-05): numbered lines, no "Steps" header under any policy.
    if only_steps {
        let body_tokens = &tokens[sections[0].body_start..];
        return try_numbered_steps_body(body_tokens)
            .map(|lines| lines.join("\n"))
            .and_then(|rendered| commit_section_organize(&tokens, &sections, rendered));
    }

    let mut parts: Vec<String> = Vec::new();
    for (idx, sec) in sections.iter().enumerate() {
        let body_end = sections
            .get(idx + 1)
            .map(|n| n.cue_start)
            .unwrap_or(tokens.len());
        let body = &tokens[sec.body_start..body_end];

        if sec.label == "Steps" {
            if let Some(lines) = try_numbered_steps_body(body) {
                if policy == RenderingPolicy::Structured && multi {
                    parts.push(format!("Steps:\n{}", lines.join("\n")));
                } else {
                    parts.push(lines.join("\n"));
                }
                continue;
            }
        }

        if sec.label == "Goal" {
            let body_text = join_tokens(body);
            let combined = if body_text.is_empty() {
                "Goal".to_owned()
            } else {
                format!("Goal {body_text}")
            };
            parts.push(ensure_terminal_period(&capitalize_sentence_start(
                &combined,
            )));
            continue;
        }

        let body_text = join_tokens(body);
        if policy == RenderingPolicy::Structured {
            // Closed labels only — never invent outside CLOSED_STRUCTURED_LABELS.
            debug_assert!(CLOSED_STRUCTURED_LABELS.contains(&sec.label));
            let body_cased = capitalize_sentence_start(&body_text);
            let body_punct = if sec.label == "Files" {
                body_cased
            } else {
                ensure_terminal_period(&body_cased)
            };
            parts.push(format!("{}:\n{}", sec.label, body_punct));
        } else {
            // Natural-shaped: keep cue word as sentence start, no colon header.
            let cue_display = natural_cue_display(sec.label);
            let combined = if body_text.is_empty() {
                cue_display.to_owned()
            } else {
                format!("{cue_display} {body_text}")
            };
            let cased = capitalize_sentence_start(&combined);
            // Paths/files often stay without forcing period mid-structure; use period.
            parts.push(ensure_terminal_period(&cased));
        }
    }

    if parts.is_empty() {
        return None;
    }

    let rendered = if policy == RenderingPolicy::Structured {
        parts.join("\n\n")
    } else {
        // Natural multi-section (DPR-26):
        //   prose sections space-joined;
        //   numbered steps on their own lines;
        //   post-steps prose space-joined after a single newline following the list.
        let mut out = String::new();
        for part in &parts {
            let is_list = part.lines().any(starts_with_numbered_marker);
            if out.is_empty() {
                out.push_str(part);
                continue;
            }
            if is_list {
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str(part);
            } else if out.lines().last().is_some_and(starts_with_numbered_marker) {
                out.push('\n');
                out.push_str(part);
            } else {
                out.push(' ');
                out.push_str(part);
            }
        }
        // Capitalize weekday tokens inside the assembled Natural multi-section text.
        apply_sentence_and_weekday_casing(&out, true)
    };
    commit_section_organize(&tokens, &sections, rendered)
}

/// Keep organization only when later spans are proved speech-boundary cues and
/// every source token that was not a consumed Structure Cue span or a rewritten
/// `1.`…`4.` step marker still appears in the rendered body.
fn commit_section_organize(
    tokens: &[(usize, usize, &str)],
    sections: &[SectionSpan],
    rendered: String,
) -> Option<String> {
    for sec in sections.iter().skip(1) {
        if !later_structure_cue(tokens, sec.cue_start) {
            return None;
        }
    }
    if preserves_surviving_content_tokens(tokens, sections, &rendered) {
        Some(rendered)
    } else {
        None
    }
}

fn preserves_surviving_content_tokens(
    tokens: &[(usize, usize, &str)],
    sections: &[SectionSpan],
    rendered: &str,
) -> bool {
    let source_keys = surviving_source_keys(tokens, sections);
    let rendered_keys: Vec<String> = word_tokens(rendered)
        .into_iter()
        .filter_map(|(_, _, tok)| {
            if is_rendered_step_marker(tok) {
                None
            } else {
                token_content_key(tok)
            }
        })
        .collect();
    is_key_subsequence(&source_keys, &rendered_keys)
}

fn surviving_source_keys(tokens: &[(usize, usize, &str)], sections: &[SectionSpan]) -> Vec<String> {
    let mut skip = vec![false; tokens.len()];
    for (idx, sec) in sections.iter().enumerate() {
        for flag in skip
            .get_mut(sec.cue_start..sec.body_start)
            .into_iter()
            .flatten()
        {
            *flag = true;
        }
        if sec.label != "Steps" {
            continue;
        }
        let body_end = sections
            .get(idx + 1)
            .map(|n| n.cue_start)
            .unwrap_or(tokens.len());
        let body = &tokens[sec.body_start..body_end];
        if try_numbered_steps_body(body).is_none() {
            continue;
        }
        for (flag, (_, _, tok)) in skip[sec.body_start..body_end]
            .iter_mut()
            .zip(tokens[sec.body_start..body_end].iter())
        {
            if ORDINALS.iter().any(|(w, _)| ascii_lower(tok) == *w) {
                *flag = true;
            }
        }
    }
    tokens
        .iter()
        .enumerate()
        .filter(|(i, _)| !skip[*i])
        .filter_map(|(_, (_, _, tok))| token_content_key(tok))
        .collect()
}

fn token_content_key(tok: &str) -> Option<String> {
    let core = tok.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    if core.is_empty() {
        None
    } else {
        Some(ascii_lower(core))
    }
}

fn is_rendered_step_marker(tok: &str) -> bool {
    matches!(tok.trim(), "1." | "2." | "3." | "4.")
}

fn is_key_subsequence(needle: &[String], haystack: &[String]) -> bool {
    let mut i = 0usize;
    for h in haystack {
        if i < needle.len() && h == &needle[i] {
            i += 1;
        }
    }
    i == needle.len()
}

fn natural_cue_display(label: &str) -> &str {
    // Natural keeps spoken casing shape: first word of the closed label.
    match label {
        "Acceptance Criteria" => "Acceptance criteria",
        "Goal" => "Goal",
        "Context" => "Context",
        "Requirements" => "Requirements",
        "Constraints" => "Constraints",
        "Steps" => "Steps",
        "Files" => "Files",
        "Notes" => "Notes",
        other => other,
    }
}

fn find_section_spans(tokens: &[(usize, usize, &str)]) -> Vec<SectionSpan> {
    if tokens.is_empty() {
        return Vec::new();
    }
    // A Recording must begin with a Structure Cue (token 0 of the post-filler stream).
    let Some((label, cue_len)) = match_section_cue(tokens, 0) else {
        return Vec::new();
    };
    let mut out = vec![SectionSpan {
        label,
        cue_start: 0,
        body_start: cue_len,
    }];
    let mut i = cue_len;
    while i < tokens.len() {
        if let Some((label, cue_len)) = match_section_cue(tokens, i) {
            if later_structure_cue(tokens, i) {
                out.push(SectionSpan {
                    label,
                    cue_start: i,
                    body_start: i + cue_len,
                });
                i += cue_len;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Later Structure Cues occur at a sentence boundary or as unpunctuated
/// consecutive-cue dictation. After any earlier `.!?`, a mid-clause cue-shaped
/// noun (`ecological context`, `and context`, `several constraints`) stays prose.
fn later_structure_cue(tokens: &[(usize, usize, &str)], i: usize) -> bool {
    if i == 0 {
        return true;
    }
    if token_ends_speech_boundary(tokens[i - 1].2) {
        return true;
    }
    if tokens[..i]
        .iter()
        .any(|(_, _, tok)| token_ends_speech_boundary(tok))
    {
        return false;
    }
    !preceded_by_determiner(tokens, i)
}

fn token_ends_speech_boundary(tok: &str) -> bool {
    tok.trim_end().ends_with(['.', '!', '?'])
}

fn preceded_by_determiner(tokens: &[(usize, usize, &str)], i: usize) -> bool {
    let prev = tokens[i - 1].2;
    let core = prev.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    matches!(
        ascii_lower(core).as_str(),
        "the"
            | "a"
            | "an"
            | "my"
            | "your"
            | "our"
            | "their"
            | "his"
            | "her"
            | "its"
            | "this"
            | "that"
            | "these"
            | "those"
            | "some"
            | "any"
    )
}

fn match_section_cue(tokens: &[(usize, usize, &str)], i: usize) -> Option<(&'static str, usize)> {
    for &(phrase, label) in SECTION_CUES {
        if phrase_match(tokens, i, phrase) {
            return Some((label, phrase.len()));
        }
    }
    None
}

fn phrase_match(tokens: &[(usize, usize, &str)], i: usize, phrase: &[&str]) -> bool {
    if i + phrase.len() > tokens.len() {
        return false;
    }
    phrase
        .iter()
        .enumerate()
        .all(|(k, w)| ascii_lower(tokens[i + k].2) == *w)
}

/// Match a spoken cue phrase after stripping one trailing comma or period
/// from each token (`quote,` → `quote`). Case-insensitive.
fn cue_phrase_match(tokens: &[(usize, usize, &str)], i: usize, phrase: &[&str]) -> bool {
    if i + phrase.len() > tokens.len() {
        return false;
    }
    phrase
        .iter()
        .enumerate()
        .all(|(k, w)| spoken_cue_token(tokens[i + k].2) == *w)
}

fn spoken_cue_token(tok: &str) -> String {
    let lower = ascii_lower(tok);
    if lower.ends_with(',') || lower.ends_with('.') {
        lower[..lower.len() - 1].to_owned()
    } else {
        lower
    }
}

/// True when the source contains a spoken mark, quote pair, or layout cue
/// that LiteralIdentity must still convert.
fn source_has_spoken_cue(text: &str) -> bool {
    let tokens = word_tokens(text);
    let mut i = 0usize;
    while i < tokens.len() {
        if spoken_cue_token(tokens[i].2) == "quote"
            && (i + 1..tokens.len()).any(|j| spoken_cue_token(tokens[j].2) == "unquote")
        {
            return true;
        }
        if cue_phrase_match(&tokens, i, &["colon", "slash", "slash"])
            || cue_phrase_match(&tokens, i, &["dash", "dash"])
            || cue_phrase_match(&tokens, i, &["dash"])
            || cue_phrase_match(&tokens, i, &["slash"])
            || cue_phrase_match(&tokens, i, &["dot"])
            || cue_phrase_match(&tokens, i, &["new", "paragraph"])
            || cue_phrase_match(&tokens, i, &["new", "line"])
            || cue_phrase_match(&tokens, i, &["exclamation", "point"])
            || (cue_phrase_match(&tokens, i, &["period"]) && period_is_cue(&tokens, i))
        {
            return true;
        }
        i += 1;
    }
    false
}

fn try_numbered_steps_body(body: &[(usize, usize, &str)]) -> Option<Vec<String>> {
    if body.is_empty() {
        return None;
    }
    // Expect one/two/three/four … runs.
    let mut items: Vec<(u8, String)> = Vec::new();
    let mut i = 0usize;
    while i < body.len() {
        let lower = ascii_lower(body[i].2);
        let Some(&(_, num)) = ORDINALS.iter().find(|(w, _)| *w == lower) else {
            // Non-ordinal content before first item → not a pure steps body.
            if items.is_empty() {
                return None;
            }
            // Append leftover to last item.
            if let Some(last) = items.last_mut() {
                last.1.push(' ');
                last.1.push_str(body[i].2);
            }
            i += 1;
            continue;
        };
        i += 1;
        let mut words = Vec::new();
        while i < body.len() {
            let l = ascii_lower(body[i].2);
            if ORDINALS.iter().any(|(w, _)| *w == l) {
                break;
            }
            words.push(body[i].2);
            i += 1;
        }
        if words.is_empty() {
            return None;
        }
        let item = words.join(" ");
        items.push((num, capitalize_sentence_start(&item)));
    }
    if items.is_empty() {
        return None;
    }
    // Require strictly increasing starting at 1 for v1 safety.
    for (idx, (num, _)) in items.iter().enumerate() {
        if *num as usize != idx + 1 {
            return None;
        }
    }
    Some(
        items
            .into_iter()
            .map(|(n, t)| format!("{n}. {t}"))
            .collect(),
    )
}

/// Convert a spoken `first` … `second` … `third` sequence into numbered lines.
///
/// The sequence may start the utterance or follow a credible speech boundary
/// (previous token ends with `.!?`, an explicit spoken break / newline, or a
/// Clear pause). Mid-clause `first` does not open a list. Later `second` /
/// `third` must themselves sit at a speech boundary, or belong to an
/// unpunctuated consecutive run with no `.!?` inside an item. An item body
/// that contains `.!?` not immediately before the next ordinal fails closed.
/// `The first time`, rankings, and dates stay prose.
fn try_spoken_ordinal_steps(text: &str, timing: Option<&LocalTiming>) -> Option<String> {
    let tokens = word_tokens(text);
    if tokens.is_empty() {
        return None;
    }
    let start = spoken_ordinal_list_start(text, &tokens, timing)?;

    let mut items: Vec<(u8, String)> = Vec::new();
    let mut markers: Vec<usize> = Vec::new();
    let mut i = start;
    while i < tokens.len() {
        let Some(num) = spoken_list_step_number(&tokens, i) else {
            if items.is_empty() {
                return None;
            }
            if let Some(last) = items.last_mut() {
                last.1.push(' ');
                last.1.push_str(tokens[i].2);
            }
            i += 1;
            continue;
        };
        markers.push(i);
        i += 1;
        let mut words = Vec::new();
        while i < tokens.len() {
            if spoken_list_step_number(&tokens, i).is_some() {
                break;
            }
            if token_ends_speech_boundary(tokens[i].2) {
                let following_ordinal =
                    i + 1 < tokens.len() && spoken_list_step_number(&tokens, i + 1).is_some();
                if !following_ordinal && i + 1 < tokens.len() {
                    // Intervening sentence after a boundary `first` — do not
                    // harvest a later `second`/`third` from the rest of the Recording.
                    return None;
                }
                words.push(tokens[i].2);
                i += 1;
                if following_ordinal {
                    break;
                }
                continue;
            }
            words.push(tokens[i].2);
            i += 1;
        }
        if words.is_empty() {
            return None;
        }
        items.push((num, capitalize_sentence_start(&words.join(" "))));
    }
    if items.len() != 3 {
        return None;
    }
    for (idx, (num, _)) in items.iter().enumerate() {
        if *num as usize != idx + 1 {
            return None;
        }
    }
    let list = items
        .into_iter()
        .map(|(n, t)| format!("{n}. {t}"))
        .collect::<Vec<_>>()
        .join("\n");
    let rendered = join_intro_and_spoken_list(text, &tokens, start, &list);
    commit_spoken_ordinal_steps(&tokens, &markers, rendered)
}

/// First boundary-qualified list `first`. Date/ranking `first` is skipped.
fn spoken_ordinal_list_start(
    text: &str,
    tokens: &[(usize, usize, &str)],
    timing: Option<&LocalTiming>,
) -> Option<usize> {
    tokens.iter().enumerate().find_map(|(i, _)| {
        (spoken_list_step_number(tokens, i) == Some(1)
            && spoken_list_open_boundary(text, tokens, i, timing))
        .then_some(i)
    })
}

/// List open is utterance-initial, after `.!?`, after an explicit spoken break,
/// or after a Clear pause immediately before this token.
/// Unlike [`later_structure_cue`], a mid-clause `first` never opens a list.
fn spoken_list_open_boundary(
    text: &str,
    tokens: &[(usize, usize, &str)],
    i: usize,
    timing: Option<&LocalTiming>,
) -> bool {
    if i == 0 {
        return true;
    }
    if token_ends_speech_boundary(tokens[i - 1].2) {
        return true;
    }
    if text[tokens[i - 1].1..tokens[i].0].contains('\n') {
        return true;
    }
    clear_pause_before_token(tokens, i, timing)
}

/// `first`/`second`/`third` used as a list marker, not a date or ranking.
fn spoken_list_step_number(tokens: &[(usize, usize, &str)], i: usize) -> Option<u8> {
    let cue = spoken_cue_token(tokens[i].2);
    let &(_, num) = SPOKEN_STEP_ORDINALS.iter().find(|(w, _)| *w == cue)?;
    if i > 0 && !token_ends_speech_boundary(tokens[i - 1].2) && preceded_by_determiner(tokens, i) {
        return None;
    }
    if ordinal_followed_by_ranking_copula(tokens, i) {
        return None;
    }
    Some(num)
}

fn ordinal_followed_by_ranking_copula(tokens: &[(usize, usize, &str)], i: usize) -> bool {
    tokens.get(i + 1).is_some_and(|(_, _, tok)| {
        matches!(
            spoken_cue_token(tok).as_str(),
            "was" | "is" | "are" | "were"
        )
    })
}

fn clear_pause_before_token(
    tokens: &[(usize, usize, &str)],
    i: usize,
    timing: Option<&LocalTiming>,
) -> bool {
    let Some(timing) = timing else {
        return false;
    };
    if timing.certainty != TimingCertainty::Clear || i == 0 {
        return false;
    }
    timing.boundaries.iter().any(|boundary| {
        let left: Vec<String> = boundary
            .left_phrase
            .split_whitespace()
            .map(ascii_lower)
            .collect();
        let right: Vec<String> = boundary
            .right_phrase
            .split_whitespace()
            .map(ascii_lower)
            .collect();
        if left.is_empty() || right.is_empty() || i < left.len() {
            return false;
        }
        phrase_tokens_at(tokens, i - left.len(), &left) && phrase_tokens_at(tokens, i, &right)
    })
}

fn phrase_tokens_at(tokens: &[(usize, usize, &str)], start: usize, phrase: &[String]) -> bool {
    if start + phrase.len() > tokens.len() {
        return false;
    }
    phrase
        .iter()
        .enumerate()
        .all(|(k, w)| ascii_lower(tokens[start + k].2) == *w)
}

/// Keep a spoken `new paragraph` (`\n\n`) instead of collapsing it to one newline.
fn join_intro_and_spoken_list(
    text: &str,
    tokens: &[(usize, usize, &str)],
    start: usize,
    list: &str,
) -> String {
    if start == 0 {
        return list.to_owned();
    }
    let prefix = text[..tokens[start].0].trim_end();
    if prefix.is_empty() {
        return list.to_owned();
    }
    let gap = &text[tokens[start - 1].1..tokens[start].0];
    let sep = if gap.contains("\n\n") { "\n\n" } else { "\n" };
    let prefix = ensure_terminal_period(&capitalize_sentence_start(prefix));
    format!("{prefix}{sep}{list}")
}

/// Keep the conversion only when every non-ordinal-marker source token survives
/// once, in order, including introductory prose before an embedded list.
fn commit_spoken_ordinal_steps(
    tokens: &[(usize, usize, &str)],
    markers: &[usize],
    rendered: String,
) -> Option<String> {
    let mut skip = vec![false; tokens.len()];
    for &i in markers {
        skip[i] = true;
    }
    let source_keys: Vec<String> = tokens
        .iter()
        .enumerate()
        .filter(|(i, _)| !skip[*i])
        .filter_map(|(_, (_, _, tok))| token_content_key(tok))
        .collect();
    let rendered_keys: Vec<String> = word_tokens(&rendered)
        .into_iter()
        .filter_map(|(_, _, tok)| {
            if is_rendered_step_marker(tok) {
                None
            } else {
                token_content_key(tok)
            }
        })
        .collect();
    if is_key_subsequence(&source_keys, &rendered_keys) {
        Some(rendered)
    } else {
        None
    }
}

// ─── Bare spoken cues ────────────────────────────────────────────────────────

fn apply_bare_spoken_cues(text: &str) -> String {
    apply_bare_spoken_cues_pass(text).0
}

fn apply_bare_spoken_cues_pass(text: &str) -> (String, bool) {
    let tokens = word_tokens(text);
    if tokens.is_empty() {
        return (text.to_owned(), false);
    }

    // Pass 1: paired quote … unquote (protect interior from further cue conversion).
    let mut skip: Vec<bool> = vec![false; tokens.len()];
    let mut quote_pairs: Vec<(usize, usize)> = Vec::new();
    {
        let mut i = 0usize;
        while i < tokens.len() {
            if spoken_cue_token(tokens[i].2) == "quote" {
                if let Some(j) =
                    (i + 1..tokens.len()).find(|&j| spoken_cue_token(tokens[j].2) == "unquote")
                {
                    quote_pairs.push((i, j));
                    skip[i..=j].fill(true);
                    i = j + 1;
                    continue;
                }
            }
            i += 1;
        }
    }

    // Metalinguistic "words X and Y" spans: do not convert symbol phrases inside.
    let meta = metalinguistic_mask(&tokens);

    let mut out = String::new();
    let mut i = 0usize;
    let mut pending_space = false;
    let mut technical_converted = false;

    let push_space = |out: &mut String, pending: &mut bool| {
        if *pending && !out.is_empty() {
            let last = out.chars().last();
            // No space right after a line break; allow space after closing quotes.
            if last.is_some_and(|c| c != '\n') {
                out.push(' ');
            }
        }
        *pending = false;
    };

    while i < tokens.len() {
        // Quote pair.
        if let Some(&(q, u)) = quote_pairs.iter().find(|(q, _)| *q == i) {
            push_space(&mut out, &mut pending_space);
            out.push('"');
            out.push_str(&join_quote_interior(&tokens[q + 1..u]));
            out.push('"');
            i = u + 1;
            pending_space = true;
            continue;
        }

        if skip[i] {
            // Should be consumed by quote pair.
            i += 1;
            continue;
        }

        // Spoken technical marks convert anywhere (including metalinguistic
        // "words X and Y" spans). Longest phrase first.
        if cue_phrase_match(&tokens, i, &["colon", "slash", "slash"]) {
            trim_trailing_space(&mut out);
            out.push_str("://");
            i += 3;
            pending_space = false;
            technical_converted = true;
            continue;
        }
        if cue_phrase_match(&tokens, i, &["dash", "dash"]) {
            push_space(&mut out, &mut pending_space);
            out.push_str("--");
            i += 2;
            pending_space = false;
            technical_converted = true;
            continue;
        }
        if cue_phrase_match(&tokens, i, &["dash"]) && dash_is_cue(&tokens, i) {
            push_space(&mut out, &mut pending_space);
            out.push('-');
            i += 1;
            pending_space = false;
            technical_converted = true;
            continue;
        }
        if cue_phrase_match(&tokens, i, &["slash"]) {
            trim_trailing_space(&mut out);
            out.push('/');
            i += 1;
            pending_space = false;
            technical_converted = true;
            continue;
        }
        if cue_phrase_match(&tokens, i, &["dot"]) {
            trim_trailing_space(&mut out);
            out.push('.');
            i += 1;
            pending_space = false;
            technical_converted = true;
            continue;
        }

        // Multi-word cues (longest first), unless metalinguistic.
        if !meta[i] {
            if cue_phrase_match(&tokens, i, &["exclamation", "point"]) {
                push_space(&mut out, &mut pending_space);
                // Glue bang to previous word.
                trim_trailing_space(&mut out);
                out.push('!');
                i += 2;
                pending_space = true;
                continue;
            }
            if cue_phrase_match(&tokens, i, &["new", "paragraph"]) {
                push_space(&mut out, &mut pending_space);
                trim_trailing_space(&mut out);
                // Ensure previous clause has period before break when it lacks punct.
                ensure_clause_period(&mut out);
                out.push_str("\n\n");
                i += 2;
                pending_space = false;
                continue;
            }
            if cue_phrase_match(&tokens, i, &["new", "line"]) {
                push_space(&mut out, &mut pending_space);
                trim_trailing_space(&mut out);
                ensure_clause_period(&mut out);
                out.push('\n');
                i += 2;
                pending_space = false;
                continue;
            }
            if cue_phrase_match(&tokens, i, &["period"]) && period_is_cue(&tokens, i) {
                push_space(&mut out, &mut pending_space);
                trim_trailing_space(&mut out);
                out.push('.');
                i += 1;
                pending_space = true;
                continue;
            }
        }

        push_space(&mut out, &mut pending_space);
        out.push_str(tokens[i].2);
        i += 1;
        pending_space = true;
    }

    (out, technical_converted)
}

/// Lone `dash` is a cue unless it is the English noun (`a dash of salt`).
fn dash_is_cue(tokens: &[(usize, usize, &str)], i: usize) -> bool {
    let prev = i.checked_sub(1).map(|j| spoken_cue_token(tokens[j].2));
    let next = tokens.get(i + 1).map(|token| spoken_cue_token(token.2));
    !matches!(
        (prev.as_deref(), next.as_deref()),
        (Some("a") | Some("the"), Some("of"))
    )
}

/// `period` is a cue unless it is ordinary language ("the period of", mid-list, …).
fn period_is_cue(tokens: &[(usize, usize, &str)], i: usize) -> bool {
    // "the period of …"
    if i > 0
        && ascii_lower(tokens[i - 1].2) == "the"
        && i + 1 < tokens.len()
        && ascii_lower(tokens[i + 1].2) == "of"
    {
        return false;
    }
    // "a period …" ordinary noun
    if i > 0 {
        let prev = ascii_lower(tokens[i - 1].2);
        if prev == "a" || prev == "the" || prev == "this" || prev == "that" {
            return false;
        }
    }
    // After a content word (or start after filler strip): treat as cue when
    // followed by end, another cue, or more content (stacked "stop period new line").
    true
}

fn metalinguistic_mask(tokens: &[(usize, usize, &str)]) -> Vec<bool> {
    let mut mask = vec![false; tokens.len()];
    // "words A and B" or "words A and B and C" — mark A,B (and multi-word symbol phrases).
    for i in 0..tokens.len() {
        if ascii_lower(tokens[i].2) != "words" {
            continue;
        }
        // Mark following tokens until a clear end ("out", "out loud", end) as meta.
        let mut j = i + 1;
        while j < tokens.len() {
            let l = ascii_lower(tokens[j].2);
            if l == "out" || l == "loud" || l == "aloud" {
                break;
            }
            mask[j] = true;
            j += 1;
        }
    }
    // "say the words …" already covered via "words".
    // "type quote … unquote" interiors handled by quote skip.
    mask
}

fn ensure_clause_period(out: &mut String) {
    let trimmed = out.trim_end();
    if trimmed.is_empty() {
        return;
    }
    if trimmed.ends_with(['.', '!', '?', ':', '\n']) {
        return;
    }
    // Only when there is a word to punctuate.
    if trimmed
        .chars()
        .last()
        .is_some_and(|c| c.is_alphanumeric() || c == ')')
    {
        let trail_ws = out.len() - trimmed.len();
        out.truncate(trimmed.len());
        out.push('.');
        for _ in 0..trail_ws {
            out.push(' ');
        }
    }
}

fn trim_trailing_space(out: &mut String) {
    while out.ends_with(' ') {
        out.pop();
    }
}

// ─── Pause layout ────────────────────────────────────────────────────────────

fn apply_clear_pause_breaks(text: &str, timing: &LocalTiming) -> String {
    let mut result = text.to_owned();
    for boundary in &timing.boundaries {
        let left = boundary.left_phrase.trim();
        let right = boundary.right_phrase.trim();
        if left.is_empty() || right.is_empty() {
            continue;
        }
        // Find left phrase as contiguous tokens, then right phrase after it.
        if let Some(joined) = split_on_phrases(&result, left, right) {
            result = joined;
        }
    }
    result
}

fn split_on_phrases(text: &str, left: &str, right: &str) -> Option<String> {
    let tokens = word_tokens(text);
    let left_toks: Vec<String> = left.split_whitespace().map(ascii_lower).collect();
    let right_toks: Vec<String> = right.split_whitespace().map(ascii_lower).collect();
    if left_toks.is_empty() || right_toks.is_empty() {
        return None;
    }
    // Find left sequence.
    let mut li = None;
    'outer: for i in 0..=tokens.len().saturating_sub(left_toks.len()) {
        for (k, w) in left_toks.iter().enumerate() {
            if ascii_lower(tokens[i + k].2) != *w {
                continue 'outer;
            }
        }
        li = Some(i);
        break;
    }
    let li = li?;
    let after_left = li + left_toks.len();
    // Right should start at after_left (contiguous in source without pause marker).
    if after_left + right_toks.len() > tokens.len() {
        return None;
    }
    for (k, w) in right_toks.iter().enumerate() {
        if ascii_lower(tokens[after_left + k].2) != *w {
            return None;
        }
    }
    // Build: tokens[..after_left] + ".\n\n" + tokens[after_left..]
    let mut left_part = join_tokens(&tokens[..after_left]);
    left_part = ensure_terminal_period(&capitalize_sentence_start(&left_part));
    // Drop trailing period we may double — ensure_terminal_period already.
    let right_part = join_tokens(&tokens[after_left..]);
    Some(format!("{left_part}\n\n{right_part}"))
}

// ─── Discourse / vocative / casing / terminal punct ──────────────────────────

fn apply_vocative_hey(text: &str) -> String {
    let tokens = word_tokens(text);
    if tokens.is_empty() {
        return text.to_owned();
    }
    if ascii_lower(tokens[0].2) != "hey" {
        return text.to_owned();
    }
    if tokens.len() == 1 {
        return text.to_owned();
    }
    // hey␠… → Hey, …
    let rest = text[tokens[0].1..].trim_start();
    format!("Hey, {rest}")
}

/// `… ok I …` / `… ok i …` → `…, ok? I …` (discourse split).
fn apply_discourse_ok(text: &str) -> String {
    let tokens = word_tokens(text);
    if tokens.len() < 3 {
        return text.to_owned();
    }
    for i in 1..tokens.len() - 1 {
        if ascii_lower(tokens[i].2) != "ok" {
            continue;
        }
        // Mid-stream ok followed by more content → discourse comma + question + new sentence.
        let mut out = String::new();
        // Left of ok
        out.push_str(&join_tokens(&tokens[..i]));
        out.push_str(", ok? ");
        let right = join_tokens(&tokens[i + 1..]);
        out.push_str(&right);
        return out;
    }
    // Trailing ok (end of utterance): comma before ok; terminal ? applied later.
    if let Some(last) = tokens.last() {
        if ascii_lower(last.2) == "ok" && tokens.len() >= 2 {
            let head = join_tokens(&tokens[..tokens.len() - 1]);
            return format!("{head}, ok");
        }
    }
    text.to_owned()
}

const WEEKDAYS: &[&str] = &[
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
];

fn apply_sentence_and_weekday_casing(text: &str, capitalize_weekdays: bool) -> String {
    if text.is_empty() {
        return text.to_owned();
    }
    let mut chars: Vec<char> = text.chars().collect();
    let mut byte_to_char = vec![0usize; text.len() + 1];
    {
        let mut ci = 0usize;
        for (bi, ch) in text.char_indices() {
            byte_to_char[bi] = ci;
            ci += 1;
            let end = bi + ch.len_utf8();
            if end <= text.len() {
                byte_to_char[end] = ci;
            }
        }
        byte_to_char[text.len()] = ci;
    }

    for (s, e, tok) in word_tokens(text) {
        let cs = byte_to_char[s];
        // Strip trailing punctuation for weekday / pronoun checks (`Hey,` → `Hey`).
        let core = tok.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        // Paths/URLs/code-like tokens keep their source case even at line/sentence start
        // (same mid-sentence protection: never force-uppercase the first character).
        if is_protected_case_token(tok) {
            let _ = e;
            continue;
        }
        if is_sentence_or_line_start(text, s) {
            if let Some(c) = chars.get_mut(cs) {
                if c.is_alphabetic() {
                    *c = c.to_ascii_uppercase();
                }
            }
        } else if core == "i" || core == "I" {
            if core.len() == 1 {
                if let Some(c) = chars.get_mut(cs) {
                    *c = 'I';
                }
            }
        } else if capitalize_weekdays && WEEKDAYS.iter().any(|w| eq_ascii_ignore_case(core, w)) {
            if let Some(c) = chars.get_mut(cs) {
                *c = c.to_ascii_uppercase();
            }
            for (k, ch) in core.chars().enumerate().skip(1) {
                if let Some(c) = chars.get_mut(cs + k) {
                    *c = ch.to_ascii_lowercase();
                }
            }
        }
        let _ = e;
    }
    chars.into_iter().collect()
}

fn is_sentence_or_line_start(text: &str, byte_idx: usize) -> bool {
    if byte_idx == 0 {
        return true;
    }
    let before = &text[..byte_idx];
    if before.ends_with('\n') {
        return true;
    }
    let trimmed = before.trim_end_matches([' ', '\t']);
    trimmed.ends_with(['.', '!', '?'])
}

fn is_closed_question(text: &str) -> bool {
    let body = text.trim().trim_end_matches(['.', '!', '?']);
    if body.is_empty() {
        return false;
    }
    let lower = ascii_lower(body);
    if lower.contains("can you") || lower.contains("could you") || lower.contains("would you") {
        return true;
    }
    if let Some(last) = body.split_whitespace().last() {
        if ascii_lower(last.trim_end_matches(['.', '!', '?', ','])) == "ok" {
            return true;
        }
    }
    false
}

fn add_terminal_punctuation_dpr(text: &str) -> String {
    if text.is_empty() {
        return text.to_owned();
    }

    // Paragraphs: period each non-empty fragment when missing.
    if text.contains("\n\n") {
        let parts: Vec<String> = text
            .split("\n\n")
            .map(|p| {
                let t = p.trim();
                if t.is_empty() {
                    String::new()
                } else {
                    let cased = t.to_owned();
                    ensure_terminal_punct_line(&cased)
                }
            })
            .collect();
        return parts.join("\n\n");
    }

    // Single newlines: only cascade when a line independently needs terminal punct
    // (matches #138 DPR-10: "Stop.\nNext item" keeps bare second line).
    if text.contains('\n') {
        let lines: Vec<&str> = text.split('\n').collect();
        let needs = lines.iter().any(|l| line_needs_terminal(l));
        if needs {
            return lines
                .iter()
                .map(|l| {
                    if l.trim().is_empty() {
                        (*l).to_owned()
                    } else {
                        ensure_terminal_punct_line(l)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
        }
        return text.to_owned();
    }

    ensure_terminal_punct_line(text)
}

fn line_needs_terminal(line: &str) -> bool {
    let s = line.trim();
    if s.is_empty() || s.ends_with(['.', '!', '?', ':']) {
        return false;
    }
    if starts_with_numbered_marker(s) || s.starts_with("- ") {
        return false;
    }
    if is_closed_question(s) {
        return true;
    }
    // Finite-verb-ish: at least two tokens and not a bare noun phrase title.
    let words = word_tokens(s);
    words.len() >= 2
        && words.iter().any(|(_, _, w)| {
            const VERBS: &[&str] = &[
                "is", "are", "was", "were", "be", "have", "has", "had", "do", "does", "did",
                "will", "would", "can", "could", "should", "may", "might", "must", "think", "send",
                "ship", "meet", "file", "ignore", "enable", "returns", "works", "ping", "let",
                "aint", "missing", "open", "clone", "use", "keep", "type", "replace", "gonna",
                "please", "there",
            ];
            VERBS.iter().any(|v| eq_ascii_ignore_case(w, v))
        })
}

fn ensure_terminal_punct_line(line: &str) -> String {
    let stripped = line.trim_end();
    if stripped.is_empty() || stripped.ends_with(['.', '!', '?', ':']) {
        return line.to_owned();
    }
    if starts_with_numbered_marker(stripped) || stripped.starts_with("- ") {
        return line.to_owned();
    }
    let trail = &line[stripped.len()..];
    if is_closed_question(stripped) {
        return format!("{stripped}?{trail}");
    }
    format!("{stripped}.{trail}")
}

fn ensure_terminal_period(text: &str) -> String {
    let stripped = text.trim_end();
    if stripped.is_empty() || stripped.ends_with(['.', '!', '?', ':']) {
        return text.to_owned();
    }
    format!("{stripped}.")
}

fn capitalize_sentence_start(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        None => String::new(),
        Some(f) => {
            // If the whole leading token is path/URL/code-like, preserve case.
            let first_tok = text.split_whitespace().next().unwrap_or(text);
            let mut out = String::new();
            if f.is_alphabetic() && !is_protected_case_token(first_tok) {
                out.push(f.to_ascii_uppercase());
            } else {
                out.push(f);
            }
            out.push_str(chars.as_str());
            // Also capitalize weekdays inside (protected tokens stay untouched).
            apply_sentence_and_weekday_casing(&out, true)
        }
    }
}

/// Path, URL, and code-like tokens whose casing must not be rewritten by
/// sentence/weekday capitalisation (aligned with formatting protected spans).
fn is_protected_case_token(tok: &str) -> bool {
    let t = tok
        .trim_end_matches([',', '.', '!', '?', ';', ')', ']', '}', '"', '\''])
        .trim_start_matches(['(', '[', '{', '"', '\'']);
    if t.is_empty() {
        return false;
    }
    if t.starts_with("http://") || t.starts_with("https://") {
        return true;
    }
    if t.starts_with("./") || t.starts_with("../") || t.starts_with("~/") {
        return true;
    }
    if t.starts_with('/') && t.len() > 1 {
        return true;
    }
    // crates/…, src/lib.rs, etc.
    if t.contains('/') && t.starts_with(|c: char| c.is_alphanumeric() || c == '.') {
        return true;
    }
    // Rust paths / qualified identifiers.
    if t.contains("::") {
        return true;
    }
    // CLI flags (`--workspace`, `-v`).
    if t.starts_with("--") {
        return true;
    }
    if t.starts_with('-') && t.len() > 1 {
        let b = t.as_bytes()[1];
        if b.is_ascii_alphanumeric() {
            return true;
        }
    }
    false
}

// ─── Token helpers ───────────────────────────────────────────────────────────

fn word_tokens(text: &str) -> Vec<(usize, usize, &str)> {
    let mut out = Vec::new();
    let mut char_iter = text.char_indices().peekable();
    while let Some((start, ch)) = char_iter.next() {
        if ch.is_whitespace() {
            continue;
        }
        let mut end = start + ch.len_utf8();
        while let Some(&(next_i, next_ch)) = char_iter.peek() {
            if next_ch.is_whitespace() {
                break;
            }
            end = next_i + next_ch.len_utf8();
            char_iter.next();
        }
        out.push((start, end, &text[start..end]));
    }
    out
}

fn join_tokens(tokens: &[(usize, usize, &str)]) -> String {
    tokens
        .iter()
        .map(|(_, _, t)| *t)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Join quote interior tokens. A trailing comma on the last interior word is
/// cue-adjacent STT glue (`quote, leave this, unquote`), not list punctuation.
fn join_quote_interior(tokens: &[(usize, usize, &str)]) -> String {
    if tokens.is_empty() {
        return String::new();
    }
    let mut parts: Vec<&str> = tokens.iter().map(|(_, _, t)| *t).collect();
    if let Some(last) = parts.last_mut() {
        if let Some(stripped) = last.strip_suffix(',') {
            *last = stripped;
        }
    }
    parts.join(" ")
}

fn ascii_lower(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_uppercase() {
                c.to_ascii_lowercase()
            } else {
                c
            }
        })
        .collect()
}

fn eq_ascii_ignore_case(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .all(|(x, y)| x.eq_ignore_ascii_case(&y))
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::path::PathBuf;
    use std::time::Instant;

    fn natural_opts() -> LocalBaselineOptions {
        LocalBaselineOptions {
            policy: RenderingPolicy::Natural,
            route: RenderingRoute::DeterministicLocal,
            timing: None,
        }
    }

    fn adaptive_opts() -> LocalBaselineOptions {
        LocalBaselineOptions {
            policy: RenderingPolicy::Adaptive,
            route: RenderingRoute::DeterministicLocal,
            timing: None,
        }
    }

    fn structured_opts() -> LocalBaselineOptions {
        LocalBaselineOptions {
            policy: RenderingPolicy::Structured,
            route: RenderingRoute::DeterministicLocal,
            timing: None,
        }
    }

    fn literal_opts() -> LocalBaselineOptions {
        LocalBaselineOptions {
            policy: RenderingPolicy::Adaptive,
            route: RenderingRoute::LiteralIdentity,
            timing: None,
        }
    }

    fn policy_from(s: &str) -> RenderingPolicy {
        RenderingPolicy::parse(s).unwrap_or(RenderingPolicy::Natural)
    }

    fn route_from(s: &str) -> RenderingRoute {
        RenderingRoute::parse(s).unwrap_or(RenderingRoute::DeterministicLocal)
    }

    #[test]
    fn literal_identity_is_identity() {
        let src = "run cargo test --workspace -- --test-threads=4";
        let b = organize_local_baseline(src, &literal_opts());
        assert_eq!(b.rendered(), src);
    }

    #[test]
    fn spoken_dash_dash_converts_in_literal_identity() {
        let b = organize_local_baseline("cargo test dash dash workspace", &literal_opts());
        assert_eq!(b.rendered(), "cargo test --workspace");
    }

    #[test]
    fn spoken_slash_and_dot_convert_in_literal_identity() {
        let b = organize_local_baseline(
            "create slash voisu core slash s r c slash lib dot rs",
            &literal_opts(),
        );
        assert_eq!(b.rendered(), "create/voisu core/s r c/lib.rs");
    }

    #[test]
    fn spoken_colon_slash_slash_converts_in_literal_identity() {
        let b = organize_local_baseline(
            "https colon slash slash example dot test slash a",
            &literal_opts(),
        );
        assert_eq!(b.rendered(), "https://example.test/a");
    }

    #[test]
    fn spoken_quote_unquote_strips_cue_comma_in_literal_identity() {
        let b = organize_local_baseline("quote, leave this, unquote", &literal_opts());
        assert_eq!(b.rendered(), "\"leave this\"");
    }

    #[test]
    fn unpaired_quote_stays_words_in_literal_identity() {
        let b = organize_local_baseline("quote leave this", &literal_opts());
        assert!(
            b.rendered().contains("quote"),
            "unpaired quote must stay a word, got {:?}",
            b.rendered()
        );
        assert!(
            !b.rendered().contains('"'),
            "unpaired quote must not invent a quote mark, got {:?}",
            b.rendered()
        );
    }

    #[test]
    fn unspoken_url_scheme_is_not_invented() {
        let b = organize_local_baseline("look at example test", &literal_opts());
        assert_eq!(b.rendered(), "look at example test");
        assert!(
            !b.rendered().contains("://"),
            "must not invent :// when those words were not said, got {:?}",
            b.rendered()
        );
    }

    #[test]
    fn spoken_dash_dash_converts_under_adaptive() {
        let b = organize_local_baseline("cargo test dash dash workspace", &adaptive_opts());
        assert_eq!(b.rendered(), "cargo test --workspace");
    }

    #[test]
    fn spoken_dash_dash_converts_inside_structured_goal() {
        let b = organize_local_baseline(
            "goal run cargo test dash dash workspace",
            &structured_opts(),
        );
        assert!(
            b.rendered().contains("--workspace"),
            "Structured Goal must still convert dash dash, got {:?}",
            b.rendered()
        );
        assert!(
            !b.rendered().to_ascii_lowercase().contains("dash"),
            "spoken dash must not remain after Structured organize, got {:?}",
            b.rendered()
        );
    }

    #[test]
    fn mixed_prose_with_dot_still_sentence_polishes() {
        let b = organize_local_baseline("please send this to example dot com", &adaptive_opts());
        assert_eq!(b.rendered(), "Please send this to example.com.");
    }

    #[test]
    fn spoken_lone_dash_converts_in_literal_identity() {
        let b = organize_local_baseline("git dash C status", &literal_opts());
        assert_eq!(b.rendered(), "git -C status");
    }

    #[test]
    fn ordinary_dash_of_noun_stays_a_word() {
        let b = organize_local_baseline("add a dash of salt", &adaptive_opts());
        assert_eq!(b.rendered(), "Add a dash of salt.");
        assert!(!b.rendered().contains('-'));
    }

    #[test]
    fn spoken_dash_dash_converts_inside_preformatted_list() {
        let src = "1. Run cargo test dash dash workspace\n2. Report results";
        let b = organize_local_baseline(src, &literal_opts());
        assert_eq!(
            b.rendered(),
            "1. Run cargo test --workspace\n2. Report results"
        );
    }

    #[test]
    fn grocery_comma_list_is_not_a_quote_conversion() {
        let b = organize_local_baseline("Cup, milk, eggs, bread", &literal_opts());
        assert_eq!(b.rendered(), "Cup, milk, eggs, bread");
        assert!(!b.rendered().contains('"'));
    }

    #[test]
    fn spoken_first_second_third_becomes_numbered_lines() {
        let src = "first do the deployment second figure out the env variable third report to me";
        let expect = "1. Do the deployment\n2. Figure out the env variable\n3. Report to me";
        for opts in [adaptive_opts(), natural_opts(), structured_opts()] {
            let b = organize_local_baseline(src, &opts);
            assert_eq!(
                b.rendered(),
                expect,
                "policy={:?} route={:?}",
                opts.policy,
                opts.route
            );
        }
    }

    #[test]
    fn the_first_time_stays_a_sentence() {
        let b = organize_local_baseline("The first time I tried this", &adaptive_opts());
        assert_eq!(b.rendered(), "The first time I tried this.");
        assert!(
            !b.rendered().contains("1."),
            "ordinary first-time speech must not become a list, got {:?}",
            b.rendered()
        );
    }

    fn assert_not_numbered_list(src: &str, rendered: &str) {
        assert!(
            !rendered.contains("1.") && !rendered.contains("2.") && !rendered.contains("3."),
            "ordinary ordinal prose must stay a sentence, {src:?} → {rendered:?}"
        );
    }

    #[test]
    fn embedded_first_second_third_after_intro_becomes_numbered_lines() {
        // Minimized public stand-in for controlled human test 5 (no private WAV).
        let src = "I need you to take this in order. First do the deployment second figure out the env variable third report to me";
        let expect = "I need you to take this in order.\n1. Do the deployment\n2. Figure out the env variable\n3. Report to me";
        for opts in [adaptive_opts(), natural_opts(), structured_opts()] {
            let b = organize_local_baseline(src, &opts);
            assert_eq!(
                b.rendered(),
                expect,
                "policy={:?} route={:?}",
                opts.policy,
                opts.route
            );
            assert_opening_survives(src, b.rendered(), "I need you to take this in order");
            assert_eq!(
                non_cue_keys(src)
                    .into_iter()
                    .filter(|k| !matches!(k.as_str(), "first" | "second" | "third"))
                    .collect::<Vec<_>>(),
                non_cue_keys(b.rendered())
                    .into_iter()
                    .filter(|k| !matches!(k.as_str(), "1" | "2" | "3"))
                    .collect::<Vec<_>>(),
                "policy={:?}: intro or list body lost → {:?}",
                opts.policy,
                b.rendered()
            );
        }
    }

    #[test]
    fn embedded_list_after_spoken_period_or_break_keeps_intro() {
        let numbered = "1. Do the deployment\n2. Figure out the env variable\n3. Report to me";
        let cases = [
            (
                "here is the plan period first do the deployment second figure out the env variable third report to me",
                format!("Here is the plan.\n{numbered}"),
            ),
            (
                "here is the plan new line first do the deployment second figure out the env variable third report to me",
                format!("Here is the plan.\n{numbered}"),
            ),
            (
                "here is the plan new paragraph first do the deployment second figure out the env variable third report to me",
                format!("Here is the plan.\n\n{numbered}"),
            ),
        ];
        for (src, expect) in cases {
            for opts in [adaptive_opts(), natural_opts(), structured_opts()] {
                let b = organize_local_baseline(src, &opts);
                assert_eq!(b.rendered(), expect, "src={src:?} policy={:?}", opts.policy);
                assert_opening_survives(src, b.rendered(), "here is the plan");
            }
        }
    }

    #[test]
    fn ordinary_ordinal_prose_stays_sentences() {
        let cases = [
            "The first time I tried this",
            "she finished first second and third in the race",
            "we met on the first of march then the second of april and the third of may",
            "I will remind you we are updating the docs first then we can talk",
            "I remember. The first time I tried this it failed",
            "I remember. First I tried this it failed. Later she finished second and the third of may we shipped",
            "The trip is booked. First of march then the second of april and the third of may",
            "Results. First was Alice second was Bob third was Carol",
        ];
        for src in cases {
            let b = organize_local_baseline(src, &adaptive_opts());
            assert_not_numbered_list(src, b.rendered());
            assert_eq!(
                non_cue_keys(src),
                non_cue_keys(b.rendered()),
                "ordinal prose lost tokens: {src:?} → {:?}",
                b.rendered()
            );
        }
    }

    #[test]
    fn missing_second_or_third_boundary_stays_prose() {
        let cases = [
            (
                "I need you to take this in order. First do the deployment second figure out the env variable",
                "take this in order",
            ),
            (
                "I need you to take this in order. First do the deployment third report to me",
                "take this in order",
            ),
            (
                "here is the plan. first do the deployment",
                "here is the plan",
            ),
        ];
        for (src, opening) in cases {
            let b = organize_local_baseline(src, &adaptive_opts());
            assert_not_numbered_list(src, b.rendered());
            assert_opening_survives(src, b.rendered(), opening);
        }
    }

    #[test]
    fn ordinal_words_without_boundary_do_not_authorize_a_list() {
        let src =
            "okay first do the deployment second figure out the env variable third report to me";
        let b = organize_local_baseline(src, &adaptive_opts());
        assert_eq!(
            b.rendered(),
            "Okay first do the deployment second figure out the env variable third report to me."
        );
        assert_not_numbered_list(src, b.rendered());

        let dictate = "now I will dictate a list first do the deployment second figure out the env variable third report to me";
        let kept = organize_local_baseline(dictate, &adaptive_opts());
        assert_eq!(
            kept.rendered(),
            "Now I will dictate a list first do the deployment second figure out the env variable third report to me."
        );
        assert_not_numbered_list(dictate, kept.rendered());
    }

    #[test]
    fn the_first_time_then_bounded_list_keeps_intro() {
        let src = "The first time I tried this it failed. First do the deployment second figure out the env variable third report to me";
        let expect = "The first time I tried this it failed.\n1. Do the deployment\n2. Figure out the env variable\n3. Report to me";
        let b = organize_local_baseline(src, &adaptive_opts());
        assert_eq!(b.rendered(), expect);
        assert_opening_survives(src, b.rendered(), "The first time I tried this");
    }

    #[test]
    fn embedded_list_stays_literal_unless_spoken_marks() {
        let src = "I need you to take this in order. First do the deployment second figure out the env variable third report to me";
        let literal = organize_local_baseline(src, &literal_opts());
        assert_eq!(literal.rendered(), src);
        assert_not_numbered_list(src, literal.rendered());

        let with_break = "here is the plan new line first do the deployment second figure out the env variable third report to me";
        let broken = organize_local_baseline(with_break, &literal_opts());
        assert_eq!(
            broken.rendered(),
            "here is the plan.\nfirst do the deployment second figure out the env variable third report to me"
        );
        assert_not_numbered_list(with_break, broken.rendered());
    }

    #[test]
    fn punctuated_spoken_items_become_numbered_lines() {
        let src = "Take this. First do X. Second do Y. Third do Z.";
        let b = organize_local_baseline(src, &adaptive_opts());
        assert_eq!(b.rendered(), "Take this.\n1. Do X.\n2. Do Y.\n3. Do Z.");
        assert_opening_survives(src, b.rendered(), "Take this");
    }

    fn adaptive_opts_with_timing(
        certainty: TimingCertainty,
        left: &str,
        right: &str,
    ) -> LocalBaselineOptions {
        LocalBaselineOptions {
            policy: RenderingPolicy::Adaptive,
            route: RenderingRoute::DeterministicLocal,
            timing: Some(LocalTiming {
                certainty,
                boundaries: vec![PauseBoundary {
                    left_phrase: left.to_owned(),
                    right_phrase: right.to_owned(),
                    pause_ms: 720,
                }],
            }),
        }
    }

    #[test]
    fn clear_pause_before_first_authorizes_a_list() {
        let src = "I need you to take this in order first do the deployment second figure out the env variable third report to me";
        let without = organize_local_baseline(src, &adaptive_opts());
        assert_not_numbered_list(src, without.rendered());

        let with_pause = organize_local_baseline(
            src,
            &adaptive_opts_with_timing(TimingCertainty::Clear, "in order", "first do"),
        );
        assert_eq!(
            with_pause.rendered(),
            "I need you to take this in order.\n1. Do the deployment\n2. Figure out the env variable\n3. Report to me"
        );
        assert_opening_survives(
            src,
            with_pause.rendered(),
            "I need you to take this in order",
        );

        let uncertain = organize_local_baseline(
            src,
            &adaptive_opts_with_timing(TimingCertainty::Uncertain, "in order", "first do"),
        );
        assert_not_numbered_list(src, uncertain.rendered());
    }

    #[test]
    fn grocery_comma_list_stays_a_sentence() {
        let b = organize_local_baseline("Cup, milk, eggs, bread", &adaptive_opts());
        assert_eq!(b.rendered(), "Cup, milk, eggs, bread.");
        assert!(
            !b.rendered().contains("1."),
            "grocery comma list must not become numbered lines, got {:?}",
            b.rendered()
        );
    }

    #[test]
    fn new_line_and_new_paragraph_still_convert_in_literal_identity() {
        let line =
            organize_local_baseline("first thought new line second thought", &literal_opts());
        assert_eq!(line.rendered(), "first thought.\nsecond thought");
        let para = organize_local_baseline("intro new paragraph body text", &literal_opts());
        assert_eq!(para.rendered(), "intro.\n\nbody text");
    }

    #[test]
    fn spoken_dash_dash_converts_in_metalinguistic_span() {
        let b = organize_local_baseline("say the words dash dash out loud", &literal_opts());
        assert!(
            b.rendered().contains("--"),
            "dash dash must convert even in a words-span, got {:?}",
            b.rendered()
        );
        assert!(
            !b.rendered().to_ascii_lowercase().contains("dash"),
            "spoken dash must not remain a word, got {:?}",
            b.rendered()
        );
    }

    #[test]
    fn spoken_marks_oracles_hold_under_every_policy() {
        let cases = [
            ("cargo test dash dash workspace", "cargo test --workspace"),
            (
                "create slash voisu core slash s r c slash lib dot rs",
                "create/voisu core/s r c/lib.rs",
            ),
            (
                "https colon slash slash example dot test slash a",
                "https://example.test/a",
            ),
            ("quote, leave this, unquote", "\"leave this\""),
        ];
        for (src, expect) in cases {
            for opts in [
                literal_opts(),
                adaptive_opts(),
                natural_opts(),
                structured_opts(),
            ] {
                let got = organize_local_baseline(src, &opts);
                assert_eq!(
                    got.rendered(),
                    expect,
                    "src={src:?} policy={:?} route={:?}",
                    opts.policy,
                    opts.route
                );
            }
        }
    }

    #[test]
    fn preformatted_list_identity() {
        let src = "1. Build\n2. Test\n3. Ship";
        let b = organize_local_baseline(src, &adaptive_opts());
        assert_eq!(b.rendered(), src);
    }

    #[test]
    fn empty_source_empty_baseline() {
        assert_eq!(organize_local_baseline("", &adaptive_opts()).rendered(), "");
    }

    #[test]
    fn natural_never_emits_structured_headers() {
        let src = "goal fix the flaky auth test";
        let b = organize_local_baseline(src, &natural_opts());
        assert!(!b.rendered().contains("Goal:"));
        assert_eq!(b.rendered(), "Goal fix the flaky auth test.");
    }

    #[test]
    fn structured_does_not_emit_goal_heading() {
        let src = "goal fix the flaky auth test";
        let b = organize_local_baseline(src, &structured_opts());
        assert!(!b.rendered().contains("Goal:"));
        assert_eq!(b.rendered(), "Goal fix the flaky auth test.");
    }

    #[test]
    fn spoken_goal_is_not_a_local_heading() {
        let src = "Goal is to deploy the application right now";
        for opts in [adaptive_opts(), natural_opts(), structured_opts()] {
            let b = organize_local_baseline(src, &opts);
            assert_eq!(
                b.rendered(),
                "Goal is to deploy the application right now.",
                "policy={:?} route={:?}",
                opts.policy,
                opts.route
            );
            assert!(!b.rendered().contains("Goal:"));
        }
    }

    #[test]
    fn leftover_admits_goal_and_mixed_structured_notes() {
        let goal_is = organize_local_baseline(
            "Goal is to deploy the application right now",
            &adaptive_opts(),
        );
        assert_eq!(
            goal_is.rendered(),
            "Goal is to deploy the application right now."
        );
        assert!(leftover_admits_format_cloud(goal_is.rendered()));

        let leading_goal = organize_local_baseline("goal ship the rust parser", &adaptive_opts());
        assert_eq!(leading_goal.rendered(), "Goal ship the rust parser.");
        assert!(leftover_admits_format_cloud(leading_goal.rendered()));

        let mixed = organize_local_baseline(
            "goal is to deploy the application right now context is the production cluster notes check the rollback",
            &adaptive_opts(),
        );
        assert_eq!(
            mixed.rendered(),
            "Goal is to deploy the application right now. Context is the production cluster. Notes check the rollback."
        );
        assert!(leftover_admits_format_cloud(mixed.rendered()));
    }

    fn rec_372011_groq_source() -> String {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/rec-372011-9-1787154075295-groq.txt");
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    fn lexical_key(tok: &str) -> Option<String> {
        let core = tok.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        if core.is_empty() {
            None
        } else {
            Some(ascii_lower(core))
        }
    }

    fn non_cue_keys(text: &str) -> Vec<String> {
        const CUES: &[&str] = &[
            "goal",
            "context",
            "notes",
            "steps",
            "files",
            "requirements",
            "constraints",
            "acceptance",
            "criteria",
        ];
        const ORDINALS: &[&str] = &["one", "two", "three", "four", "1", "2", "3", "4"];
        word_tokens(text)
            .into_iter()
            .filter_map(|(_, _, t)| lexical_key(t))
            .filter(|k| !CUES.contains(&k.as_str()) && !ORDINALS.contains(&k.as_str()))
            .collect()
    }

    fn assert_opening_survives(src: &str, rendered: &str, opening: &str) {
        let lower = ascii_lower(rendered);
        assert!(
            lower.contains(&ascii_lower(opening)),
            "opening {opening:?} lost from source {src:?} → {rendered:?}"
        );
        assert!(
            !lower.trim_start().starts_with("goal "),
            "false Goal section ate the prefix: {rendered:?}"
        );
        let src_n = word_tokens(src).len();
        let out_n = word_tokens(rendered).len();
        assert!(
            out_n + 2 >= src_n,
            "word count collapsed by deleting the prefix: {src_n} → {out_n} ({rendered:?})"
        );
    }

    #[test]
    fn controlled_rec_372011_groq_source_keeps_opening_prefix() {
        // rec-372011-9-1787154075295 Groq Source Transcript.
        let src = rec_372011_groq_source();
        assert!(
            src.starts_with("This paragraph deliberately"),
            "fixture must keep the Groq opening, got {:?}",
            &src[..src.len().min(80)]
        );
        for opts in [adaptive_opts(), natural_opts(), structured_opts()] {
            let b = organize_local_baseline(&src, &opts);
            assert_opening_survives(&src, b.rendered(), "This paragraph deliberately");
            assert_eq!(
                non_cue_keys(&src),
                non_cue_keys(b.rendered()),
                "policy={:?}: non-cue tokens missing, duplicated, or reordered → {:?}",
                opts.policy,
                b.rendered()
            );
        }
    }

    #[test]
    fn historical_rec_115529_class_keeps_prose_prefix() {
        // rec-115529-8-1786612351451 deletion class (minimized public wording).
        let src = "ok so I have finished the writeup and I will remind you we are updating the docs first then we can talk about the goal during the review and the context for the notes that stay in this paragraph";
        for opts in [adaptive_opts(), natural_opts(), structured_opts()] {
            let b = organize_local_baseline(src, &opts);
            assert_opening_survives(src, b.rendered(), "ok so I have finished");
            assert!(
                ascii_lower(b.rendered()).contains("updating the docs first"),
                "policy={:?}: prefix prose dropped → {:?}",
                opts.policy,
                b.rendered()
            );
        }
    }

    #[test]
    fn mid_sentence_cue_nouns_stay_prose_and_keep_prefix() {
        let src = "The gardener explained that his goal was restoration and the context included flooding while his notes mentioned birds and the practical steps involved planting because project files included a map and funding requirements prohibited herbicides and physical constraints included a narrow road so acceptance criteria stayed a phrase in this sentence";
        for opts in [adaptive_opts(), natural_opts(), structured_opts()] {
            let b = organize_local_baseline(src, &opts);
            assert_opening_survives(src, b.rendered(), "The gardener explained");
            let lower = ascii_lower(b.rendered());
            for noun in [
                "goal",
                "context",
                "notes",
                "steps",
                "files",
                "requirements",
                "constraints",
                "acceptance",
                "criteria",
            ] {
                assert!(
                    lower.contains(noun),
                    "policy={:?}: cue-shaped noun {noun:?} lost → {:?}",
                    opts.policy,
                    b.rendered()
                );
            }
            assert!(
                !b.rendered().contains("Goal:"),
                "mid-sentence nouns must stay prose, got {:?}",
                b.rendered()
            );
        }
    }

    fn assert_no_false_section_headers(policy: RenderingPolicy, rendered: &str) {
        for header in [
            "Context:",
            "Notes:",
            "Steps:",
            "Files:",
            "Requirements:",
            "Constraints:",
        ] {
            assert!(
                !rendered.contains(header),
                "policy={policy:?}: false section header {header:?} → {rendered:?}"
            );
        }
    }

    #[test]
    fn opening_structure_cue_does_not_split_later_prose_cue_nouns() {
        let gardener = "goal keep the complete transcript without truncation. The gardener explained that his goal was to restore native plants. The ecological context included flooding. His field notes mentioned three bird species and one damaged fence. The practical steps involved planting. Your project files included a map. The funding requirements prohibited herbicides. The physical constraints included a narrow road.";
        let and_context = "goal keep the complete transcript without truncation. The gardener explained the restoration and context included flooding.";
        let several_constraints = "goal keep the complete transcript without truncation. The municipal contract listed several constraints on the site.";
        let cases: &[(&str, &[&str])] = &[
            (
                gardener,
                &[
                    "ecological context",
                    "field notes",
                    "practical steps",
                    "project files",
                    "three",
                    "files",
                ],
            ),
            (and_context, &["and context"]),
            (several_constraints, &["several constraints"]),
        ];
        for (src, phrases) in cases {
            for opts in [adaptive_opts(), natural_opts(), structured_opts()] {
                let b = organize_local_baseline(src, &opts);
                let rendered = b.rendered();
                assert_no_false_section_headers(opts.policy, rendered);
                let lower = ascii_lower(rendered);
                for phrase in *phrases {
                    assert!(
                        lower.contains(phrase),
                        "policy={:?}: {phrase:?} split or dropped → {rendered:?}",
                        opts.policy
                    );
                }
            }
        }
    }

    #[test]
    fn first_structure_cue_must_begin_the_utterance() {
        let dictated = "um uh goal fix the flaky auth test context it fails on CI only";
        let organized = organize_local_baseline(dictated, &adaptive_opts());
        assert_eq!(
            organized.rendered(),
            "Goal fix the flaky auth test. Context it fails on CI only."
        );

        let prose = "We talked about the goal during lunch and the context changed";
        let kept = organize_local_baseline(prose, &adaptive_opts());
        assert_eq!(
            kept.rendered(),
            "We talked about the goal during lunch and the context changed."
        );
        assert!(
            !ascii_lower(kept.rendered())
                .trim_start()
                .starts_with("goal "),
            "bare cue-shaped nouns inside prose must stay literal, got {:?}",
            kept.rendered()
        );
    }

    #[test]
    fn organized_non_cue_tokens_survive_exactly_once() {
        let src = "goal fix the flaky auth test context it fails on CI only requirements keep public API stable constraints no new dependencies steps one reproduce two isolate three fix four verify acceptance criteria CI is green on main files crates/voisu-core/src/auth.rs notes keep the change small";
        for opts in [adaptive_opts(), natural_opts(), structured_opts()] {
            let b = organize_local_baseline(src, &opts);
            assert_eq!(
                non_cue_keys(src),
                non_cue_keys(b.rendered()),
                "policy={:?}: non-cue tokens missing, duplicated, or reordered → {:?}",
                opts.policy,
                b.rendered()
            );
        }

        let prefix = "Please keep this prefix before the goal during lunch and the notes after";
        let rolled = organize_local_baseline(prefix, &adaptive_opts());
        assert_eq!(
            rolled.rendered(),
            "Please keep this prefix before the goal during lunch and the notes after."
        );
        assert_eq!(non_cue_keys(prefix), non_cue_keys(rolled.rendered()));
    }

    #[test]
    fn genuine_multi_section_dictation_still_organizes() {
        let src = "goal fix the flaky auth test context it fails on CI only requirements keep public API stable constraints no new dependencies steps one reproduce two isolate three fix four verify acceptance criteria CI is green on main files crates/voisu-core/src/auth.rs notes keep the change small";
        let expect = "Goal fix the flaky auth test. Context it fails on CI only. Requirements keep public API stable. Constraints no new dependencies.\n1. Reproduce\n2. Isolate\n3. Fix\n4. Verify\nAcceptance criteria CI is green on main. Files crates/voisu-core/src/auth.rs. Notes keep the change small.";
        for opts in [adaptive_opts(), natural_opts()] {
            let b = organize_local_baseline(src, &opts);
            assert_eq!(b.rendered(), expect, "policy={:?}", opts.policy);
        }
        let structured = organize_local_baseline(src, &structured_opts());
        let structured_lower = ascii_lower(structured.rendered());
        assert!(
            structured_lower.contains("flaky auth test"),
            "Structured dictation dropped the Goal body: {:?}",
            structured.rendered()
        );
        assert!(
            structured_lower.contains("keep the change small"),
            "Structured dictation dropped the Notes body: {:?}",
            structured.rendered()
        );
        assert!(
            structured.rendered().contains("1. Reproduce"),
            "Structured dictation lost numbered steps: {:?}",
            structured.rendered()
        );
    }

    #[test]
    fn genuine_section_bodies_keep_content_cue_shaped_words() {
        let src = "goal fix the flaky auth test context it fails on CI only notes keep the files and mention three remaining tests";
        for opts in [adaptive_opts(), natural_opts(), structured_opts()] {
            let b = organize_local_baseline(src, &opts);
            let rendered = b.rendered();
            let lower = ascii_lower(rendered);
            assert!(
                lower.contains("flaky auth test"),
                "policy={:?}: genuine Goal body lost → {rendered:?}",
                opts.policy
            );
            assert!(
                lower.contains("files"),
                "policy={:?}: content files dropped → {rendered:?}",
                opts.policy
            );
            assert!(
                lower.contains("three"),
                "policy={:?}: content three dropped → {rendered:?}",
                opts.policy
            );
            assert!(
                !rendered.contains("Files:"),
                "policy={:?}: content files became a header → {rendered:?}",
                opts.policy
            );
        }
    }

    #[test]
    fn leftover_rejects_local_jobs_and_ordinary_chat() {
        let dash_dash = organize_local_baseline("cargo test dash dash workspace", &adaptive_opts());
        assert_eq!(dash_dash.rendered(), "cargo test --workspace");
        assert!(!leftover_admits_format_cloud(dash_dash.rendered()));

        let numbered = organize_local_baseline(
            "first do the deployment second figure out the env variable third report to me",
            &adaptive_opts(),
        );
        assert_eq!(
            numbered.rendered(),
            "1. Do the deployment\n2. Figure out the env variable\n3. Report to me"
        );
        assert!(!leftover_admits_format_cloud(numbered.rendered()));

        let grocery = organize_local_baseline("Cup, milk, eggs, bread", &adaptive_opts());
        assert_eq!(grocery.rendered(), "Cup, milk, eggs, bread.");
        assert!(!leftover_admits_format_cloud(grocery.rendered()));

        let first_time = organize_local_baseline("The first time I tried this", &adaptive_opts());
        assert_eq!(first_time.rendered(), "The first time I tried this.");
        assert!(!leftover_admits_format_cloud(first_time.rendered()));

        let ordinary = organize_local_baseline("pls send the notes when you can", &adaptive_opts());
        assert_eq!(ordinary.rendered(), "Pls send the notes when you can.");
        assert!(!leftover_admits_format_cloud(ordinary.rendered()));
    }

    #[test]
    fn r1_uncertain_backtrack_preserves_all_words() {
        let src = "send it friday actually monday";
        let b = organize_local_baseline(src, &adaptive_opts());
        let lower = b.rendered().to_ascii_lowercase();
        assert!(lower.contains("friday"));
        assert!(lower.contains("actually"));
        assert!(lower.contains("monday"));
        assert_eq!(b.rendered(), "Send it Friday actually Monday.");
    }

    #[test]
    fn r1_clear_backtrack_no_wait_is_justified() {
        // Corpus DPR-24: single-token left after a function word ("it").
        let src = "send it friday no wait monday";
        let b = organize_local_baseline(src, &adaptive_opts());
        assert_eq!(b.rendered(), "Send it Monday.");
        assert!(!b.rendered().to_ascii_lowercase().contains("friday"));
        assert!(!b.rendered().to_ascii_lowercase().contains("wait"));
    }

    #[test]
    fn r1_clear_backtrack_leading_single_token() {
        // Isolated / leading single-token X — full correction span is proven.
        let src = "foo no wait bar";
        let b = organize_local_baseline(src, &adaptive_opts());
        assert_eq!(b.rendered(), "Bar.");
        assert!(!b.rendered().to_ascii_lowercase().contains("foo"));
        assert!(!b.rendered().to_ascii_lowercase().contains("wait"));
    }

    #[test]
    fn r1_multiword_left_backtrack_preserves_all_words() {
        // R1: partial delete of multi-word left ("new york") is unsafe.
        // Must not become "send it new london".
        let src = "send it new york no wait london";
        let b = organize_local_baseline(src, &adaptive_opts());
        let lower = b.rendered().to_ascii_lowercase();
        assert!(
            lower.contains("new") && lower.contains("york") && lower.contains("london"),
            "multi-word left must preserve all words, got {:?}",
            b.rendered()
        );
        assert!(
            !lower.contains("new london") && lower.contains("york"),
            "must not partially delete york only → {:?}",
            b.rendered()
        );
        // Full fail-closed: marker words stay too.
        assert!(lower.contains("no") && lower.contains("wait"));
    }

    #[test]
    fn r1_incomplete_backtrack_provenance_keeps_words() {
        // Missing replacement token after `no wait` — fail closed (keep all words).
        let src = "send it friday no wait";
        let b = organize_local_baseline(src, &adaptive_opts());
        let lower = b.rendered().to_ascii_lowercase();
        assert!(lower.contains("friday"));
        assert!(lower.contains("no"));
        assert!(lower.contains("wait"));

        // Non-alphabetic side → fail closed.
        let src2 = "send it 2 no wait 3";
        let b2 = organize_local_baseline(src2, &adaptive_opts());
        let lower2 = b2.rendered().to_ascii_lowercase();
        assert!(lower2.contains("2"));
        assert!(lower2.contains("no"));
        assert!(lower2.contains("wait"));
        assert!(lower2.contains("3"));
    }

    #[test]
    fn leading_clear_fillers_removed_content_fillers_kept() {
        let b = organize_local_baseline("um uh I think we should ship friday", &adaptive_opts());
        assert_eq!(b.rendered(), "I think we should ship Friday.");

        let kept = organize_local_baseline(
            "type um then continue and do not strip filler words like uh",
            &adaptive_opts(),
        );
        assert_eq!(
            kept.rendered(),
            "Type um then continue and do not strip filler words like uh."
        );
    }

    #[test]
    fn metalinguistic_symbol_mentions_stay_words() {
        let b = organize_local_baseline(
            "say the words period and new line out loud",
            &adaptive_opts(),
        );
        assert_eq!(b.rendered(), "Say the words period and new line out loud.");
        assert!(!b.rendered().contains('\n'));
    }

    #[test]
    fn ordinary_noun_period_not_a_cue() {
        let b = organize_local_baseline(
            "the period of the moon is twenty seven days",
            &adaptive_opts(),
        );
        assert_eq!(b.rendered(), "The period of the moon is twenty seven days.");
    }

    #[test]
    fn dual_stt_complementary_merged_input_unit() {
        // DPR-33: merge is external; organizer sees the merged selected source.
        let merged = "open crates/voisu-core/src/lib.rs and check correlation_id";
        let b = organize_local_baseline(merged, &adaptive_opts());
        assert_eq!(
            b.rendered(),
            "Open crates/voisu-core/src/lib.rs and check correlation_id."
        );
    }

    #[test]
    fn no_invented_words_property() {
        let samples = [
            "hey can you send the notes when you get a chance",
            "do not enable the feature flag in production",
            "file the bug in voisu not voice so",
            "clone https://github.com/Anuraj-dev/voisu",
            "there is two issues with the patch",
        ];
        for src in samples {
            let out = organize_local_baseline(src, &adaptive_opts());
            let src_words: Vec<String> = word_tokens(src)
                .into_iter()
                .map(|(_, _, t)| ascii_lower(t))
                .collect();
            for (_, _, t) in word_tokens(out.rendered()) {
                // Strip punctuation glued by casing/vocative (`Hey,` → `hey`).
                let core = t.trim_matches(|c: char| !c.is_ascii_alphanumeric());
                if core.is_empty() {
                    continue;
                }
                let lower = ascii_lower(core);
                if lower.chars().any(|c| c.is_ascii_alphabetic()) {
                    assert!(
                        src_words.iter().any(|s| {
                            let sc = s.trim_matches(|c: char| !c.is_ascii_alphanumeric());
                            ascii_lower(sc) == lower
                        }),
                        "invented word {t:?} from source {src:?} → {:?}",
                        out.rendered()
                    );
                }
            }
        }
    }

    #[test]
    fn protected_url_and_path_substrings_preserved() {
        let src = "clone https://github.com/Anuraj-dev/voisu and open crates/voisu-core/src/lib.rs";
        let b = organize_local_baseline(src, &adaptive_opts());
        assert!(b.rendered().contains("https://github.com/Anuraj-dev/voisu"));
        assert!(b.rendered().contains("crates/voisu-core/src/lib.rs"));
    }

    #[test]
    fn protected_path_at_utterance_start_keeps_case() {
        // Sentence-start casing must not rewrite path/URL/code-like tokens.
        let src = "crates/voisu-core/src/lib.rs is failing";
        let b = organize_local_baseline(src, &adaptive_opts());
        assert!(
            b.rendered().starts_with("crates/voisu-core/src/lib.rs"),
            "path case rewritten at utterance start: {:?}",
            b.rendered()
        );
        assert!(
            !b.rendered().starts_with("Crates/"),
            "path first letter uppercased: {:?}",
            b.rendered()
        );
    }

    #[test]
    fn local_path_leaves_headroom_under_delivery_deadline() {
        let src = "goal fix the flaky auth test context it fails on CI only requirements keep public API stable constraints no new dependencies steps one reproduce two isolate three fix four verify acceptance criteria CI is green on main files crates/voisu-core/src/auth.rs notes keep the change small";
        let start = Instant::now();
        for _ in 0..200 {
            let _ = organize_local_baseline(src, &natural_opts());
        }
        let elapsed = start.elapsed();
        // 200 runs well under 1500ms on CI hosts; no sleep-based flakiness.
        assert!(
            elapsed.as_millis() < 1500,
            "local baseline too slow: {elapsed:?} for 200 runs"
        );
    }

    #[test]
    fn corpus_local_routes_match_local_baseline() {
        let path = corpus_path();
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let root: Value = serde_json::from_str(&raw).expect("corpus JSON");
        let fixtures = root["fixtures"].as_array().expect("fixtures array");

        let mut checked = 0usize;
        for fix in fixtures {
            let id = fix["id"].as_str().unwrap_or("");
            let route = fix["route"].as_str().unwrap_or("");
            if route != "deterministic_local" && route != "literal_identity" {
                continue;
            }
            let expected = fix["local_baseline"]
                .as_str()
                .unwrap_or_else(|| panic!("{id}: missing local_baseline"));
            let policy = policy_from(fix["policy"].as_str().unwrap_or("adaptive"));
            let route_e = route_from(route);

            let selected_provider = fix["source_selection"]["selected_provider"]
                .as_str()
                .unwrap_or("provider_a");
            let sources = fix["sources"].as_array().expect("sources");
            let mut source_text = None;
            for s in sources {
                if s["provider"].as_str() == Some(selected_provider) && s["available"] == true {
                    source_text = s["text"].as_str();
                    break;
                }
            }
            let selected_source = source_text.unwrap_or_else(|| panic!("{id}: no selected source"));
            let merged_source;
            let source = if fix["source_selection"]["reason"] == "safe_complementary_merge" {
                // Source selection owns the conservative merge; this harness
                // promotes the organizer half of DPR-33 without a deferral.
                merged_source =
                    "open crates/voisu-core/src/lib.rs and check correlation_id".to_owned();
                merged_source.as_str()
            } else {
                selected_source
            };

            let timing = parse_timing(fix);
            let opts = LocalBaselineOptions {
                policy,
                route: route_e,
                timing,
            };
            let got = organize_local_baseline(source, &opts);
            assert_eq!(
                got.rendered(),
                expected,
                "{id} ({}) failed\n  source: {source:?}\n  got:    {:?}\n  expect: {expected:?}",
                fix["title"].as_str().unwrap_or(""),
                got.rendered()
            );
            checked += 1;
        }
        // Sanity: most local fixtures must run (not an empty suite).
        assert!(
            checked == 38,
            "expected all 38 applicable local fixtures, checked {checked}"
        );
    }

    fn parse_timing(fix: &Value) -> Option<LocalTiming> {
        let t = fix.get("timing")?;
        if t.is_null() {
            return None;
        }
        let certainty = match t["certainty"].as_str()? {
            "clear" => TimingCertainty::Clear,
            "uncertain" => TimingCertainty::Uncertain,
            _ => return None,
        };
        let mut boundaries = Vec::new();
        if let Some(arr) = t["boundaries"].as_array() {
            for b in arr {
                boundaries.push(PauseBoundary {
                    left_phrase: b["left_phrase"].as_str().unwrap_or("").to_owned(),
                    right_phrase: b["right_phrase"].as_str().unwrap_or("").to_owned(),
                    pause_ms: b["pause_ms"].as_u64().unwrap_or(0) as u32,
                });
            }
        }
        Some(LocalTiming {
            certainty,
            boundaries,
        })
    }

    fn corpus_path() -> PathBuf {
        // crates/voisu-core → repo root
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/research/developer-prompt-rendering-behavior-corpus-2026-08-11.json")
            .canonicalize()
            .expect("corpus path")
    }
}
