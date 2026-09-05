//! Slice B4: confidence-aware divergence-point arbitration.
//!
//! When both Source Transcripts are present and BOTH providers retained word
//! confidence evidence for their own text, the two texts are aligned word by
//! word and each divergent region is offered to the OTHER provider's words —
//! but only when every guard below holds. Anything else keeps the current
//! selection exactly; every guard fails closed.
//!
//! # The algorithm
//!
//! 1. **Alignment.** The two texts are compared as `normalized_words`
//!    sequences (the same normalization the §3.4 source gates use: punctuation
//!    stripped, case-folded, contractions expanded) with a Levenshtein
//!    alignment that carries a full backpointer history — the same approach as
//!    the transcript-quality metrics aligner, re-implemented here rather than
//!    adding a dependency. Maximal runs of non-matching operations are the
//!    divergent regions. The alignment is position-based, never first-match,
//!    so repeated words pair occurrence-for-occurrence (the B2 lesson).
//! 2. **Region guards**, evaluated per region in this order, each failing
//!    closed to "keep the incumbent words":
//!    - **Shape**: both sides heard something, the word-count change stays
//!      within [`MAX_REGION_LENGTH_DELTA`], and both region boundaries fall on
//!      whole whitespace tokens so a splice never cuts the middle of an
//!      expanded contraction.
//!    - **Masks**: the region sits outside spoken `quote … unquote` pairs and
//!      say-the-words (`metalinguistic`) spans on BOTH sides, reusing the same
//!      `pub(crate)` masks the user-vocabulary correction and the
//!      dictation-grammar pass use.
//!    - **Meaning**: no token in either region is a negation, a digit or
//!      number word, a question word, or an affirmation/polarity word — the
//!      minimal-edit word classes that can invert meaning silently (see
//!      [`token_forbids_flip`]).
//!    - **Decisive confidence gap**: every incumbent word in the region is
//!      below [`FLIP_INCUMBENT_MAX_CONFIDENCE`] AND every other-side word is at
//!      or above [`FLIP_OTHER_MIN_CONFIDENCE`]. Missing confidence for any
//!      word, or any tie at a threshold, keeps the incumbent.
//! 3. **Assembly.** All accepted regions are spliced into the incumbent text
//!    in one pass, preserving the incumbent's rendering everywhere except the
//!    replaced tokens (punctuation attached to a replaced boundary token is
//!    re-attached to the replacement).
//! 4. **Final gates.** The assembled candidate must still be source-derived
//!    (every delivered word was heard by SOME provider — never re-interpreted,
//!    never invented) and must pass the same quality guards a merge candidate
//!    passes. Either gate failing rejects EVERY flip and delivers the
//!    incumbent unchanged.
//!
//! # Placement and provenance
//!
//! Arbitration runs INSIDE selection — after `decide_uncorrected` chose the
//! incumbent and before the user-vocabulary correction — and only when the
//! incumbent IS one provider's source text (`SourceDeepgram`, `SourceGroq`,
//! `NearIdenticalGroq`). A reconciled, repaired, or reconstructed final is
//! never arbitrated. The delivered text after a flip has mixed provenance; the
//! word-confidence evidence the correction gate reads stays the SELECTED
//! (backbone) provider's — arbitration never re-tags evidence, and the
//! correction gate's documented rules for evidence that does or does not reach
//! a span apply unchanged.
//!
//! Fail-closed everywhere: an alignment that exceeds [`MAX_ALIGNMENT_CELLS`],
//! a confidence stream that does not positionally describe its provider's own
//! text, or any guard tie keeps the current selection byte-identically.

use crate::diagnostics::{ConfidenceArbitrationRejection, MAX_CONFIDENCE_ARBITRATION_REJECTIONS};
use crate::local_baseline::{metalinguistic_mask, quote_pairs_and_skip, word_tokens};
use crate::{SourceTranscript, is_source_derived, normalize_token, quality_failure_reason};

/// Every incumbent word in a considered region must be BELOW this confidence
/// for the incumbent side to count as uncertain enough to overturn. At exactly
/// the threshold (a tie) the incumbent keeps the words: the provider was sure
/// enough that overriding it is the risky direction.
pub(crate) const FLIP_INCUMBENT_MAX_CONFIDENCE: f64 = 0.5;

/// Every other-side word in a considered region must be AT LEAST this
/// confident for the other side's hearing to be decisive. Below it, the flip
/// would trade one uncertain hearing for another.
pub(crate) const FLIP_OTHER_MIN_CONFIDENCE: f64 = 0.75;

/// A flip may not change a region's word count by more than this: a
/// one-word insertion or deletion inside an otherwise aligned region is
/// admissible when both sides heard the region, anything larger is a
/// structural disagreement a confidence gap cannot vouch for.
pub(crate) const MAX_REGION_LENGTH_DELTA: usize = 1;

/// The alignment DP is bounded: a pair whose cell count exceeds this is not
/// arbitrated (the incumbent selection stands). Real dictations align in
/// thousands of cells; the cap only bounds pathological inputs.
const MAX_ALIGNMENT_CELLS: usize = 1_000_000;

/// One divergent region: half-open `(start, end)` index ranges into the
/// incumbent's (`a`) and the other side's (`b`) normalized word sequences.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Region {
    a: (usize, usize),
    b: (usize, usize),
}

/// Everything arbitration needs: the incumbent selection's provider text and
/// its own confidence evidence, the other provider's text and evidence, and
/// the Source Transcripts the final candidate must stay derived from.
pub(crate) struct ArbitrationInput<'a> {
    pub incumbent_text: &'a str,
    pub incumbent_confidences: &'a [(String, f64)],
    pub other_text: &'a str,
    pub other_confidences: &'a [(String, f64)],
    pub sources: &'a [SourceTranscript],
}

/// The outcome of one arbitration pass. `text` is the incumbent text with
/// accepted flips spliced in (identical to the incumbent when
/// `regions_flipped` is zero).
pub(crate) struct ArbitrationOutcome {
    pub text: String,
    pub regions_considered: u32,
    pub regions_flipped: u32,
    pub rejections: Vec<ConfidenceArbitrationRejection>,
}

/// One alignment operation between the two normalized word sequences.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    /// Both sides aligned on the same normalized word.
    Match,
    /// A word on each side aligns imperfectly (a substitution).
    Sub,
    /// An incumbent word no other-side word accounts for.
    Del,
    /// An other-side word no incumbent word accounts for.
    Ins,
}

/// Runs one arbitration pass. Returns `None` — keep the current selection
/// exactly — when either confidence stream does not positionally describe its
/// provider's own text, or when the alignment is too large to compute.
pub(crate) fn arbitrate(input: ArbitrationInput<'_>) -> Option<ArbitrationOutcome> {
    let incumbent = try_prepare_side(input.incumbent_text, input.incumbent_confidences)?;
    let other = try_prepare_side(input.other_text, input.other_confidences)?;
    let ops = align(&incumbent.normalized, &other.normalized)?;
    let regions = divergent_regions(&ops);
    let regions_considered = u32::try_from(regions.len()).unwrap_or(u32::MAX);
    let mut rejections = Vec::new();
    let mut flips = Vec::new();
    for region in &regions {
        match evaluate_region(region, &incumbent, &other) {
            Ok(()) => flips.push(*region),
            Err(reason) => rejections.push(reason),
        }
    }
    let mut text = incumbent.text.to_owned();
    let mut regions_flipped = 0u32;
    if !flips.is_empty() {
        let candidate = apply_flips(&incumbent, &other, &flips);
        // The final gates: the candidate stays source-derived and passes the
        // same quality guards any merge candidate passes. A failure rejects
        // every flip — the incumbent is delivered unchanged.
        if is_source_derived(&candidate, input.sources)
            && quality_failure_reason(&candidate, input.sources).is_none()
        {
            text = candidate;
            regions_flipped = u32::try_from(flips.len()).unwrap_or(u32::MAX);
        } else {
            rejections.extend(std::iter::repeat_n(
                ConfidenceArbitrationRejection::CandidateRejected,
                flips.len(),
            ));
        }
    }
    rejections.truncate(MAX_CONFIDENCE_ARBITRATION_REJECTIONS);
    Some(ArbitrationOutcome {
        text,
        regions_considered,
        regions_flipped,
        rejections,
    })
}

/// A provider's text prepared for arbitration: whitespace tokens with byte
/// spans, the normalized word sequence with its token mapping, the per-word
/// confidence of its OWN evidence stream aligned to the normalized positions,
/// and the quote/metalinguistic masks over its tokens.
struct PreparedSide<'a> {
    text: &'a str,
    tokens: Vec<(usize, usize, &'a str)>,
    normalized: Vec<String>,
    /// Normalized word index → whitespace token index.
    token_of_norm: Vec<usize>,
    /// Whether the normalized word at this index is the first part of its token.
    starts_token: Vec<bool>,
    /// Whether the normalized word at this index is the last part of its token.
    ends_token: Vec<bool>,
    /// Confidence per normalized word; `None` where the provider's evidence
    /// does not reach.
    confidences: Vec<Option<f64>>,
    /// Quote/metalinguistic mask, per token.
    masked: Vec<bool>,
}

/// Prepares one side, or `None` when the confidence stream does not
/// positionally describe this text (the evidence is then unusable and the
/// whole arbitration is skipped — fail closed).
fn try_prepare_side<'a>(text: &'a str, evidence: &'a [(String, f64)]) -> Option<PreparedSide<'a>> {
    let tokens = word_tokens(text);
    let mut normalized = Vec::new();
    let mut token_of_norm = Vec::new();
    let mut starts_token = Vec::new();
    let mut ends_token = Vec::new();
    for (token_index, (_, _, token)) in tokens.iter().enumerate() {
        let mut parts: Vec<String> = normalize_token(token)
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect();
        let part_count = parts.len();
        for (part_index, part) in parts.drain(..).enumerate() {
            normalized.push(part);
            token_of_norm.push(token_index);
            starts_token.push(part_index == 0);
            ends_token.push(part_index + 1 == part_count);
        }
    }
    let mut confidences = vec![None; normalized.len()];
    let mut position = 0usize;
    for (word, confidence) in evidence {
        for part in normalize_token(word) {
            if part.is_empty() {
                continue;
            }
            match confidences.get_mut(position) {
                // Evidence past the text's own words — e.g. the tail of a
                // stripped anchored outro — is ignored; the delivered text no
                // longer contains those words.
                None => {}
                Some(slot) => {
                    if normalized[position] != part {
                        return None;
                    }
                    *slot = Some(clamp_confidence(*confidence));
                    position += 1;
                }
            }
        }
    }
    let (_pairs, quote_skip) = quote_pairs_and_skip(&tokens);
    let meta = metalinguistic_mask(&tokens);
    let masked = tokens
        .iter()
        .enumerate()
        .map(|(index, _)| quote_skip[index] || meta[index])
        .collect();
    Some(PreparedSide {
        text,
        tokens,
        normalized,
        token_of_norm,
        starts_token,
        ends_token,
        confidences,
        masked,
    })
}

/// Mirrors the Deepgram ingest clamp: a non-finite number is unproven and
/// every confidence is held inside the `[0, 1]` domain.
fn clamp_confidence(confidence: f64) -> f64 {
    if confidence.is_finite() {
        confidence.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// Word-level Levenshtein alignment with a full backpointer history, in the
/// shape of the transcript-quality metrics aligner. Diagonal steps win ties so
/// an imperfect word pair stays ONE substitution instead of a delete+insert
/// split that would produce empty-sided regions. Returns `None` past the cell
/// cap (fail closed).
fn align(a: &[String], b: &[String]) -> Option<Vec<Op>> {
    let n = a.len();
    let m = b.len();
    if (n + 1).saturating_mul(m + 1) > MAX_ALIGNMENT_CELLS {
        return None;
    }
    // Costs roll on two rows; the traceback needs the full backpointer
    // history, so `back` keeps one byte per cell.
    let mut dp = vec![vec![0u32; m + 1]; 2];
    let mut back = vec![0u8; (n + 1) * (m + 1)];
    let index = |i: usize, j: usize| i * (m + 1) + j;
    for j in 1..=m {
        dp[0][j] = j as u32;
        back[index(0, j)] = 2;
    }
    for i in 1..=n {
        let cur = i % 2;
        let prev = 1 - cur;
        dp[cur][0] = i as u32;
        back[index(i, 0)] = 1;
        for j in 1..=m {
            let cost = u32::from(a[i - 1] != b[j - 1]);
            let diagonal = dp[prev][j - 1] + cost;
            let deletion = dp[prev][j] + 1;
            let insertion = dp[cur][j - 1] + 1;
            if diagonal <= deletion && diagonal <= insertion {
                dp[cur][j] = diagonal;
                back[index(i, j)] = 3;
            } else if deletion <= insertion {
                dp[cur][j] = deletion;
                back[index(i, j)] = 1;
            } else {
                dp[cur][j] = insertion;
                back[index(i, j)] = 2;
            }
        }
    }
    let mut ops = Vec::new();
    let mut i = n;
    let mut j = m;
    while i > 0 || j > 0 {
        match back[index(i, j)] {
            1 => {
                ops.push(Op::Del);
                i -= 1;
            }
            2 => {
                ops.push(Op::Ins);
                j -= 1;
            }
            3 => {
                ops.push(if a[i - 1] == b[j - 1] {
                    Op::Match
                } else {
                    Op::Sub
                });
                i -= 1;
                j -= 1;
            }
            _ => break,
        }
    }
    ops.reverse();
    Some(ops)
}

/// Groups maximal runs of non-matching alignment operations into divergent
/// regions.
fn divergent_regions(ops: &[Op]) -> Vec<Region> {
    let mut regions = Vec::new();
    let mut current: Option<Region> = None;
    let mut i = 0usize;
    let mut j = 0usize;
    for op in ops {
        match op {
            Op::Match => {
                if let Some(region) = current.take() {
                    regions.push(region);
                }
                i += 1;
                j += 1;
            }
            Op::Sub => {
                let region = current.get_or_insert(Region {
                    a: (i, i),
                    b: (j, j),
                });
                region.a.1 = i + 1;
                region.b.1 = j + 1;
                i += 1;
                j += 1;
            }
            Op::Del => {
                let region = current.get_or_insert(Region {
                    a: (i, i),
                    b: (j, j),
                });
                region.a.1 = i + 1;
                i += 1;
            }
            Op::Ins => {
                let region = current.get_or_insert(Region {
                    a: (i, i),
                    b: (j, j),
                });
                region.b.1 = j + 1;
                j += 1;
            }
        }
    }
    if let Some(region) = current {
        regions.push(region);
    }
    regions
}

/// Evaluates one region's guards, in the documented order: shape, masks,
/// meaning, then the decisive confidence gap.
fn evaluate_region(
    region: &Region,
    incumbent: &PreparedSide<'_>,
    other: &PreparedSide<'_>,
) -> Result<(), ConfidenceArbitrationRejection> {
    let (i0, i1) = region.a;
    let (j0, j1) = region.b;
    // Shape: both sides heard the region, the count change is small, and both
    // boundaries fall on whole tokens.
    if i0 == i1 || j0 == j1 {
        return Err(ConfidenceArbitrationRejection::RegionShapeUnsafe);
    }
    if (i1 - i0).abs_diff(j1 - j0) > MAX_REGION_LENGTH_DELTA {
        return Err(ConfidenceArbitrationRejection::RegionShapeUnsafe);
    }
    if !incumbent.region_on_token_boundaries(i0, i1) || !other.region_on_token_boundaries(j0, j1) {
        return Err(ConfidenceArbitrationRejection::RegionShapeUnsafe);
    }
    // Masks: no flip inside a spoken quote or say-the-words span, on either
    // side.
    if incumbent.region_masked(i0, i1) || other.region_masked(j0, j1) {
        return Err(ConfidenceArbitrationRejection::MaskedSpan);
    }
    // Meaning: no negation, number, question, or polarity word in either
    // region.
    if incumbent.has_forbidden_token(i0, i1) || other.has_forbidden_token(j0, j1) {
        return Err(ConfidenceArbitrationRejection::MeaningInvertingTokens);
    }
    // The decisive confidence gap: the incumbent must be uncertain and the
    // other side confident. Missing confidence for any word, or a tie at
    // either threshold, keeps the incumbent.
    let Some(incumbent_max) = incumbent.region_max_confidence(i0, i1) else {
        return Err(ConfidenceArbitrationRejection::MissingConfidence);
    };
    let Some(other_min) = other.region_min_confidence(j0, j1) else {
        return Err(ConfidenceArbitrationRejection::MissingConfidence);
    };
    if incumbent_max < FLIP_INCUMBENT_MAX_CONFIDENCE && other_min >= FLIP_OTHER_MIN_CONFIDENCE {
        Ok(())
    } else {
        Err(ConfidenceArbitrationRejection::ConfidenceGapNotDecisive)
    }
}

impl PreparedSide<'_> {
    fn region_on_token_boundaries(&self, start: usize, end: usize) -> bool {
        self.starts_token[start] && self.ends_token[end - 1]
    }

    fn region_masked(&self, start: usize, end: usize) -> bool {
        let first_token = self.token_of_norm[start];
        let last_token = self.token_of_norm[end - 1];
        (first_token..=last_token).any(|token| self.masked[token])
    }

    fn has_forbidden_token(&self, start: usize, end: usize) -> bool {
        self.normalized[start..end]
            .iter()
            .any(|token| token_forbids_flip(token))
    }

    /// The MAXIMUM confidence across the region, or `None` when any word in
    /// the region has no confidence from this provider.
    fn region_max_confidence(&self, start: usize, end: usize) -> Option<f64> {
        self.confidences[start..end]
            .iter()
            .try_fold(f64::NEG_INFINITY, |maximum, confidence| {
                confidence.map(|value| maximum.max(value))
            })
    }

    /// The MINIMUM confidence across the region, or `None` when any word in
    /// the region has no confidence from this provider.
    fn region_min_confidence(&self, start: usize, end: usize) -> Option<f64> {
        self.confidences[start..end]
            .iter()
            .try_fold(f64::INFINITY, |minimum, confidence| {
                confidence.map(|value| minimum.min(value))
            })
    }
}

/// Negations: flipping any of these can silently invert the sentence.
const NEGATION_TOKENS: [&str; 12] = [
    "not", "no", "never", "cannot", "nor", "none", "nothing", "nobody", "neither", "without",
    "hardly", "scarcely",
];

/// Spelled-out numbers and ordinals: `three`↔`free`, `fifth`↔`fist`-class
/// minimal pairs that change quantities, dates, and rankings.
const NUMBER_TOKENS: [&str; 42] = [
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
    "twenty",
    "thirty",
    "forty",
    "fifty",
    "sixty",
    "seventy",
    "eighty",
    "ninety",
    "hundred",
    "thousand",
    "million",
    "billion",
    "trillion",
    "dozen",
    "couple",
    "half",
    "quarter",
    "first",
    "second",
    "third",
    "once",
    "twice",
];

/// Affirmation/polarity words: a flip here can turn an answer or a
/// confirmation into its opposite.
const POLARITY_TOKENS: [&str; 15] = [
    "yes",
    "yeah",
    "yep",
    "yup",
    "nope",
    "nah",
    "ok",
    "okay",
    "true",
    "false",
    "correct",
    "incorrect",
    "right",
    "wrong",
    "maybe",
];

/// Question words: a flip here can turn a question into a statement or change
/// what is being asked. Punctuation is stripped by normalization, so no local
/// signal remains to tell them apart.
const QUESTION_TOKENS: [&str; 10] = [
    "who", "whom", "whose", "what", "when", "where", "why", "how", "which", "whether",
];

/// Whether flipping a region containing this (already normalized) token could
/// invert meaning. Digits are forbidden as a class — every numeral is a
/// number.
fn token_forbids_flip(token: &str) -> bool {
    token.chars().any(|character| character.is_ascii_digit())
        || NEGATION_TOKENS.contains(&token)
        || NUMBER_TOKENS.contains(&token)
        || POLARITY_TOKENS.contains(&token)
        || QUESTION_TOKENS.contains(&token)
}

/// Splits `token` into `(leading punctuation, alphanumeric core, trailing
/// punctuation)`. A region boundary token always has a core: tokens without
/// one normalize to no words and can never carry a normalized region.
fn split_token_punctuation(token: &str) -> (&str, &str, &str) {
    let core_start = token
        .char_indices()
        .find(|(_, character)| character.is_alphanumeric())
        .map(|(index, _)| index)
        .unwrap_or(token.len());
    let core_end = token
        .char_indices()
        .rev()
        .find(|(_, character)| character.is_alphanumeric())
        .map(|(index, character)| index + character.len_utf8())
        .unwrap_or(core_start);
    if core_start >= core_end {
        return (token, "", "");
    }
    (
        &token[..core_start],
        &token[core_start..core_end],
        &token[core_end..],
    )
}

/// Builds the replacement text for one flip: the other side's raw tokens for
/// the region, with the incumbent's boundary punctuation re-attached so the
/// splice does not eat a comma, a closing quote, or a sentence terminal that
/// the incumbent's rendering carried.
fn replacement_for(
    incumbent: &PreparedSide<'_>,
    other: &PreparedSide<'_>,
    flip: &Region,
) -> (usize, usize, String) {
    let incumbent_start = incumbent.token_of_norm[flip.a.0];
    let incumbent_end = incumbent.token_of_norm[flip.a.1 - 1] + 1;
    let other_start = other.token_of_norm[flip.b.0];
    let other_end = other.token_of_norm[flip.b.1 - 1] + 1;
    let mut replacement = other.tokens[other_start..other_end]
        .iter()
        .map(|(_, _, token)| *token)
        .collect::<Vec<_>>()
        .join(" ");
    let first = incumbent.tokens[incumbent_start].2;
    let last = incumbent.tokens[incumbent_end - 1].2;
    let (prefix, _, _) = split_token_punctuation(first);
    let (_, _, suffix) = split_token_punctuation(last);
    if !prefix.is_empty() && replacement.starts_with(|character: char| character.is_alphanumeric())
    {
        replacement = format!("{prefix}{replacement}");
    }
    if !suffix.is_empty() && replacement.ends_with(|character: char| character.is_alphanumeric()) {
        replacement.push_str(suffix);
    }
    (incumbent_start, incumbent_end, replacement)
}

/// Splices every accepted flip into the incumbent text in one pass. Regions
/// are separated by at least one matched word and never share a token (the
/// token-boundary guard), so the byte ranges are disjoint.
fn apply_flips(incumbent: &PreparedSide<'_>, other: &PreparedSide<'_>, flips: &[Region]) -> String {
    let mut out = String::with_capacity(incumbent.text.len());
    let mut cursor = 0usize;
    for flip in flips {
        let (start, end, replacement) = replacement_for(incumbent, other, flip);
        let byte_start = incumbent.tokens[start].0;
        let byte_end = incumbent.tokens[end - 1].1;
        out.push_str(&incumbent.text[cursor..byte_start]);
        out.push_str(&replacement);
        cursor = byte_end;
    }
    out.push_str(&incumbent.text[cursor..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Provider;

    fn confidences(words: &[(&str, f64)]) -> Vec<(String, f64)> {
        words
            .iter()
            .map(|(word, confidence)| ((*word).to_owned(), *confidence))
            .collect()
    }

    fn sources(deepgram: &str, groq: &str) -> Vec<SourceTranscript> {
        vec![
            SourceTranscript {
                provider: Provider::Deepgram,
                text: deepgram.to_owned(),
            },
            SourceTranscript {
                provider: Provider::Groq,
                text: groq.to_owned(),
            },
        ]
    }

    // ─── Alignment ──────────────────────────────────────────────────────────

    #[test]
    fn identical_sequences_align_to_all_matches() {
        let words = crate::normalized_words("the cache migration ran today");
        let ops = align(&words, &words).expect("aligns");
        assert!(ops.iter().all(|op| *op == Op::Match));
        assert!(divergent_regions(&ops).is_empty());
    }

    #[test]
    fn punctuation_and_casing_normalize_away_in_alignment() {
        // "Cache," vs "cache" and "today." vs "TODAY" are the SAME normalized
        // words: the alignment sees no divergence.
        let left = crate::normalized_words("Cache, the daemon restarted today.");
        let right = crate::normalized_words("cache THE daemon restarted TODAY");
        let ops = align(&left, &right).expect("aligns");
        assert!(divergent_regions(&ops).is_empty());
    }

    #[test]
    fn contractions_expand_to_the_same_normalized_words() {
        let left = crate::normalized_words("the daemon can't restart");
        let right = crate::normalized_words("the daemon cannot restart");
        let ops = align(&left, &right).expect("aligns");
        // "can't" expands to [can, not]; "cannot" normalizes to [cannot] —
        // NOT the same, but the divergence must be a single bounded region,
        // never an alignment panic or a first-match mispairing.
        assert_eq!(divergent_regions(&ops).len(), 1);
    }

    #[test]
    fn repeated_words_pair_by_occurrence_not_first_match() {
        // The B2 lesson: "cat" appears twice and only ONE side differs per
        // position. Occurrence-indexed alignment yields two substitution
        // regions, each pairing its own occurrence.
        let left = crate::normalized_words("the cat chased the cat");
        let right = crate::normalized_words("the bat chased the bat");
        let ops = align(&left, &right).expect("aligns");
        let regions = divergent_regions(&ops);
        assert_eq!(regions.len(), 2, "each occurrence is its own region");
        assert_eq!(regions[0].a, (1, 2));
        assert_eq!(regions[1].a, (4, 5));
        assert_eq!(regions[0].b, (1, 2));
        assert_eq!(regions[1].b, (4, 5));
    }

    #[test]
    fn pure_insertions_and_deletions_form_empty_sided_regions() {
        let left = crate::normalized_words("deploy the service");
        let right = crate::normalized_words("deploy the rust service");
        let ops = align(&left, &right).expect("aligns");
        let regions = divergent_regions(&ops);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].a, (2, 2), "the incumbent side is empty");
        assert_eq!(regions[0].b, (2, 3));
    }

    #[test]
    fn an_oversized_alignment_fails_closed() {
        // Exceeding MAX_ALIGNMENT_CELLS must return None, not allocate the
        // full matrix.
        let words: Vec<String> = (0..2_000).map(|index| format!("w{index}")).collect();
        assert!(align(&words, words.clone().as_slice()).is_none());
    }

    // ─── Thresholds ─────────────────────────────────────────────────────────

    #[test]
    fn a_flip_fires_at_the_threshold_margin() {
        let all = sources("deploy the cache migration", "deploy the cash migration");
        let outcome = arbitrate(ArbitrationInput {
            incumbent_text: "deploy the cash migration",
            incumbent_confidences: &confidences(&[
                ("deploy", 0.9),
                ("the", 0.9),
                ("cash", 0.49),
                ("migration", 0.9),
            ]),
            other_text: "deploy the cache migration",
            other_confidences: &confidences(&[
                ("deploy", 0.9),
                ("the", 0.9),
                ("cache", 0.75),
                ("migration", 0.9),
            ]),
            sources: &all,
        })
        .expect("arbitrates");
        assert_eq!(outcome.regions_considered, 1);
        assert_eq!(outcome.regions_flipped, 1);
        assert_eq!(outcome.text, "deploy the cache migration");
    }

    #[test]
    fn an_incumbent_tie_at_the_threshold_keeps_the_incumbent() {
        let all = sources("deploy the cache migration", "deploy the cash migration");
        let outcome = arbitrate(ArbitrationInput {
            incumbent_text: "deploy the cash migration",
            incumbent_confidences: &confidences(&[
                ("deploy", 0.9),
                ("the", 0.9),
                ("cash", FLIP_INCUMBENT_MAX_CONFIDENCE),
                ("migration", 0.9),
            ]),
            other_text: "deploy the cache migration",
            other_confidences: &confidences(&[
                ("deploy", 0.9),
                ("the", 0.9),
                ("cache", 0.9),
                ("migration", 0.9),
            ]),
            sources: &all,
        })
        .expect("arbitrates");
        assert_eq!(outcome.regions_flipped, 0);
        assert_eq!(outcome.text, "deploy the cash migration");
        assert_eq!(
            outcome.rejections,
            vec![ConfidenceArbitrationRejection::ConfidenceGapNotDecisive]
        );
    }

    #[test]
    fn an_other_side_word_below_the_threshold_keeps_the_incumbent() {
        let all = sources("deploy the cache migration", "deploy the cash migration");
        let outcome = arbitrate(ArbitrationInput {
            incumbent_text: "deploy the cash migration",
            incumbent_confidences: &confidences(&[
                ("deploy", 0.9),
                ("the", 0.9),
                ("cash", 0.2),
                ("migration", 0.9),
            ]),
            other_text: "deploy the cache migration",
            other_confidences: &confidences(&[
                ("deploy", 0.9),
                ("the", 0.9),
                ("cache", FLIP_OTHER_MIN_CONFIDENCE - 0.01),
                ("migration", 0.9),
            ]),
            sources: &all,
        })
        .expect("arbitrates");
        assert_eq!(outcome.regions_flipped, 0);
        assert_eq!(
            outcome.rejections,
            vec![ConfidenceArbitrationRejection::ConfidenceGapNotDecisive]
        );
    }

    #[test]
    fn the_region_maximum_and_minimum_bind_not_the_average() {
        // The incumbent side is uncertain only if EVERY word is uncertain; the
        // other side is decisive only if EVERY word is confident. One
        // confident incumbent word in the region kills the flip.
        let all = sources("the cache daemon restarted", "the cash server restarted");
        let outcome = arbitrate(ArbitrationInput {
            incumbent_text: "the cash server restarted",
            // "cash" is shaky but "server" is confident: the region max is 0.9.
            incumbent_confidences: &confidences(&[
                ("the", 0.9),
                ("cash", 0.2),
                ("server", 0.9),
                ("restarted", 0.9),
            ]),
            other_text: "the cache daemon restarted",
            other_confidences: &confidences(&[
                ("the", 0.9),
                ("cache", 0.95),
                ("daemon", 0.95),
                ("restarted", 0.9),
            ]),
            sources: &all,
        })
        .expect("arbitrates");
        assert_eq!(outcome.regions_flipped, 0);
        assert_eq!(
            outcome.rejections,
            vec![ConfidenceArbitrationRejection::ConfidenceGapNotDecisive]
        );
    }

    #[test]
    fn missing_confidence_for_any_word_keeps_the_incumbent() {
        let all = sources("deploy the cache migration", "deploy the cash migration");
        let outcome = arbitrate(ArbitrationInput {
            incumbent_text: "deploy the cash migration",
            incumbent_confidences: &confidences(&[
                ("deploy", 0.9),
                ("the", 0.9),
                ("cash", 0.2),
                ("migration", 0.9),
            ]),
            other_text: "deploy the cache migration",
            // Evidence stops before "cache": the other side cannot vouch.
            other_confidences: &confidences(&[("deploy", 0.9), ("the", 0.9)]),
            sources: &all,
        })
        .expect("arbitrates");
        assert_eq!(outcome.regions_flipped, 0);
        assert_eq!(
            outcome.rejections,
            vec![ConfidenceArbitrationRejection::MissingConfidence]
        );
    }

    #[test]
    fn evidence_that_does_not_describe_its_text_skips_arbitration() {
        let all = sources("deploy the cache migration", "deploy the cash migration");
        assert!(
            arbitrate(ArbitrationInput {
                incumbent_text: "deploy the cash migration",
                incumbent_confidences: &confidences(&[
                    ("entirely", 0.9),
                    ("different", 0.9),
                    ("words", 0.9)
                ]),
                other_text: "deploy the cache migration",
                other_confidences: &confidences(&[
                    ("deploy", 0.9),
                    ("the", 0.9),
                    ("cache", 0.9),
                    ("migration", 0.9)
                ]),
                sources: &all,
            })
            .is_none()
        );
    }

    #[test]
    fn evidence_past_the_text_end_is_ignored_not_a_mismatch() {
        // A sanitized anchored outro leaves the evidence stream longer than
        // the delivered text; the extra tail must not invalidate the stream.
        let all = sources("deploy the cache migration", "deploy the cash migration");
        let outcome = arbitrate(ArbitrationInput {
            incumbent_text: "deploy the cash migration",
            incumbent_confidences: &confidences(&[
                ("deploy", 0.9),
                ("the", 0.9),
                ("cash", 0.2),
                ("migration", 0.9),
                ("please", 0.1),
                ("subscribe", 0.1),
            ]),
            other_text: "deploy the cache migration",
            other_confidences: &confidences(&[
                ("deploy", 0.9),
                ("the", 0.9),
                ("cache", 0.95),
                ("migration", 0.9),
            ]),
            sources: &all,
        })
        .expect("the extra tail is tolerated");
        assert_eq!(outcome.regions_flipped, 1);
        assert_eq!(outcome.text, "deploy the cache migration");
    }

    // ─── Meaning-inverting rejections ───────────────────────────────────────

    #[test]
    fn a_negation_is_never_flipped_away() {
        // Incumbent heard "not" (shaky); the other side heard "now". Flipping
        // would silently delete the negation.
        let all = sources("the daemon will now restart", "the daemon will not restart");
        let outcome = arbitrate(ArbitrationInput {
            incumbent_text: "the daemon will not restart",
            incumbent_confidences: &confidences(&[
                ("the", 0.9),
                ("daemon", 0.9),
                ("will", 0.9),
                ("not", 0.3),
                ("restart", 0.9),
            ]),
            other_text: "the daemon will now restart",
            other_confidences: &confidences(&[
                ("the", 0.95),
                ("daemon", 0.95),
                ("will", 0.95),
                ("now", 0.95),
                ("restart", 0.95),
            ]),
            sources: &all,
        })
        .expect("arbitrates");
        assert_eq!(outcome.regions_flipped, 0);
        assert_eq!(outcome.text, "the daemon will not restart");
        assert_eq!(
            outcome.rejections,
            vec![ConfidenceArbitrationRejection::MeaningInvertingTokens]
        );
    }

    #[test]
    fn a_negation_is_never_flipped_in() {
        // Mirror direction: flipping "now" (shaky) to a confident "not" would
        // invent a negation the incumbent never heard.
        let all = sources("the daemon will not restart", "the daemon will now restart");
        let outcome = arbitrate(ArbitrationInput {
            incumbent_text: "the daemon will now restart",
            incumbent_confidences: &confidences(&[
                ("the", 0.9),
                ("daemon", 0.9),
                ("will", 0.9),
                ("now", 0.3),
                ("restart", 0.9),
            ]),
            other_text: "the daemon will not restart",
            other_confidences: &confidences(&[
                ("the", 0.95),
                ("daemon", 0.95),
                ("will", 0.95),
                ("not", 0.95),
                ("restart", 0.95),
            ]),
            sources: &all,
        })
        .expect("arbitrates");
        assert_eq!(outcome.regions_flipped, 0);
        assert_eq!(
            outcome.rejections,
            vec![ConfidenceArbitrationRejection::MeaningInvertingTokens]
        );
    }

    #[test]
    fn an_inserted_negation_from_a_contraction_is_an_empty_sided_region() {
        // The other side heard "can't" (expands to [can, not]) where the
        // incumbent heard only "can": the negation is a pure insertion, and a
        // pure insertion has no incumbent words to compare confidence
        // against — the shape guard rejects it before any confidence read.
        let all = sources("the job can finish today", "the job can't finish today");
        let outcome = arbitrate(ArbitrationInput {
            incumbent_text: "the job can finish today",
            incumbent_confidences: &confidences(&[
                ("the", 0.9),
                ("job", 0.9),
                ("can", 0.3),
                ("finish", 0.9),
                ("today", 0.9),
            ]),
            other_text: "the job can't finish today",
            other_confidences: &confidences(&[
                ("the", 0.95),
                ("job", 0.95),
                ("can't", 0.95),
                ("finish", 0.95),
                ("today", 0.95),
            ]),
            sources: &all,
        })
        .expect("arbitrates");
        assert_eq!(outcome.regions_flipped, 0);
        assert_eq!(
            outcome.rejections,
            vec![ConfidenceArbitrationRejection::RegionShapeUnsafe]
        );
    }

    #[test]
    fn digits_and_number_words_are_never_flipped() {
        // "three" ↔ "free": a quantity flip.
        let all = sources("retry after three seconds", "retry after free seconds");
        let outcome = arbitrate(ArbitrationInput {
            incumbent_text: "retry after free seconds",
            incumbent_confidences: &confidences(&[
                ("retry", 0.9),
                ("after", 0.9),
                ("free", 0.3),
                ("seconds", 0.9),
            ]),
            other_text: "retry after three seconds",
            other_confidences: &confidences(&[
                ("retry", 0.95),
                ("after", 0.95),
                ("three", 0.95),
                ("seconds", 0.95),
            ]),
            sources: &all,
        })
        .expect("arbitrates");
        assert_eq!(outcome.regions_flipped, 0);
        assert_eq!(
            outcome.rejections,
            vec![ConfidenceArbitrationRejection::MeaningInvertingTokens]
        );
    }

    #[test]
    fn a_numeric_token_is_never_flipped() {
        let all = sources("build 104 shipped", "build roy four shipped");
        let outcome = arbitrate(ArbitrationInput {
            incumbent_text: "build roy four shipped",
            incumbent_confidences: &confidences(&[
                ("build", 0.9),
                ("roy", 0.2),
                ("four", 0.2),
                ("shipped", 0.9),
            ]),
            other_text: "build 104 shipped",
            other_confidences: &confidences(&[("build", 0.95), ("104", 0.95), ("shipped", 0.95)]),
            sources: &all,
        })
        .expect("arbitrates");
        // The region also changes the count by one (admissible), but "104"
        // carries a digit and "four" is a number word: forbidden either way.
        assert_eq!(outcome.regions_flipped, 0);
        assert_eq!(
            outcome.rejections,
            vec![ConfidenceArbitrationRejection::MeaningInvertingTokens]
        );
    }

    #[test]
    fn a_polarity_word_is_never_flipped() {
        let all = sources("the tests pass correct now", "the tests pass banana now");
        let outcome = arbitrate(ArbitrationInput {
            incumbent_text: "the tests pass banana now",
            incumbent_confidences: &confidences(&[
                ("the", 0.9),
                ("tests", 0.9),
                ("pass", 0.9),
                ("banana", 0.3),
                ("now", 0.9),
            ]),
            other_text: "the tests pass correct now",
            other_confidences: &confidences(&[
                ("the", 0.95),
                ("tests", 0.95),
                ("pass", 0.95),
                ("correct", 0.95),
                ("now", 0.95),
            ]),
            sources: &all,
        })
        .expect("arbitrates");
        assert_eq!(outcome.regions_flipped, 0);
        assert_eq!(
            outcome.rejections,
            vec![ConfidenceArbitrationRejection::MeaningInvertingTokens]
        );
    }

    #[test]
    fn a_question_word_is_never_flipped() {
        let all = sources("ask when the build runs", "ask wren the build runs");
        let outcome = arbitrate(ArbitrationInput {
            incumbent_text: "ask wren the build runs",
            incumbent_confidences: &confidences(&[
                ("ask", 0.9),
                ("wren", 0.3),
                ("the", 0.9),
                ("build", 0.9),
                ("runs", 0.9),
            ]),
            other_text: "ask when the build runs",
            other_confidences: &confidences(&[
                ("ask", 0.95),
                ("when", 0.95),
                ("the", 0.95),
                ("build", 0.95),
                ("runs", 0.95),
            ]),
            sources: &all,
        })
        .expect("arbitrates");
        assert_eq!(outcome.regions_flipped, 0);
        assert_eq!(
            outcome.rejections,
            vec![ConfidenceArbitrationRejection::MeaningInvertingTokens]
        );
    }

    // ─── Shape, masks, provenance ───────────────────────────────────────────

    #[test]
    fn a_region_straddling_a_contraction_boundary_is_rejected() {
        // The incumbent's "can't" expands to [can, not]; the divergence
        // against "cannot" is the whole expanded span [can, not] ↔
        // [cannot]. The region carries a negation either way, so the
        // meaning guard rejects the flip before any confidence is read.
        let all = sources("they cannot attend today", "they can't attend today");
        let outcome = arbitrate(ArbitrationInput {
            incumbent_text: "they can't attend today",
            incumbent_confidences: &confidences(&[
                ("they", 0.9),
                ("can't", 0.2),
                ("attend", 0.9),
                ("today", 0.9),
            ]),
            other_text: "they cannot attend today",
            other_confidences: &confidences(&[
                ("they", 0.95),
                ("cannot", 0.95),
                ("attend", 0.95),
                ("today", 0.95),
            ]),
            sources: &all,
        })
        .expect("arbitrates");
        assert_eq!(outcome.regions_flipped, 0);
        assert_eq!(
            outcome.rejections,
            vec![ConfidenceArbitrationRejection::MeaningInvertingTokens]
        );
    }

    #[test]
    fn a_count_change_beyond_the_bound_is_rejected() {
        let all = sources("ship the new cache layer today", "ship cache layer");
        let outcome = arbitrate(ArbitrationInput {
            incumbent_text: "ship cache layer",
            incumbent_confidences: &confidences(&[("ship", 0.3), ("cache", 0.3), ("layer", 0.3)]),
            other_text: "ship the new cache layer today",
            other_confidences: &confidences(&[
                ("ship", 0.95),
                ("the", 0.95),
                ("new", 0.95),
                ("cache", 0.95),
                ("layer", 0.95),
                ("today", 0.95),
            ]),
            sources: &all,
        })
        .expect("arbitrates");
        assert_eq!(outcome.regions_flipped, 0);
        assert_eq!(
            outcome.rejections,
            vec![
                ConfidenceArbitrationRejection::RegionShapeUnsafe,
                ConfidenceArbitrationRejection::RegionShapeUnsafe,
            ],
            "both pure-insertion runs have no incumbent words to vouch for them"
        );
    }

    #[test]
    fn a_region_with_a_word_count_change_beyond_the_bound_is_rejected() {
        // One aligned substitution plus two other-side insertions: both sides
        // non-empty, but the count changes by two — beyond the documented
        // bound — so the decisive confidence gap alone may not buy the flip.
        let all = sources("ship the new fast cache", "ship big cache");
        let outcome = arbitrate(ArbitrationInput {
            incumbent_text: "ship big cache",
            incumbent_confidences: &confidences(&[("ship", 0.3), ("big", 0.2), ("cache", 0.3)]),
            other_text: "ship the new fast cache",
            other_confidences: &confidences(&[
                ("ship", 0.95),
                ("the", 0.95),
                ("new", 0.95),
                ("fast", 0.95),
                ("cache", 0.95),
            ]),
            sources: &all,
        })
        .expect("arbitrates");
        assert_eq!(outcome.regions_flipped, 0);
        assert_eq!(
            outcome.rejections,
            vec![ConfidenceArbitrationRejection::RegionShapeUnsafe]
        );
    }

    #[test]
    fn a_masked_incumbent_region_is_never_flipped() {
        // The divergence sits inside a spoken quote pair in the incumbent.
        let all = sources(
            "the quote cache unquote value",
            "the quote cash unquote value",
        );
        let outcome = arbitrate(ArbitrationInput {
            incumbent_text: "the quote cash unquote value",
            incumbent_confidences: &confidences(&[
                ("the", 0.3),
                ("quote", 0.9),
                ("cash", 0.3),
                ("unquote", 0.9),
                ("value", 0.3),
            ]),
            other_text: "the quote cache unquote value",
            other_confidences: &confidences(&[
                ("the", 0.95),
                ("quote", 0.95),
                ("cache", 0.95),
                ("unquote", 0.95),
                ("value", 0.95),
            ]),
            sources: &all,
        })
        .expect("arbitrates");
        assert_eq!(outcome.regions_flipped, 0);
        assert_eq!(
            outcome.rejections,
            vec![ConfidenceArbitrationRejection::MaskedSpan]
        );
    }

    #[test]
    fn a_masked_other_side_region_is_never_flipped() {
        // Say-the-words span on the OTHER side: what it "heard" there was the
        // user talking ABOUT words, not dictating them.
        let all = sources(
            "say the words cache out loud",
            "say the words cash out loud",
        );
        let outcome = arbitrate(ArbitrationInput {
            incumbent_text: "say the words cash out loud",
            incumbent_confidences: &confidences(&[
                ("say", 0.3),
                ("the", 0.3),
                ("words", 0.3),
                ("cash", 0.3),
                ("out", 0.3),
                ("loud", 0.3),
            ]),
            other_text: "say the words cache out loud",
            other_confidences: &confidences(&[
                ("say", 0.95),
                ("the", 0.95),
                ("words", 0.95),
                ("cache", 0.95),
                ("out", 0.95),
                ("loud", 0.95),
            ]),
            sources: &all,
        })
        .expect("arbitrates");
        assert_eq!(outcome.regions_flipped, 0);
        assert_eq!(
            outcome.rejections,
            vec![ConfidenceArbitrationRejection::MaskedSpan]
        );
    }

    #[test]
    fn a_candidate_with_words_no_provider_heard_is_rejected_wholesale() {
        // Defensive: the splice is built from provider words, but the final
        // source-derived gate must reject the whole candidate if that ever
        // stops holding — no flip may ship invented text.
        let incumbent_only = vec![SourceTranscript {
            provider: Provider::Groq,
            text: "deploy the cash migration".to_owned(),
        }];
        let outcome = arbitrate(ArbitrationInput {
            incumbent_text: "deploy the cash migration",
            incumbent_confidences: &confidences(&[
                ("deploy", 0.9),
                ("the", 0.9),
                ("cash", 0.2),
                ("migration", 0.9),
            ]),
            other_text: "deploy the cache migration",
            other_confidences: &confidences(&[
                ("deploy", 0.9),
                ("the", 0.9),
                ("cache", 0.95),
                ("migration", 0.9),
            ]),
            sources: &incumbent_only,
        })
        .expect("arbitrates");
        assert_eq!(outcome.regions_flipped, 0);
        assert!(
            outcome
                .rejections
                .contains(&ConfidenceArbitrationRejection::CandidateRejected)
        );
        assert_eq!(outcome.text, "deploy the cash migration");
    }

    // ─── Splice rendering ───────────────────────────────────────────────────

    #[test]
    fn boundary_punctuation_is_preserved_across_a_flip() {
        let all = sources(
            "Deploy the cache migration today.",
            "deploy the cash migration today",
        );
        let outcome = arbitrate(ArbitrationInput {
            incumbent_text: "deploy the cash migration today",
            incumbent_confidences: &confidences(&[
                ("deploy", 0.9),
                ("the", 0.9),
                ("cash", 0.2),
                ("migration", 0.9),
                ("today", 0.9),
            ]),
            other_text: "Deploy the cache migration today.",
            other_confidences: &confidences(&[
                ("Deploy.", 0.95),
                ("the", 0.95),
                ("cache", 0.95),
                ("migration", 0.95),
                ("today.", 0.95),
            ]),
            sources: &all,
        })
        .expect("arbitrates");
        assert_eq!(outcome.regions_flipped, 1);
        // The incumbent's rendering is otherwise untouched: no invented
        // capital or period, the Deepgram-side rendering is NOT adopted.
        assert_eq!(outcome.text, "deploy the cache migration today");
    }

    #[test]
    fn a_trailing_sentence_terminal_survives_a_final_word_flip() {
        let all = sources("i like the cache", "i like the cash");
        let outcome = arbitrate(ArbitrationInput {
            incumbent_text: "i like the cash.",
            incumbent_confidences: &confidences(&[
                ("i", 0.9),
                ("like", 0.9),
                ("the", 0.9),
                ("cash", 0.2),
            ]),
            other_text: "i like the cache",
            other_confidences: &confidences(&[
                ("i", 0.95),
                ("like", 0.95),
                ("the", 0.95),
                ("cache", 0.95),
            ]),
            sources: &all,
        })
        .expect("arbitrates");
        assert_eq!(outcome.regions_flipped, 1);
        assert_eq!(outcome.text, "i like the cache.");
    }

    #[test]
    fn two_flips_in_one_pass_are_both_applied() {
        let all = sources(
            "the cache warmed the cache again",
            "the cash warmed the cash again",
        );
        let outcome = arbitrate(ArbitrationInput {
            incumbent_text: "the cash warmed the cash again",
            incumbent_confidences: &confidences(&[
                ("the", 0.3),
                ("cash", 0.2),
                ("warmed", 0.9),
                ("the", 0.3),
                ("cash", 0.2),
                ("again", 0.9),
            ]),
            other_text: "the cache warmed the cache again",
            other_confidences: &confidences(&[
                ("the", 0.95),
                ("cache", 0.95),
                ("warmed", 0.95),
                ("the", 0.95),
                ("cache", 0.95),
                ("again", 0.95),
            ]),
            sources: &all,
        })
        .expect("arbitrates");
        assert_eq!(outcome.regions_considered, 2);
        assert_eq!(outcome.regions_flipped, 2);
        assert_eq!(outcome.text, "the cache warmed the cache again");
    }

    #[test]
    fn zero_divergent_regions_records_a_clean_considered_count() {
        let all = sources("identical words", "identical words");
        let outcome = arbitrate(ArbitrationInput {
            incumbent_text: "identical words",
            incumbent_confidences: &confidences(&[("identical", 0.9), ("words", 0.9)]),
            other_text: "identical words",
            other_confidences: &confidences(&[("identical", 0.9), ("words", 0.9)]),
            sources: &all,
        })
        .expect("arbitrates");
        assert_eq!(outcome.regions_considered, 0);
        assert_eq!(outcome.regions_flipped, 0);
        assert!(outcome.rejections.is_empty());
        assert_eq!(outcome.text, "identical words");
    }

    // The pipeline-level observable behavior (selection gates, byte-identical
    // defaults, correction-gate provenance, diagnostics on the decision) is
    // covered in crates/voisu-core/tests/transcript_decision.rs.
}
