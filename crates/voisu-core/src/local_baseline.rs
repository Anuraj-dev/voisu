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

use crate::prompt_rendering::{
    RenderingPolicy, RenderingRoute, TimingCertainty, CLOSED_STRUCTURED_LABELS,
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
/// - `literal_identity` → identity (no organize).
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

fn organize_impl(source: &str, options: &LocalBaselineOptions) -> String {
    if source.is_empty() {
        return String::new();
    }

    if options.route == RenderingRoute::LiteralIdentity {
        return source.to_owned();
    }

    // Already multi-line numbered / bullet preformatted → identity.
    if is_preformatted_list(source) {
        return source.to_owned();
    }

    let mut text = source.to_owned();

    // R1-safe removals first (operate on raw spoken words).
    text = strip_leading_clear_fillers(&text);
    text = apply_clear_backtrack(&text);

    // Structural: multi-section / steps / Structured single-section labels.
    if let Some(structured) = try_section_organize(&text, options.policy) {
        return structured;
    }

    // Bare spoken cues (period, new line, quote…unquote, …).
    text = apply_bare_spoken_cues(&text);

    // Clear pause → paragraph break; uncertain → single stream.
    if let Some(timing) = options.timing.as_ref() {
        if timing.certainty == TimingCertainty::Clear {
            text = apply_clear_pause_breaks(&text, timing);
        }
    }

    // Light punctuation / casing polish.
    text = apply_discourse_ok(&text);
    text = apply_vocative_hey(&text);
    // Unpaired bare `quote` (no `unquote`) leaves the following stream as words:
    // sentence-case the start, but do not weekday-capitalize inside that span (DPR-11).
    let weekday = !has_unpaired_quote_cue(source);
    text = apply_sentence_and_weekday_casing(&text, weekday);
    text = add_terminal_punctuation_dpr(&text);

    text
}

/// True when a bare `quote` token appears without a later `unquote`.
fn has_unpaired_quote_cue(text: &str) -> bool {
    let tokens = word_tokens(text);
    let mut open = false;
    for &(_, _, tok) in &tokens {
        let l = ascii_lower(tok);
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
    let structured_single =
        policy == RenderingPolicy::Structured && sections.len() == 1 && !only_steps;

    // Adaptive/Natural with a single non-steps section: Natural-shaped (no headers).
    if !multi && !only_steps && !structured_single {
        return None;
    }

    // Steps-only (DPR-05): numbered lines, no "Steps" header under any policy.
    if only_steps {
        let body_tokens = &tokens[sections[0].body_start..];
        return try_numbered_steps_body(body_tokens).map(|lines| lines.join("\n"));
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

    if policy == RenderingPolicy::Structured {
        Some(parts.join("\n\n"))
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
            } else if out
                .lines()
                .last()
                .is_some_and(starts_with_numbered_marker)
            {
                out.push('\n');
                out.push_str(part);
            } else {
                out.push(' ');
                out.push_str(part);
            }
        }
        // Capitalize weekday tokens inside the assembled Natural multi-section text.
        Some(apply_sentence_and_weekday_casing(&out, true))
    }
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
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < tokens.len() {
        if let Some((label, cue_len)) = match_section_cue(tokens, i) {
            out.push(SectionSpan {
                label,
                cue_start: i,
                body_start: i + cue_len,
            });
            i += cue_len;
        } else {
            i += 1;
        }
    }
    out
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

// ─── Bare spoken cues ────────────────────────────────────────────────────────

fn apply_bare_spoken_cues(text: &str) -> String {
    let tokens = word_tokens(text);
    if tokens.is_empty() {
        return text.to_owned();
    }

    // Pass 1: paired quote … unquote (protect interior from further cue conversion).
    let mut skip: Vec<bool> = vec![false; tokens.len()];
    let mut quote_pairs: Vec<(usize, usize)> = Vec::new();
    {
        let mut i = 0usize;
        while i < tokens.len() {
            if ascii_lower(tokens[i].2) == "quote" {
                if let Some(j) = (i + 1..tokens.len()).find(|&j| ascii_lower(tokens[j].2) == "unquote")
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
            let interior = join_tokens(&tokens[q + 1..u]);
            out.push_str(&interior);
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

        // Multi-word cues (longest first), unless metalinguistic.
        if !meta[i] {
            if phrase_match(&tokens, i, &["exclamation", "point"]) {
                push_space(&mut out, &mut pending_space);
                // Glue bang to previous word.
                trim_trailing_space(&mut out);
                out.push('!');
                i += 2;
                pending_space = true;
                continue;
            }
            if phrase_match(&tokens, i, &["new", "paragraph"]) {
                push_space(&mut out, &mut pending_space);
                trim_trailing_space(&mut out);
                // Ensure previous clause has period before break when it lacks punct.
                ensure_clause_period(&mut out);
                out.push_str("\n\n");
                i += 2;
                pending_space = false;
                continue;
            }
            if phrase_match(&tokens, i, &["new", "line"]) {
                push_space(&mut out, &mut pending_space);
                trim_trailing_space(&mut out);
                ensure_clause_period(&mut out);
                out.push('\n');
                i += 2;
                pending_space = false;
                continue;
            }
            if phrase_match(&tokens, i, &["period"]) && period_is_cue(&tokens, i) {
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

    out
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
    if trimmed.chars().last().is_some_and(|c| c.is_alphanumeric() || c == ')') {
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
    let left_toks: Vec<String> = left
        .split_whitespace()
        .map(ascii_lower)
        .collect();
    let right_toks: Vec<String> = right
        .split_whitespace()
        .map(ascii_lower)
        .collect();
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
    "monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday",
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
        } else if capitalize_weekdays
            && WEEKDAYS
                .iter()
                .any(|w| eq_ascii_ignore_case(core, w))
        {
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
                "will", "would", "can", "could", "should", "may", "might", "must", "think",
                "send", "ship", "meet", "file", "ignore", "enable", "returns", "works",
                "ping", "let", "aint", "missing", "open", "clone", "use", "keep", "type",
                "replace", "gonna", "please", "there",
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
    fn preformatted_list_identity() {
        let src = "1. Build\n2. Test\n3. Ship";
        let b = organize_local_baseline(src, &adaptive_opts());
        assert_eq!(b.rendered(), src);
    }

    #[test]
    fn empty_source_empty_baseline() {
        assert_eq!(
            organize_local_baseline("", &adaptive_opts()).rendered(),
            ""
        );
    }

    #[test]
    fn natural_never_emits_structured_headers() {
        let src = "goal fix the flaky auth test";
        let b = organize_local_baseline(src, &natural_opts());
        assert!(!b.rendered().contains("Goal:"));
        assert_eq!(b.rendered(), "Goal fix the flaky auth test.");
    }

    #[test]
    fn structured_emits_closed_goal_label() {
        let src = "goal fix the flaky auth test";
        let b = organize_local_baseline(src, &structured_opts());
        assert_eq!(b.rendered(), "Goal:\nFix the flaky auth test.");
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
        assert_eq!(
            b.rendered(),
            "Say the words period and new line out loud."
        );
        assert!(!b.rendered().contains('\n'));
    }

    #[test]
    fn ordinary_noun_period_not_a_cue() {
        let b = organize_local_baseline(
            "the period of the moon is twenty seven days",
            &adaptive_opts(),
        );
        assert_eq!(
            b.rendered(),
            "The period of the moon is twenty seven days."
        );
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
        let fixtures = root["fixtures"]
            .as_array()
            .expect("fixtures array");

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
                merged_source = "open crates/voisu-core/src/lib.rs and check correlation_id".to_owned();
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
