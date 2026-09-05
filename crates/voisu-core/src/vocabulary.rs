//! User-vocabulary constrained post-correction.
//!
//! After the final Transcript is chosen (post-reconciliation, post-selection),
//! exact-match substitutions from the USER's personal dictionary are applied to
//! the chosen text. The design is the slice-B2, constrained form of the parked
//! `dictionary-jargon-rescue` philosophy (internal/specs/2026-07-28-dictionary-jargon-rescue.md):
//! every substitution is
//!
//! * **user-owned** — only terms the user typed into their dictionary
//!   participate (the built-in developer glossary never corrects), and the
//!   dictionary term is the canonical written form;
//! * **exact-match** — the term's alphanumeric-run token sequence must appear
//!   verbatim (case-insensitively) as whole tokens in the Transcript. A
//!   mishearing with different letters never matches, so the correction cannot
//!   invert meaning by character arithmetic;
//! * **span-guarded** — matches inside spoken `quote … unquote` pairs or
//!   say-the-words (`metalinguistic`) spans are skipped via the same
//!   masking machinery the dictation-grammar pass uses;
//! * **never deleting** — a substitution replaces exactly the matched
//!   whole-token span with the term, whose alphanumeric run count equals the
//!   matched span's, so no word content is ever removed and no words are
//!   invented beyond the term itself;
//! * **confidence-gated** — when the final Transcript is the Deepgram source
//!   and Deepgram's word confidences are available for the matched span, a
//!   substitution is applied only when at least one word in the span is NOT
//!   confidently transcribed (any confidence below
//!   [`CONFIDENT_WORD_SKIP_THRESHOLD`]). A fully confident span is left alone:
//!   the provider heard it clearly and overriding it is the risky direction.
//!   When confidences are unavailable for the span — a Groq-sourced, merged,
//!   repaired, or reconstructed final Transcript, or a span whose words were
//!   not parsed — the substitution IS applied: the user explicitly asked for
//!   this vocabulary. That asymmetry is deliberate and documented.
//!
//! Casing is preserved, not imposed: the matched span's casing shape decides
//! how the canonical term is written back (lowercase → `term.to_lowercase()`,
//! UPPER → `term.to_uppercase()`, Title → term title-cased, mixed → the term
//! exactly as the user wrote it). A consistently-cased span therefore usually
//! round-trips unchanged; the substitution's visible effect is canonicalizing
//! inconsistent casing and rejoining punctuation-separated forms of hyphenated
//! or slashed terms (`daemon reload` → `daemon-reload`).
//!
//! The pass is idempotent and fail-closed by construction: it is a pure
//! function over already-loaded terms, and an empty user dictionary (or a
//! dictionary read failure, which the loader degrades to empty user terms with
//! a local diagnostic) yields byte-identical output.

use crate::local_baseline::{metalinguistic_mask, quote_pairs_and_skip, word_tokens};

/// Deepgram word confidence at or above which a word counts as confidently
/// transcribed. A matched span whose words are ALL at least this confident is
/// left untouched: the provider was sure, and overriding a confident hearing is
/// the risky direction. Any word below the threshold (or without a confidence)
/// keeps the substitution available.
pub(crate) const CONFIDENT_WORD_SKIP_THRESHOLD: f64 = 0.9;

/// Maximum number of user dictionary terms considered per Transcript. The
/// daemon hands the pipeline the user's terms in stored order, so beyond this
/// cap the LAST user terms are skipped — a bounded, documented cap that keeps
/// the scan cheap on adversarially large dictionaries.
pub(crate) const USER_VOCABULARY_TERM_LIMIT: usize = 200;

/// A user-vocabulary term made matchable: its canonical written form, its
/// alphanumeric-run token sequence, and the separator characters its written
/// form may tolerate between runs.
struct VocabularyTerm {
    original: String,
    runs: Vec<String>,
    separators: Vec<char>,
}

/// Applies user-vocabulary corrections to `text`.
///
/// `word_confidences` carries the Deepgram word-level confidence evidence for
/// the FINAL text (empty when unavailable — see the module docs for when the
/// caller supplies it). Words are `(word, confidence)` pairs in transcript
/// order; confidence values are clamped to `[0, 1]` semantics by the caller
/// (Deepgram reports 0..1).
pub(crate) fn apply_user_vocabulary(
    text: &str,
    terms: &[String],
    word_confidences: &[(String, f64)],
) -> String {
    if text.is_empty() {
        return text.to_owned();
    }
    let terms: Vec<VocabularyTerm> = terms
        .iter()
        .take(USER_VOCABULARY_TERM_LIMIT)
        .filter_map(|term| VocabularyTerm::parse(term))
        .collect();
    if terms.is_empty() {
        return text.to_owned();
    }

    // Span safety: byte ranges the correction must never rewrite — spoken
    // `quote … unquote` pairs (cues included) and say-the-words spans, using
    // the same masking machinery as the dictation-grammar pass.
    let masked = masked_ranges(text);
    let confidence_runs = confidence_runs(word_confidences);

    // Whole-token matches, longest-span-at-start first.
    let mut matches: Vec<Match> = Vec::new();
    let runs = alphanumeric_runs(text);
    for (term_index, term) in terms.iter().enumerate() {
        for start in 0..runs.len().saturating_sub(term.runs.len() - 1) {
            let end = start + term.runs.len();
            if !runs[start..end]
                .iter()
                .zip(&term.runs)
                .all(|(run, expected)| run.lowered == *expected)
            {
                continue;
            }
            let span_start = runs[start].start;
            let span_end = runs[end - 1].end;
            if !separators_allowed(text, &runs[start..end], &term.separators) {
                continue;
            }
            if masked
                .iter()
                .any(|range| span_start < range.end && range.start < span_end)
            {
                continue;
            }
            if confidently_transcribed(&runs[start..end], &confidence_runs) {
                continue;
            }
            let shape = casing_shape(&text[span_start..span_end]);
            matches.push(Match {
                start: span_start,
                end: span_end,
                replacement: shaped_term(&term.original, shape),
                order: term_index,
            });
        }
    }

    if matches.is_empty() {
        return text.to_owned();
    }
    matches.sort_by(|left, right| {
        left.start
            .cmp(&right.start)
            .then(right.end.cmp(&left.end))
            .then(left.order.cmp(&right.order))
    });
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for candidate in matches {
        if candidate.start < cursor {
            continue; // overlaps an already-accepted (leftmost-longest) match
        }
        out.push_str(&text[cursor..candidate.start]);
        out.push_str(&candidate.replacement);
        cursor = candidate.end;
    }
    out.push_str(&text[cursor..]);
    out
}

struct Match {
    start: usize,
    end: usize,
    replacement: String,
    /// Dictionary order tiebreak so resolution stays deterministic.
    order: usize,
}

impl VocabularyTerm {
    /// Parses a dictionary term into its run sequence. A term is eligible only
    /// when it both begins and ends with an alphanumeric character: a trailing
    /// symbol (`C#`) would require INVENTING that symbol in the Transcript —
    /// the provider never heard it — so such terms are skipped here (they still
    /// receive Deepgram keyterm boosting, which has no such hazard).
    fn parse(term: &str) -> Option<Self> {
        let trimmed = term.trim();
        let first = trimmed.chars().next()?;
        let last = trimmed.chars().last()?;
        if !first.is_alphanumeric() || !last.is_alphanumeric() {
            return None;
        }
        let runs: Vec<String> = alphanumeric_runs(trimmed)
            .into_iter()
            .map(|run| run.lowered)
            .collect();
        if runs.is_empty() {
            return None;
        }
        let separators: Vec<char> = trimmed
            .chars()
            .filter(|character| !character.is_alphanumeric())
            .collect();
        Some(Self {
            original: trimmed.to_owned(),
            runs,
            separators,
        })
    }
}

/// One maximal alphanumeric run of text: its byte span and lowercase form.
struct TextRun {
    start: usize,
    end: usize,
    lowered: String,
}

fn alphanumeric_runs(text: &str) -> Vec<TextRun> {
    let mut runs = Vec::new();
    let mut start: Option<usize> = None;
    for (index, character) in text.char_indices() {
        if character.is_alphanumeric() {
            if start.is_none() {
                start = Some(index);
            }
        } else if let Some(begin) = start.take() {
            runs.push(make_run(text, begin, index));
        }
    }
    if let Some(begin) = start {
        runs.push(make_run(text, begin, text.len()));
    }
    runs
}

fn make_run(text: &str, start: usize, end: usize) -> TextRun {
    TextRun {
        start,
        end,
        lowered: text[start..end].to_lowercase(),
    }
}

/// Every character between consecutive matched runs must be whitespace or one
/// of the term's own separators. This keeps the substitution from swallowing
/// clause punctuation (`daemon, reload` never becomes `daemon-reload`) while
/// allowing the plain space/hyphen/slash joins the term's written form implies.
fn separators_allowed(text: &str, runs: &[TextRun], separators: &[char]) -> bool {
    runs.windows(2).all(|pair| {
        text[pair[0].end..pair[1].start]
            .chars()
            .all(|character| character.is_whitespace() || separators.contains(&character))
    })
}

/// Byte ranges of the Transcript that must never be rewritten: every token the
/// quote mask (spoken `quote … unquote` pairs, cues included) or the
/// metalinguistic mask (say-the-words spans) marks. Per-token ranges are
/// sufficient: a match that overlaps only the whitespace BETWEEN two masked
/// tokens necessarily overlaps a run inside one of them.
fn masked_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    let tokens = word_tokens(text);
    let (_quote_pairs, quote_skip) = quote_pairs_and_skip(&tokens);
    let meta = metalinguistic_mask(&tokens);
    tokens
        .iter()
        .enumerate()
        .filter(|(index, _)| quote_skip[*index] || meta[*index])
        .map(|(_, (start, end, _))| *start..*end)
        .collect()
}

/// Flattens `(word, confidence)` pairs into lowercase alphanumeric runs, each
/// inheriting its word's confidence, so run-sequence matching aligns with the
/// Transcript-side runs regardless of how a provider tokenized punctuation.
fn confidence_runs(word_confidences: &[(String, f64)]) -> Vec<(String, f64)> {
    word_confidences
        .iter()
        .flat_map(|(word, confidence)| {
            alphanumeric_runs(word)
                .into_iter()
                .map(move |run| (run.lowered, *confidence))
        })
        .collect()
}

/// Whether the matched runs are fully confident in the Deepgram evidence. The
/// FIRST run-sequence match in the confidence stream decides; a span whose
/// words are absent from the evidence (or no evidence at all) is treated as
/// NOT confidently transcribed, so the user's substitution applies.
fn confidently_transcribed(runs: &[TextRun], confidence_runs: &[(String, f64)]) -> bool {
    if runs.is_empty() || confidence_runs.len() < runs.len() {
        return false;
    }
    'outer: for start in 0..=confidence_runs.len() - runs.len() {
        for (offset, run) in runs.iter().enumerate() {
            if confidence_runs[start + offset].0 != run.lowered {
                continue 'outer;
            }
        }
        return runs
            .iter()
            .enumerate()
            .all(|(offset, _)| confidence_runs[start + offset].1 >= CONFIDENT_WORD_SKIP_THRESHOLD);
    }
    false
}

/// The casing shape of a matched span, judged across all cased characters of
/// its runs. Runs without cased characters (digits) are neutral.
#[derive(Clone, Copy, Eq, PartialEq)]
enum CasingShape {
    Lower,
    Title,
    Upper,
    Mixed,
}

fn casing_shape(span: &str) -> CasingShape {
    let mut any_cased = false;
    let mut all_upper = true;
    let mut all_lower = true;
    let mut all_title = true;
    for run in alphanumeric_runs(span) {
        let mut run_started_upper: Option<bool> = None;
        for character in span[run.start..run.end].chars() {
            if character.is_uppercase() {
                any_cased = true;
                all_lower = false;
                if run_started_upper.is_none() {
                    run_started_upper = Some(true);
                } else {
                    all_title = false;
                }
            } else if character.is_lowercase() {
                any_cased = true;
                all_upper = false;
                if run_started_upper.is_none() {
                    run_started_upper = Some(false);
                    all_title = false;
                }
            }
        }
        // A run whose first cased character is not uppercase (e.g. "rust")
        // cannot belong to a Title-shaped span; a caseless run (e.g. "99") is
        // neutral.
        if run_started_upper == Some(false) {
            all_title = false;
        }
    }
    if !any_cased {
        return CasingShape::Mixed;
    }
    if all_upper {
        CasingShape::Upper
    } else if all_lower {
        CasingShape::Lower
    } else if all_title {
        CasingShape::Title
    } else {
        CasingShape::Mixed
    }
}

/// Writes the canonical term in the matched span's casing shape.
fn shaped_term(term: &str, shape: CasingShape) -> String {
    match shape {
        CasingShape::Lower => term.to_lowercase(),
        CasingShape::Upper => term.to_uppercase(),
        CasingShape::Title => titlecase(term),
        CasingShape::Mixed => term.to_owned(),
    }
}

/// Title-cases a term: each alphanumeric run starts with its uppercase form
/// and continues lowercase; separators stay exactly as written.
fn titlecase(term: &str) -> String {
    let mut out = String::with_capacity(term.len());
    let mut in_run = false;
    for character in term.chars() {
        if character.is_alphanumeric() {
            if in_run {
                out.extend(character.to_lowercase());
            } else {
                out.extend(character.to_uppercase());
                in_run = true;
            }
        } else {
            in_run = false;
            out.push(character);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(list: &[&str]) -> Vec<String> {
        list.iter().map(|term| (*term).to_owned()).collect()
    }

    #[test]
    fn a_mixed_shape_single_word_term_is_canonicalized() {
        let corrected = apply_user_vocabulary("the rUst compiler is fast", &terms(&["Rust"]), &[]);
        assert_eq!(corrected, "the Rust compiler is fast");
    }

    #[test]
    fn consistent_shapes_round_trip_unchanged() {
        // The casing rules preserve the span's shape, so consistently-cased
        // spans are identity rewrites — fidelity outranks polish.
        for span in ["rust", "RUST", "Rust"] {
            let corrected =
                apply_user_vocabulary(&format!("use {span} now"), &terms(&["Rust"]), &[]);
            assert_eq!(corrected, format!("use {span} now"), "span {span:?}");
        }
    }

    #[test]
    fn a_multi_word_term_with_inconsistent_casing_is_canonicalized() {
        let corrected =
            apply_user_vocabulary("open Claude code please", &terms(&["Claude Code"]), &[]);
        assert_eq!(corrected, "open Claude Code please");
    }

    #[test]
    fn a_hyphenated_term_rejoins_its_spaced_form() {
        let corrected =
            apply_user_vocabulary("run the daemon reload job", &terms(&["daemon-reload"]), &[]);
        assert_eq!(corrected, "run the daemon-reload job");
    }

    #[test]
    fn a_slashed_term_rejoins_its_spaced_form() {
        let corrected = apply_user_vocabulary("the CI CD pipeline", &terms(&["CI/CD"]), &[]);
        assert_eq!(corrected, "the CI/CD pipeline");
    }

    #[test]
    fn clause_punctuation_between_runs_is_never_swallowed() {
        // The comma is clause punctuation, not a join: the gap rule refuses.
        let corrected = apply_user_vocabulary(
            "run the daemon, reload job",
            &terms(&["daemon-reload"]),
            &[],
        );
        assert_eq!(corrected, "run the daemon, reload job");
    }

    #[test]
    fn word_boundaries_are_enforced_so_substrings_never_fire() {
        // "cat" must not fire inside "concatenate" (one run), and the shaped
        // rewrite must not fire mid-token either.
        let corrected = apply_user_vocabulary("we concatenate lists", &terms(&["cat"]), &[]);
        assert_eq!(corrected, "we concatenate lists");
    }

    #[test]
    fn a_term_with_a_trailing_symbol_is_skipped_because_it_would_invent_it() {
        // "C#" can only be produced by INVENTING the '#': the provider never
        // heard it. Such terms are excluded from correction entirely.
        let corrected = apply_user_vocabulary("prefer c over go", &terms(&["C#"]), &[]);
        assert_eq!(corrected, "prefer c over go");
    }

    #[test]
    fn nothing_fires_inside_spoken_quotes() {
        let corrected =
            apply_user_vocabulary("quote rUst unquote stays as heard", &terms(&["Rust"]), &[]);
        assert_eq!(corrected, "quote rUst unquote stays as heard");
    }

    #[test]
    fn the_same_term_still_corrects_outside_the_quote() {
        let corrected = apply_user_vocabulary("rUst quote rust unquote", &terms(&["Rust"]), &[]);
        assert_eq!(corrected, "Rust quote rust unquote");
    }

    #[test]
    fn nothing_fires_inside_say_the_words_spans() {
        let corrected =
            apply_user_vocabulary("say the words rUst out loud now", &terms(&["Rust"]), &[]);
        assert_eq!(corrected, "say the words rUst out loud now");
    }

    #[test]
    fn fully_confident_deepgram_words_skip_the_substitution() {
        // Deepgram heard "rust" at 0.97: leave it alone even though the casing
        // is inconsistent with the dictionary.
        let confidences = vec![("the".to_owned(), 0.99), ("rust".to_owned(), 0.97)];
        let corrected = apply_user_vocabulary("the rUst compiler", &terms(&["Rust"]), &confidences);
        assert_eq!(corrected, "the rUst compiler");
    }

    #[test]
    fn a_low_confidence_word_keeps_the_substitution_available() {
        let confidences = vec![("the".to_owned(), 0.99), ("rust".to_owned(), 0.42)];
        let corrected = apply_user_vocabulary("the rUst compiler", &terms(&["Rust"]), &confidences);
        assert_eq!(corrected, "the Rust compiler");
    }

    #[test]
    fn a_multi_word_span_needs_every_word_not_confident_to_apply() {
        // ALL words at/above the threshold → skip.
        let confident = vec![("daemon".to_owned(), 0.95), ("reload".to_owned(), 0.91)];
        let corrected =
            apply_user_vocabulary("run daemon reload", &terms(&["daemon-reload"]), &confident);
        assert_eq!(corrected, "run daemon reload");
        // One word below the threshold → apply.
        let shaky = vec![("daemon".to_owned(), 0.95), ("reload".to_owned(), 0.5)];
        let corrected =
            apply_user_vocabulary("run daemon reload", &terms(&["daemon-reload"]), &shaky);
        assert_eq!(corrected, "run daemon-reload");
    }

    #[test]
    fn words_absent_from_the_confidence_stream_fall_back_to_applying() {
        let confidences = vec![("unrelated".to_owned(), 0.99)];
        let corrected = apply_user_vocabulary("the rUst compiler", &terms(&["Rust"]), &confidences);
        assert_eq!(corrected, "the Rust compiler");
    }

    #[test]
    fn corrections_are_idempotent() {
        let dictionary = terms(&["Rust", "daemon-reload", "Claude Code"]);
        let once =
            apply_user_vocabulary("rUst and daemon reload plus Claude code", &dictionary, &[]);
        let twice = apply_user_vocabulary(&once, &dictionary, &[]);
        assert_eq!(once, "Rust and daemon-reload plus Claude Code");
        assert_eq!(once, twice, "running twice must change nothing");
    }

    #[test]
    fn an_empty_dictionary_is_byte_identical() {
        let text = "the rUst daemon reload transcript";
        assert_eq!(apply_user_vocabulary(text, &[], &[]), text);
    }

    #[test]
    fn term_count_is_capped_at_the_documented_limit() {
        let mut dictionary: Vec<String> = Vec::new();
        // USER_VOCABULARY_TERM_LIMIT filler terms first, the real term last.
        for index in 0..USER_VOCABULARY_TERM_LIMIT {
            dictionary.push(format!("filler{index:04}"));
        }
        dictionary.push("Rust".to_owned());
        let corrected = apply_user_vocabulary("the rUst compiler", &dictionary, &[]);
        assert_eq!(
            corrected, "the rUst compiler",
            "terms past the cap must be skipped"
        );
    }

    #[test]
    fn longer_matches_win_at_the_same_start() {
        // "Claude Code" (two runs) must win over "Claude" at the same span
        // start, and "Claude" must not corrupt the accepted span.
        let corrected = apply_user_vocabulary(
            "open Claude code now",
            &terms(&["Claude", "Claude Code"]),
            &[],
        );
        assert_eq!(corrected, "open Claude Code now");
    }

    #[test]
    fn trailing_punctuation_and_surroundings_survive() {
        let corrected = apply_user_vocabulary("use rUst, then go.", &terms(&["Rust"]), &[]);
        assert_eq!(corrected, "use Rust, then go.");
    }

    #[test]
    fn a_lowercase_span_of_an_uppercase_term_stays_lowercase() {
        // Shape preservation, not imposition: "api" written in the dictionary
        // as "API" stays "api" when the provider heard it lowercase.
        let corrected = apply_user_vocabulary("the api layer", &terms(&["API"]), &[]);
        assert_eq!(corrected, "the api layer");
    }

    #[test]
    fn digits_are_neutral_in_shape_detection() {
        let corrected = apply_user_vocabulary("measure the p99 LATENCY", &terms(&["p99"]), &[]);
        // The span "p99" is all-lowercase → rewritten as the term's lowercase
        // form, unchanged.
        assert_eq!(corrected, "measure the p99 LATENCY");
    }

    #[test]
    fn confidence_evidence_with_punctuated_provider_words_still_aligns() {
        // Deepgram word tokens may carry punctuation variants; runs align.
        let confidences = vec![("rust,".to_owned(), 0.2)];
        let corrected = apply_user_vocabulary("rUst rocks", &terms(&["Rust"]), &confidences);
        assert_eq!(corrected, "Rust rocks");
    }
}
