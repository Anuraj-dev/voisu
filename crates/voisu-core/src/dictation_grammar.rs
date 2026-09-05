//! Deterministic dictation grammar: numbers, times, currency, and email-speak
//! (Track B slice B3).
//!
//! This module extends the local spoken-mark family (`local_baseline`) with
//! four conservative, English-only, offline transforms. Every rule is a pure
//! function over whitespace tokens and follows the same contract as the
//! spoken marks: **ambiguous → leave as spoken**. Uncertain text may remain
//! unformatted but is never silently deleted, invented, or reinterpreted
//! (Transcript Fidelity outranks presentation polish).
//!
//! # Pinned decisions (slice B3)
//!
//! - Numbers convert only when the phrase carries an explicit value shape:
//!   a scale word (`hundred` / `thousand`), a decimal marker (`point`), the
//!   two-token year shape (`twenty twenty-four`), or a value context
//!   (currency unit, `o'clock`, am/pm, email shape). Plain quantities in
//!   prose (`twenty seven days`, `I have one question`, `twenty-five`)
//!   stay words — the locked DPR behavior corpus pins
//!   `the period of the moon is twenty seven days` as ordinary prose.
//! - `one hundred percent` → `100 percent` (scale word authorizes);
//!   `fifty percent` stays words (`percent` is not a currency unit).
//! - Values above 9 999 stay spoken (`one million`, `one hundred thousand`);
//!   no thousands separators are invented.
//! - `seven o'clock` → `7 o'clock` (the spoken `o'clock` word is kept; no
//!   `:00` is invented). `noon` / `midnight` stay words. Hours must parse
//!   1–12 and minutes 0–59, else the phrase stays spoken; a time without an
//!   explicit marker (`three thirty` alone) stays spoken.
//! - `ninety-nine cents` → `99 cents` and `twenty euros` → `20 euros`
//!   (unit word preserved; only dollars get a symbol, per slice scope).
//! - `fifty dollars` → `$50` replaces the spoken unit word with `$` — the
//!   one sanctioned deletion, explicitly approved in the B3 slice scope.
//! - Email-speak needs the full email shape with a closed TLD allowlist;
//!   `we met at the dot of dawn` and `worked at a dot com startup` stay
//!   prose.
//!
//! # Span safety
//!
//! Conversions never fire inside `quote … unquote` spans or metalinguistic
//! `say the words … out loud` spans (the same masks the spoken marks use),
//! mid-span tokens must be bare (glued punctuation like `three, pm` fails
//! closed), and a number phrase never starts mid-run (the leftmost number
//! token owns the phrase), so partial rewrites that would mangle a sentence
//! fail closed. Trailing sentence punctuation on the final token of a
//! converted span is preserved, never deleted.

use crate::local_baseline::{ascii_lower, metalinguistic_mask, quote_pairs_and_skip, word_tokens};

/// Largest value dictation grammar renders as digits. Larger spoken amounts
/// (`one million`, `one hundred thousand`) stay words — rendering them would
/// require inventing thousands separators.
const MAX_RENDERED_VALUE: u32 = 9_999;

/// Spoken cardinal smalls (`zero` … `ninety`). Deliberately excludes `oh`
/// (an interjection in prose) and every ordinal (`first`, `fifth`), which
/// are not cardinals and never convert.
const SMALLS: &[(&str, u32)] = &[
    ("zero", 0),
    ("one", 1),
    ("two", 2),
    ("three", 3),
    ("four", 4),
    ("five", 5),
    ("six", 6),
    ("seven", 7),
    ("eight", 8),
    ("nine", 9),
    ("ten", 10),
    ("eleven", 11),
    ("twelve", 12),
    ("thirteen", 13),
    ("fourteen", 14),
    ("fifteen", 15),
    ("sixteen", 16),
    ("seventeen", 17),
    ("eighteen", 18),
    ("nineteen", 19),
    ("twenty", 20),
    ("thirty", 30),
    ("forty", 40),
    ("fifty", 50),
    ("sixty", 60),
    ("seventy", 70),
    ("eighty", 80),
    ("ninety", 90),
];

/// Spoken scale words. `hundred` scales the pending partial; `thousand` and
/// `million` commit it.
const SCALES: &[(&str, u32)] = &[
    ("hundred", 100),
    ("thousand", 1_000),
    ("million", 1_000_000),
];

/// Closed TLD allowlist for email-speak. Unknown TLDs fail closed (stay
/// spoken). Includes RFC 2606 `test` (used across the repo's fixtures) and
/// excludes words that collide with prose (`at`, `am`).
const EMAIL_TLDS: &[&str] = &[
    "com", "net", "org", "edu", "gov", "mil", "int", "io", "ai", "dev", "app", "co", "me", "sh",
    "so", "fm", "tv", "gg", "xyz", "info", "biz", "us", "uk", "ca", "de", "fr", "eu", "jp", "in",
    "au", "nl", "se", "no", "fi", "es", "it", "pt", "pl", "ch", "be", "dk", "ie", "nz", "za",
    "test",
];

/// Cue words that can never serve as email local-part or domain words.
const EMAIL_CUE_WORDS: &[&str] = &["at", "dot", "underscore", "hyphen"];

/// One accepted dictation-grammar rewrite: a half-open token span and the
/// text that replaces it.
struct CueSpan {
    start: usize,
    end: usize,
    replacement: String,
}

/// Apply all dictation-grammar transforms to `text`.
///
/// Pure, deterministic, allocation-only; never panics on odd Unicode and
/// never invents words outside the pinned rewrite set. Idempotent: converted
/// output contains no spoken cue words for the converted span, so a second
/// pass is the identity.
pub(crate) fn apply_dictation_grammar(text: &str) -> String {
    let tokens = word_tokens(text);
    if tokens.is_empty() {
        return text.to_owned();
    }
    let (_pairs, quote_mask) = quote_pairs_and_skip(&tokens);
    let meta = metalinguistic_mask(&tokens);

    let mut out = String::new();
    let mut last_end = 0usize;
    let mut i = 0usize;
    while i < tokens.len() {
        // Quote interiors and metalinguistic spans keep every spoken word.
        if quote_mask[i] || meta[i] {
            i += 1;
            continue;
        }
        // The leftmost number token owns a number phrase; a numeric rewrite
        // never starts mid-run (that would partially rewrite the phrase and
        // mangle the sentence). Email-speak is shape-based, not numeric, so
        // it is still attempted.
        let numberish_prev = i > 0 && is_numberish_token(tokens[i - 1].2);
        let matched = try_email_span(&tokens, i, &quote_mask, &meta);
        let matched = if numberish_prev {
            matched
        } else {
            matched
                .or_else(|| try_time_span(&tokens, i, &quote_mask, &meta))
                .or_else(|| try_currency_span(&tokens, i, &quote_mask, &meta))
                .or_else(|| try_number_span(&tokens, i, &quote_mask, &meta))
        };
        let Some(m) = matched else {
            i += 1;
            continue;
        };
        let start = tokens[m.start].0;
        let end = tokens[m.end - 1].1;
        out.push_str(&text[last_end..start]);
        out.push_str(&m.replacement);
        last_end = end;
        i = m.end;
    }
    if last_end == 0 {
        return text.to_owned();
    }
    out.push_str(&text[last_end..]);
    out
}

// ─── Token helpers ───────────────────────────────────────────────────────────

/// Strip a short run of sentence punctuation from the token tail. Returns
/// `(core, suffix)`; the suffix is re-appended after a rewrite so no spoken
/// punctuation is ever deleted.
fn token_core(tok: &str) -> (&str, &str) {
    let core_len = tok.trim_end_matches([',', '.', '!', '?', ';', ':']).len();
    tok.split_at(core_len)
}

fn small_value(word: &str) -> Option<u32> {
    SMALLS.iter().find(|(w, _)| *w == word).map(|(_, v)| *v)
}

fn scale_value(word: &str) -> Option<u32> {
    SCALES.iter().find(|(w, _)| *w == word).map(|(_, v)| *v)
}

/// Small value of one spoken word, including hyphenated tens-unit compounds
/// (`twenty-four` → 24). Only canonical compounds parse: `one-two` and
/// `twenty-twenty` fail closed.
fn small_token_value(word: &str) -> Option<u32> {
    if let Some(v) = small_value(word) {
        return Some(v);
    }
    let (tens, unit) = word.split_once('-')?;
    let tens_v = small_value(tens)?;
    let unit_v = small_value(unit)?;
    (tens_v >= 20 && tens_v % 10 == 0 && unit_v < 10).then_some(tens_v + unit_v)
}

/// Value of one spoken cardinal token, including scale words. Ordinals
/// (`first`, `fifth`) are not in the tables and never match.
fn cardinal_value(tok: &str) -> Option<u32> {
    let (core, _) = token_core(tok);
    if core.is_empty() {
        return None;
    }
    let lower = ascii_lower(core);
    small_token_value(&lower).or_else(|| scale_value(&lower))
}

fn is_cardinal_token(tok: &str) -> bool {
    cardinal_value(tok).is_some()
}

/// True when a token could be part of a spoken number phrase (cardinal word
/// or a decimal `point`). Used to keep numeric rewrites away from mid-run
/// start positions.
fn is_numberish_token(tok: &str) -> bool {
    let (core, _) = token_core(tok);
    !core.is_empty() && (is_cardinal_token(core) || ascii_lower(core) == "point")
}

/// Parse a run of spoken cardinal words in canonical order.
///
/// Fails closed on non-canonical sequences (`three twenty`, `five fifteen`,
/// `thousand thousand`) so ambiguous speech is never reinterpreted. Returns
/// the composed value (`one hundred five` → 105, `twenty five hundred` →
/// 2500, `one hundred thousand` → 100 000).
fn parse_cardinal_run(words: &[String]) -> Option<u32> {
    let mut total: u32 = 0;
    let mut current: u32 = 0;
    let mut first = true;
    let mut prev_small: Option<u32> = None;
    for word in words {
        if let Some(scale) = scale_value(word) {
            // A scale needs either a pending small (`three hundred`) or to be
            // the run head (`hundred dollars`). `thousand thousand` fails.
            if current == 0 && !first {
                return None;
            }
            if scale == 100 {
                current = current.max(1) * 100;
            } else {
                total += current.max(1) * scale;
                current = 0;
            }
            prev_small = None;
        } else {
            let v = small_token_value(word)?;
            // Canonical small-after-small only: tens then unit (`twenty
            // five`). `three twenty`, `five fifteen` fail closed.
            if let Some(p) = prev_small
                && !(p >= 20 && p % 10 == 0 && v < 10)
            {
                return None;
            }
            current += v;
            prev_small = Some(v);
        }
        first = false;
    }
    Some(total + current)
}

/// Digit string for one spoken fraction word. Fractions accept only
/// unambiguous single digit words (`oh` → "0", `zero` → "0", `four` → "4");
/// tens and teens in fraction position are ambiguous digit-vs-value speech
/// (`point forty five`) and fail closed.
fn digit_string(word: &str) -> Option<String> {
    if word == "oh" {
        return Some("0".to_owned());
    }
    small_value(word).filter(|v| *v < 10).map(|v| v.to_string())
}

/// True when every token in `start..end` sits outside both protective masks.
fn span_is_clear(quote_mask: &[bool], meta: &[bool], start: usize, end: usize) -> bool {
    end <= quote_mask.len() && !(start..end).any(|k| quote_mask[k] || meta[k])
}

/// True when every non-final token in `start..end` carries no glued
/// punctuation. Final-token punctuation is preserved by the rewrite.
fn span_interior_bare(tokens: &[(usize, usize, &str)], start: usize, end: usize) -> bool {
    (start..end.saturating_sub(1)).all(|k| token_core(tokens[k].2).1.is_empty())
}

// ─── Numbers ─────────────────────────────────────────────────────────────────

/// Number phrases: decimal (`three point one four` → `3.14`, `point five` →
/// `0.5`), year shape (`twenty twenty-four` → `2024`), and scale runs
/// (`one hundred five` → `105`, `two thousand five` → `2005`).
///
/// Plain quantities without a scale/point/year shape (`twenty seven days`,
/// `I have one question`) stay words; values above [`MAX_RENDERED_VALUE`]
/// stay words; `one hundred and five` fails closed (`and` continuation).
fn try_number_span(
    tokens: &[(usize, usize, &str)],
    i: usize,
    quote_mask: &[bool],
    meta: &[bool],
) -> Option<CueSpan> {
    try_decimal_span(tokens, i, quote_mask, meta)
        .or_else(|| try_cardinal_run(tokens, i, quote_mask, meta))
}

/// `[cardinals] point [digits]` → decimal. The integer part may be empty
/// only when the phrase starts at `point` (`point five` → `0.5`); a `point`
/// directly after `exclamation` is the bang cue, not a decimal marker.
fn try_decimal_span(
    tokens: &[(usize, usize, &str)],
    i: usize,
    quote_mask: &[bool],
    meta: &[bool],
) -> Option<CueSpan> {
    let mut j = i;
    while j < tokens.len() && is_cardinal_token(tokens[j].2) {
        j += 1;
    }
    let int_words: Vec<String> = tokens[i..j]
        .iter()
        .map(|t| ascii_lower(token_core(t.2).0))
        .collect();

    let point_tok = tokens.get(j)?;
    let (point_core, point_suffix) = token_core(point_tok.2);
    if !point_suffix.is_empty() || ascii_lower(point_core) != "point" {
        return None;
    }
    // `exclamation point …` is the bang cue, not a decimal.
    if i > 0 {
        let (prev_core, _) = token_core(tokens[i - 1].2);
        if ascii_lower(prev_core) == "exclamation" {
            return None;
        }
    }
    j += 1;

    let mut frac = String::new();
    while let Some(tok) = tokens.get(j) {
        let (core, suffix) = token_core(tok.2);
        if !suffix.is_empty() {
            break;
        }
        let Some(digits) = digit_string(&ascii_lower(core)) else {
            break;
        };
        frac.push_str(&digits);
        j += 1;
    }
    if frac.is_empty() || frac.len() > 6 {
        return None;
    }
    let int_value = if int_words.is_empty() {
        0
    } else {
        // A bare leading scale (`hundred point five`) is not canonical
        // speech — fail closed.
        if scale_value(&int_words[0]).is_some() {
            return None;
        }
        let value = parse_cardinal_run(&int_words)?;
        if value > MAX_RENDERED_VALUE {
            return None;
        }
        value
    };
    if !span_is_clear(quote_mask, meta, i, j) || !span_interior_bare(tokens, i, j) {
        return None;
    }
    let final_suffix = token_core(tokens[j - 1].2).1;
    Some(CueSpan {
        start: i,
        end: j,
        replacement: format!("{int_value}.{frac}{final_suffix}"),
    })
}

/// True when the token is a time-of-day marker (`am`, `pm`, `o'clock`,
/// `oclock`, `o`). A number run followed by one is a failed time attempt
/// (`thirteen thirty pm`) and must stay spoken rather than become digits.
fn is_time_marker_token(tok: &str) -> bool {
    let (core, _) = token_core(tok);
    matches!(
        ascii_lower(core).as_str(),
        "am" | "pm" | "o'clock" | "oclock" | "o"
    )
}

/// Cardinal runs with an explicit value shape.
fn try_cardinal_run(
    tokens: &[(usize, usize, &str)],
    i: usize,
    quote_mask: &[bool],
    meta: &[bool],
) -> Option<CueSpan> {
    let mut j = i;
    while j < tokens.len() && is_cardinal_token(tokens[j].2) {
        j += 1;
    }
    let len = j - i;
    if len == 0 {
        return None;
    }
    let words: Vec<String> = tokens[i..j]
        .iter()
        .map(|t| ascii_lower(token_core(t.2).0))
        .collect();

    // A run followed by a time marker is a failed time attempt
    // (`thirteen thirty pm`) — never re-read it as digits.
    if tokens.get(j).is_some_and(|t| is_time_marker_token(t.2)) {
        return None;
    }

    // Year shape: exactly two tokens, both 10..99 (`twenty twenty-four`,
    // `nineteen ninety-nine`). Scale words do not parse as smalls, so
    // `twenty hundred` never reaches this branch.
    if len == 2
        && let (Some(a), Some(b)) = (small_token_value(&words[0]), small_token_value(&words[1]))
        && a >= 10
        && b >= 10
        && span_is_clear(quote_mask, meta, i, j)
        && span_interior_bare(tokens, i, j)
    {
        let final_suffix = token_core(tokens[j - 1].2).1;
        return Some(CueSpan {
            start: i,
            end: j,
            replacement: format!("{}{final_suffix}", a * 100 + b),
        });
    }

    // Scale runs: at least one scale word, canonical order, head not a scale
    // word (`hundred one` fails), value within the renderable range.
    // `one hundred and five` fails closed: a partial rewrite would leave
    // `100 and five`, so the whole phrase stays spoken.
    if len >= 2
        && scale_value(&words[0]).is_none()
        && words.iter().any(|w| scale_value(w).is_some())
    {
        let and_continues = tokens.get(j).is_some_and(|t| {
            let (core, _) = token_core(t.2);
            ascii_lower(core) == "and"
        }) && tokens.get(j + 1).is_some_and(|t| is_cardinal_token(t.2));
        if and_continues {
            return None;
        }
        let value = parse_cardinal_run(&words)?;
        if value == 0 || value > MAX_RENDERED_VALUE {
            return None;
        }
        if !span_is_clear(quote_mask, meta, i, j) || !span_interior_bare(tokens, i, j) {
            return None;
        }
        let final_suffix = token_core(tokens[j - 1].2).1;
        return Some(CueSpan {
            start: i,
            end: j,
            replacement: format!("{value}{final_suffix}"),
        });
    }
    None
}

// ─── Times ───────────────────────────────────────────────────────────────────

/// Times of day. Accepted shapes (hour must parse 1–12, minute 0–59, else
/// the phrase stays spoken):
///
/// - `H o'clock [am|pm]` → `H o'clock [am|pm]` (spoken `o'clock` kept)
/// - `H [minutes] am|pm` → `H[:MM] am|pm`
///
/// `H minutes` without a marker (`three thirty` alone) stays spoken, as do
/// `noon` / `midnight`.
fn try_time_span(
    tokens: &[(usize, usize, &str)],
    i: usize,
    quote_mask: &[bool],
    meta: &[bool],
) -> Option<CueSpan> {
    let (hour_core, _) = token_core(tokens[i].2);
    let hour = small_value(&ascii_lower(hour_core)).filter(|h| (1..=12).contains(h))?;
    let mut j = i + 1;

    // Optional spoken `o'clock` (`o'clock` / `oclock` / `o clock`).
    let mut oclock: Option<String> = None;
    if let Some(tok) = tokens.get(j) {
        let (core, suffix) = token_core(tok.2);
        let lower = ascii_lower(core);
        if lower == "o'clock" || lower == "oclock" {
            oclock = Some(core.to_owned());
            j += 1;
        } else if suffix.is_empty()
            && lower == "o"
            && tokens
                .get(j + 1)
                .is_some_and(|t| ascii_lower(token_core(t.2).0) == "clock")
        {
            let next = tokens.get(j + 1)?;
            let (next_core, next_suffix) = token_core(next.2);
            if !next_suffix.is_empty() {
                return None;
            }
            oclock = Some(format!("{core} {next_core}"));
            j += 2;
        }
    }

    // Optional minutes, only when there was no `o'clock`.
    let mut minute: Option<u32> = None;
    if oclock.is_none()
        && let Some((value, next)) = parse_minute_tokens(tokens, j)
    {
        minute = Some(value);
        j = next;
    }

    // Optional am/pm marker (`pm` itself is preserved as spoken).
    let mut meridiem: Option<String> = None;
    if let Some(tok) = tokens.get(j) {
        let (core, _) = token_core(tok.2);
        let lower = ascii_lower(core);
        if lower == "am" || lower == "pm" {
            meridiem = Some(core.to_owned());
            j += 1;
        }
    }

    // Without an explicit time marker the phrase stays spoken
    // (`three thirty` alone, `twelve noon`).
    if oclock.is_none() && meridiem.is_none() {
        return None;
    }
    if let Some(m) = minute
        && m > 59
    {
        return None;
    }
    if !span_is_clear(quote_mask, meta, i, j) || !span_interior_bare(tokens, i, j) {
        return None;
    }

    let final_suffix = token_core(tokens[j - 1].2).1;
    let mut rendered = match minute {
        Some(m) => format!("{hour}:{m:02}"),
        None => hour.to_string(),
    };
    if let Some(spoken) = oclock {
        rendered = format!("{rendered} {spoken}");
    }
    if let Some(spoken) = meridiem {
        rendered = format!("{rendered} {spoken}");
    }
    rendered.push_str(final_suffix);
    Some(CueSpan {
        start: i,
        end: j,
        replacement: rendered,
    })
}

/// Parse minute words at `j`: teens (`fifteen`), tens with optional unit
/// (`forty-five`, `forty five`), or `oh`/`zero` + unit (`oh five` → 5).
/// Lone units (`three five pm`) fail closed. `oh`/`zero` alone fail too.
fn parse_minute_tokens(tokens: &[(usize, usize, &str)], j: usize) -> Option<(u32, usize)> {
    let tok = tokens.get(j)?;
    let (core, suffix) = token_core(tok.2);
    if !suffix.is_empty() {
        return None;
    }
    let lower = ascii_lower(core);
    if lower == "oh" || lower == "zero" {
        let next = tokens.get(j + 1)?;
        let (next_core, next_suffix) = token_core(next.2);
        if !next_suffix.is_empty() {
            return None;
        }
        let unit = small_value(&ascii_lower(next_core)).filter(|v| (1..=9).contains(v))?;
        return Some((unit, j + 2));
    }
    let value = small_token_value(&lower)?;
    if (10..=19).contains(&value) {
        return Some((value, j + 1));
    }
    if value >= 20 {
        if let Some(next) = tokens.get(j + 1) {
            let (next_core, next_suffix) = token_core(next.2);
            if next_suffix.is_empty()
                && let Some(unit) = small_value(&ascii_lower(next_core)).filter(|v| *v < 10)
            {
                return Some((value + unit, j + 2));
            }
        }
        return Some((value, j + 1));
    }
    None
}

// ─── Currency ────────────────────────────────────────────────────────────────

/// How a currency unit renders. `Dollar` replaces the spoken unit word with
/// the `$` symbol (the one sanctioned deletion, pinned in the slice scope);
/// `Preserved` keeps the spoken unit word (`20 euros`, `99 cents`).
#[derive(Clone, Copy)]
enum CurrencyUnit {
    Dollar,
    Preserved,
}

fn currency_unit(word: &str) -> Option<CurrencyUnit> {
    match ascii_lower(word).as_str() {
        "dollar" | "dollars" => Some(CurrencyUnit::Dollar),
        "euro" | "euros" | "cent" | "cents" => Some(CurrencyUnit::Preserved),
        _ => None,
    }
}

/// `[cardinals] dollar(s)` → `$N`; `[cardinals] euro(s)|cent(s)` → `N unit`.
/// The unit context authorizes even a single number word (`one dollar`);
/// non-canonical runs (`three twenty dollars`), amounts above
/// [`MAX_RENDERED_VALUE`], and `X dollars and Y cents` continuations fail
/// closed and stay spoken.
fn try_currency_span(
    tokens: &[(usize, usize, &str)],
    i: usize,
    quote_mask: &[bool],
    meta: &[bool],
) -> Option<CueSpan> {
    let mut j = i;
    while j < tokens.len() && is_cardinal_token(tokens[j].2) && j - i < 6 {
        j += 1;
    }
    if j == i {
        return None;
    }
    let unit_tok = tokens.get(j)?;
    let (unit_core, unit_suffix) = token_core(unit_tok.2);
    let unit = currency_unit(unit_core)?;

    // Two-sided continuation ambiguity: `X dollars and Y cents`,
    // `X dollars Y cents`, or a dangling `X dollars and` — a partial rewrite
    // would mangle the phrase, so all of it stays spoken. This phrase is a
    // continuation when the token before it is a unit word, or `and` after
    // a unit word.
    if i > 0 && currency_unit(token_core(tokens[i - 1].2).0).is_some() {
        return None;
    }
    if i >= 2
        && ascii_lower(token_core(tokens[i - 1].2).0) == "and"
        && currency_unit(token_core(tokens[i - 2].2).0).is_some()
    {
        return None;
    }
    let mut k = j + 1;
    let mut saw_and = false;
    if tokens
        .get(k)
        .is_some_and(|t| ascii_lower(token_core(t.2).0) == "and")
    {
        saw_and = true;
        k += 1;
    }
    if saw_and && tokens.get(k).is_none() {
        return None;
    }
    if tokens.get(k).is_some_and(|t| is_cardinal_token(t.2))
        && tokens
            .get(k + 1)
            .is_some_and(|t| currency_unit(token_core(t.2).0).is_some())
    {
        return None;
    }

    let words: Vec<String> = tokens[i..j]
        .iter()
        .map(|t| ascii_lower(token_core(t.2).0))
        .collect();
    let value = parse_cardinal_run(&words)?;
    if value > MAX_RENDERED_VALUE {
        return None;
    }
    if !span_is_clear(quote_mask, meta, i, j + 1) || !span_interior_bare(tokens, i, j + 1) {
        return None;
    }
    let replacement = match unit {
        CurrencyUnit::Dollar => format!("${value}{unit_suffix}"),
        CurrencyUnit::Preserved => format!("{value} {unit_core}{unit_suffix}"),
    };
    Some(CueSpan {
        start: i,
        end: j + 1,
        replacement,
    })
}

// ─── Email-speak ─────────────────────────────────────────────────────────────

/// Email-speak → address, only when the full email shape matches:
///
/// `local ("dot"|"underscore"|"hyphen" local){0,2} "at" label
/// ("dot"|"hyphen" label)+ TLD`
///
/// - the local part is 1–3 plain words (`[a-z0-9_-]`, spoken cues allowed);
/// - domain labels are plain `[a-z0-9-]` words of length ≥ 2 (no underscore,
///   no cue words as labels);
/// - the last label must be in the closed [`EMAIL_TLDS`] allowlist;
/// - the first `at` that fits the shape wins (`email me at foo at bar dot
///   com` → `email me at foo@bar.com`);
/// - a bare single-word local part (no digit/`_`/`-`/dot cue) only converts
///   when the address is the whole utterance or an email cue / spoken `at`
///   chain introduces it — mid-sentence `goal look at example dot com` is a
///   website reference and keeps the spoken `at` (pinned by the DPR
///   pipeline). The residual limitation: a bare whole-utterance `look at
///   example dot com` still matches the approved shape spec and converts;
///   distinguishing it needs semantics the local lane must not guess;
/// - everything else — `we met at the dot of dawn`, `worked at a dot com
///   startup`, unknown TLDs — stays spoken.
fn try_email_span(
    tokens: &[(usize, usize, &str)],
    i: usize,
    quote_mask: &[bool],
    meta: &[bool],
) -> Option<CueSpan> {
    // Local part: word (cue word){0,2} then `at`.
    let mut local_words: Vec<&str> = Vec::new();
    let mut local_cues: Vec<&str> = Vec::new();
    let mut k = i;
    loop {
        let tok = tokens.get(k)?;
        let (core, suffix) = token_core(tok.2);
        if !suffix.is_empty() || !is_email_local_word(core) {
            return None;
        }
        local_words.push(core);
        k += 1;
        if tokens
            .get(k)
            .is_some_and(|t| ascii_lower(token_core(t.2).0) == "at")
        {
            break;
        }
        let cue_tok = tokens.get(k)?;
        let (cue_core, cue_suffix) = token_core(cue_tok.2);
        if !cue_suffix.is_empty()
            || !matches!(
                ascii_lower(cue_core).as_str(),
                "dot" | "underscore" | "hyphen"
            )
        {
            return None;
        }
        local_cues.push(cue_core);
        if local_cues.len() > 2 {
            return None;
        }
        k += 1;
    }
    let at_index = k;
    // The spoken `at` itself must be bare (`at,` mid-span fails closed).
    let (at_core, at_suffix) = token_core(tokens[at_index].2);
    if !at_suffix.is_empty() || ascii_lower(at_core) != "at" {
        return None;
    }

    // Domain: label (dot|hyphen label)+ with the final label a TLD.
    let mut labels: Vec<&str> = Vec::new();
    let mut domain_cues: Vec<String> = Vec::new();
    k = at_index + 1;
    loop {
        let tok = tokens.get(k)?;
        let (core, suffix) = token_core(tok.2);
        if !is_email_domain_word(core) {
            return None;
        }
        let continues = tokens.get(k + 1).is_some_and(|t| {
            let (cue_core, cue_suffix) = token_core(t.2);
            cue_suffix.is_empty() && matches!(ascii_lower(cue_core).as_str(), "dot" | "hyphen")
        });
        if continues {
            if !suffix.is_empty() {
                return None;
            }
            labels.push(core);
            let cue_tok = tokens.get(k + 1)?;
            domain_cues.push(ascii_lower(token_core(cue_tok.2).0));
            k += 2;
            if labels.len() > 4 {
                return None;
            }
        } else {
            // Final label: it may carry one trailing sentence comma/period,
            // which is preserved after the address.
            if !matches!(suffix, "" | "," | ".") {
                return None;
            }
            labels.push(core);
            k += 1;
            break;
        }
    }
    if labels.len() < 2 || !EMAIL_TLDS.contains(&ascii_lower(labels[labels.len() - 1]).as_str()) {
        return None;
    }
    if !span_is_clear(quote_mask, meta, i, k) {
        return None;
    }

    // Local-part authorization. A bare single-word local part with no
    // technical character is the most prose-ambiguous shape — `look at
    // example dot com` is a website reference, not an address — so it only
    // converts when the address is the whole utterance, or an email cue
    // (`email` / `mail` / `address`) or a spoken `at` chain introduces it.
    // Technical or multi-word local parts (digits, `_`, `-`, dot cues) are
    // trusted anywhere.
    let local_is_technical = !local_cues.is_empty()
        || local_words.iter().any(|w| {
            w.chars()
                .any(|c| c.is_ascii_digit() || c == '_' || c == '-')
        });
    let whole_utterance = i == 0 && k == tokens.len();
    let introduced = i > 0
        && matches!(
            ascii_lower(token_core(tokens[i - 1].2).0).as_str(),
            "email" | "mail" | "address" | "at"
        );
    if !local_is_technical && !whole_utterance && !introduced {
        return None;
    }

    // Rebuild the address, preserving the spoken casing of each word.
    let mut address = String::new();
    for (idx, word) in local_words.iter().enumerate() {
        if idx > 0 {
            match ascii_lower(local_cues[idx - 1]) {
                cue if cue == "underscore" => address.push('_'),
                cue if cue == "hyphen" => address.push('-'),
                _ => address.push('.'),
            }
        }
        address.push_str(word);
    }
    address.push('@');
    for (idx, label) in labels.iter().enumerate() {
        if idx > 0 {
            address.push_str(if domain_cues[idx - 1] == "hyphen" {
                "-"
            } else {
                "."
            });
        }
        address.push_str(label);
    }
    let (_, final_suffix) = token_core(tokens[k - 1].2);
    address.push_str(final_suffix);

    Some(CueSpan {
        start: i,
        end: k,
        replacement: address,
    })
}

fn is_email_local_word(tok: &str) -> bool {
    !EMAIL_CUE_WORDS.contains(&ascii_lower(tok).as_str())
        && !tok.is_empty()
        && tok
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && tok.chars().any(|c| c.is_ascii_alphanumeric())
}

fn is_email_domain_word(tok: &str) -> bool {
    !EMAIL_CUE_WORDS.contains(&ascii_lower(tok).as_str())
        && tok.len() >= 2
        && tok.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && tok.chars().any(|c| c.is_ascii_alphanumeric())
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_baseline::{LocalBaselineOptions, organize_local_baseline};
    use crate::prompt_rendering::{RenderingPolicy, RenderingRoute};

    fn adaptive() -> LocalBaselineOptions {
        LocalBaselineOptions {
            policy: RenderingPolicy::Adaptive,
            route: RenderingRoute::DeterministicLocal,
            timing: None,
        }
    }

    fn literal() -> LocalBaselineOptions {
        LocalBaselineOptions {
            policy: RenderingPolicy::Adaptive,
            route: RenderingRoute::LiteralIdentity,
            timing: None,
        }
    }

    fn grammar(src: &str) -> String {
        apply_dictation_grammar(src)
    }

    /// Ordinary-prose guardrail: the grammar pass leaves the source alone
    /// and the organized baseline is just sentence polish.
    fn assert_organize_unchanged(src: &str) {
        assert_eq!(grammar(src), src, "{src:?} must not convert");
        let mut expected = src.to_owned();
        if let Some(first) = expected.get(..1) {
            let rest = expected.get(1..).unwrap_or_default();
            expected = format!("{}{rest}", first.to_ascii_uppercase());
        }
        if !expected.ends_with(['.', '!', '?']) {
            expected.push('.');
        }
        let b = organize_local_baseline(src, &adaptive());
        assert_eq!(b.rendered(), expected, "{src:?} must stay ordinary prose");
    }

    // ── Numbers: positive conversions ──

    #[test]
    fn year_shapes_convert() {
        assert_eq!(grammar("twenty twenty-four"), "2024");
        assert_eq!(grammar("nineteen ninety-nine"), "1999");
        assert_eq!(grammar("twenty twenty"), "2020");
        assert_eq!(grammar("twenty ten"), "2010");
    }

    #[test]
    fn scale_runs_convert() {
        assert_eq!(grammar("one hundred five"), "105");
        assert_eq!(grammar("one hundred twenty-three"), "123");
        assert_eq!(grammar("two thousand five"), "2005");
        assert_eq!(grammar("one hundred"), "100");
        assert_eq!(grammar("twenty five hundred"), "2500");
    }

    #[test]
    fn decimals_convert() {
        assert_eq!(grammar("point five"), "0.5");
        assert_eq!(grammar("three point one four"), "3.14");
        assert_eq!(grammar("zero point five"), "0.5");
        assert_eq!(grammar("twenty point five"), "20.5");
        assert_eq!(grammar("three point oh five"), "3.05");
    }

    #[test]
    fn tens_in_fraction_position_fail_closed() {
        // `point forty five` is ambiguous digit-vs-value speech; leaving it
        // spoken is the fail-closed pin.
        assert_eq!(grammar("three point forty five"), "three point forty five");
    }

    #[test]
    fn one_hundred_percent_pinned_conversion() {
        assert_eq!(grammar("one hundred percent uptime"), "100 percent uptime");
    }

    // ── Numbers: guardrails ──

    #[test]
    fn stray_number_words_in_prose_stay_words() {
        assert_organize_unchanged("I have one question");
        assert_organize_unchanged("there is two issues with the patch");
        assert_organize_unchanged("twenty");
        assert_organize_unchanged("twenty-five");
    }

    #[test]
    fn plain_compound_quantities_stay_words() {
        // Locked by the DPR behavior corpus: `the period of the moon is
        // twenty seven days` is ordinary prose, not a conversion site.
        assert_organize_unchanged("the period of the moon is twenty seven days");
        assert_organize_unchanged("seven eleven");
        assert_organize_unchanged("three thirty");
    }

    #[test]
    fn ordinals_stay_words() {
        assert_organize_unchanged("the first time I tried this");
        assert_organize_unchanged("she finished fifth in the race");
        assert_organize_unchanged("the ninth iteration");
    }

    #[test]
    fn ambiguous_or_oversized_numbers_stay_words() {
        assert_organize_unchanged("one hundred and five");
        assert_organize_unchanged("one million users");
        assert_organize_unchanged("one hundred thousand dollars");
        assert_organize_unchanged("three twenty");
        assert_organize_unchanged("three twenty dollars");
        assert_organize_unchanged("four oh four");
    }

    #[test]
    fn fifty_percent_stays_words_percent_is_not_a_unit() {
        assert_organize_unchanged("fifty percent of users");
    }

    #[test]
    fn exclamation_point_is_not_a_decimal_marker() {
        assert_eq!(grammar("exclamation point five"), "exclamation point five");
    }

    // ── Times: positive conversions ──

    #[test]
    fn times_convert() {
        assert_eq!(grammar("three thirty pm"), "3:30 pm");
        assert_eq!(grammar("three pm"), "3 pm");
        assert_eq!(grammar("three fifteen am"), "3:15 am");
        assert_eq!(grammar("twelve pm"), "12 pm");
        assert_eq!(grammar("twelve am"), "12 am");
        assert_eq!(grammar("three oh five pm"), "3:05 pm");
        assert_eq!(grammar("three forty-five pm"), "3:45 pm");
        assert_eq!(grammar("three forty five pm"), "3:45 pm");
        assert_eq!(grammar("three twenty five pm"), "3:25 pm");
    }

    #[test]
    fn oclock_keeps_spoken_word() {
        // Pinned: `7 o'clock`, not `7:00` — no invented minute digits.
        assert_eq!(grammar("seven o'clock"), "7 o'clock");
        assert_eq!(grammar("seven oclock"), "7 oclock");
        assert_eq!(grammar("seven o'clock pm"), "7 o'clock pm");
    }

    // ── Times: guardrails ──

    #[test]
    fn out_of_range_times_stay_spoken() {
        assert_organize_unchanged("thirteen thirty pm");
        assert_organize_unchanged("three seventy pm");
        assert_organize_unchanged("zero thirty pm");
    }

    #[test]
    fn noon_and_midnight_stay_words() {
        assert_organize_unchanged("meet at noon");
        assert_organize_unchanged("meet at midnight");
        assert_organize_unchanged("twelve noon");
    }

    #[test]
    fn am_as_verb_is_not_meridiem() {
        assert_organize_unchanged("I am tired");
    }

    // ── Currency: positive conversions ──

    #[test]
    fn dollars_get_symbol() {
        assert_eq!(grammar("fifty dollars"), "$50");
        assert_eq!(grammar("one dollar"), "$1");
        assert_eq!(grammar("twenty five dollars"), "$25");
        assert_eq!(grammar("forty-five dollars"), "$45");
        assert_eq!(grammar("one hundred dollars"), "$100");
        assert_eq!(grammar("three hundred dollars"), "$300");
    }

    #[test]
    fn euros_and_cents_keep_the_unit_word() {
        assert_eq!(grammar("twenty euros"), "20 euros");
        assert_eq!(grammar("ninety-nine cents"), "99 cents");
        assert_eq!(grammar("one euro"), "1 euro");
        assert_eq!(grammar("one cent"), "1 cent");
    }

    // ── Currency: guardrails ──

    #[test]
    fn ambiguous_currency_phrases_stay_spoken() {
        assert_organize_unchanged("fifty dollars and fifty cents");
        assert_organize_unchanged("fifty dollars fifty cents");
        assert_organize_unchanged("fifty dollars and");
        assert_organize_unchanged("some dollars");
        assert_organize_unchanged("dollars");
    }

    #[test]
    fn currency_allows_trailing_and_change() {
        assert_eq!(grammar("fifty dollars and change"), "$50 and change");
    }

    // ── Email-speak: positive conversions ──

    #[test]
    fn email_speak_converts() {
        assert_eq!(grammar("foo at bar dot com"), "foo@bar.com");
        assert_eq!(
            grammar("john dot doe at example dot com"),
            "john.doe@example.com"
        );
        assert_eq!(
            grammar("foo underscore bar at example dot com"),
            "foo_bar@example.com"
        );
        assert_eq!(
            grammar("mail at voisu hyphen core dot test"),
            "mail@voisu-core.test"
        );
        assert_eq!(
            grammar("email me at foo at bar dot com"),
            "email me at foo@bar.com"
        );
        assert_eq!(grammar("foo at bar dot com."), "foo@bar.com.");
        assert_eq!(
            grammar("email foo at bar dot com, today"),
            "email foo@bar.com, today"
        );
        assert_eq!(
            grammar("foo2 at mail hyphen two dot co"),
            "foo2@mail-two.co"
        );
        assert_eq!(grammar("a at mail dot co dot uk"), "a@mail.co.uk");
    }

    #[test]
    fn bare_local_part_needs_whole_utterance_or_a_cue() {
        // Pinned DPR pipeline behavior: `goal look at example dot com` is a
        // website reference — the spoken `at` stays a word.
        assert_eq!(
            grammar("goal look at example dot com"),
            "goal look at example dot com"
        );
        // A bare local part with trailing words is ambiguous → stay spoken.
        assert_eq!(
            grammar("foo at bar dot com today"),
            "foo at bar dot com today"
        );
    }

    // ── Email-speak: guardrails ──

    #[test]
    fn email_speak_in_prose_stays() {
        // Mandated B3 negative: mid-sentence email-looking prose never
        // becomes an address. Dot-bearing prose also flows through the
        // pre-existing spoken-mark `dot` cue, so the pin is made at the
        // grammar level here and at the no-`@` organize level below.
        assert_eq!(
            grammar("we met at the dot of dawn"),
            "we met at the dot of dawn"
        );
        assert_eq!(
            grammar("I worked at a dot com startup"),
            "I worked at a dot com startup"
        );
        assert_organize_unchanged("one at a time");
        assert_organize_unchanged("foo at bar");
    }

    #[test]
    fn unknown_tld_stays_spoken() {
        assert_eq!(grammar("foo at bar dot zork"), "foo at bar dot zork");
        assert_eq!(grammar("foo at bar"), "foo at bar");
    }

    #[test]
    fn email_shape_requires_labels() {
        assert_eq!(grammar("foo at dot com"), "foo at dot com");
        assert_eq!(grammar("a at b dot com"), "a at b dot com");
    }

    #[test]
    fn organized_email_prose_never_invents_an_address() {
        let b = organize_local_baseline("we met at the dot of dawn", &adaptive());
        assert!(
            !b.rendered().contains('@'),
            "email-speak prose must not invent an address, got {:?}",
            b.rendered()
        );
        let lower = ascii_lower(b.rendered());
        assert!(lower.contains("met") && lower.contains("dawn"));
    }

    // ── Span safety ──

    #[test]
    fn quote_interiors_keep_spoken_words() {
        let b = organize_local_baseline("quote twenty twenty-four unquote", &adaptive());
        assert_eq!(
            b.rendered(),
            "\"twenty twenty-four\"",
            "quoted numbers must stay words"
        );
        let b = organize_local_baseline("quote foo at bar dot com unquote", &adaptive());
        assert_eq!(b.rendered(), "\"foo at bar dot com\"");
        let b = organize_local_baseline(
            "quote meeting at three thirty pm unquote today",
            &adaptive(),
        );
        assert_eq!(b.rendered(), "\"meeting at three thirty pm\" today.");
        assert!(
            !b.rendered().contains("3:30"),
            "quoted time must stay spoken, got {:?}",
            b.rendered()
        );
    }

    #[test]
    fn metalinguistic_spans_keep_spoken_words() {
        let b = organize_local_baseline("say the words one two three out loud", &adaptive());
        assert_eq!(b.rendered(), "Say the words one two three out loud.");
        assert_eq!(
            grammar("say the words fifty dollars out loud"),
            "say the words fifty dollars out loud"
        );
    }

    #[test]
    fn glued_punctuation_fails_closed_mid_span() {
        // The comma is glued mid-phrase; converting would delete it.
        assert_eq!(grammar("three, pm"), "three, pm");
        assert_eq!(grammar("one, hundred five"), "one, hundred five");
    }

    // ── Idempotency + round-trip ──

    #[test]
    fn grammar_pass_is_idempotent() {
        let cases = [
            "twenty twenty-four",
            "one hundred five dollars",
            "three thirty pm",
            "seven o'clock",
            "fifty dollars",
            "ninety-nine cents",
            "twenty euros",
            "foo at bar dot com",
            "point five",
            "meet at three fifteen pm please",
        ];
        for src in cases {
            let once = grammar(src);
            assert_eq!(
                grammar(&once),
                once,
                "second grammar pass changed {src:?} → {once:?}"
            );
        }
    }

    #[test]
    fn organize_output_is_stable_when_reorganized() {
        let cases = [
            "meeting moved to three thirty pm",
            "I paid fifty dollars",
            "email foo at bar dot com now",
            "back in twenty twenty-four",
        ];
        for src in cases {
            let once = organize_local_baseline(src, &adaptive());
            let twice = organize_local_baseline(once.rendered(), &adaptive());
            assert_eq!(
                twice.rendered(),
                once.rendered(),
                "re-organizing changed output for {src:?}"
            );
        }
    }

    #[test]
    fn converted_output_has_no_leftover_cue_words() {
        let cases: &[(&str, &[&str])] = &[
            ("twenty twenty-four", &["twenty", "four"]),
            ("one hundred five", &["one", "hundred", "five"]),
            ("point five", &["point"]),
            ("three point one four", &["three", "point", "one", "four"]),
            ("three thirty pm", &["three", "thirty"]),
            ("fifty dollars", &["fifty", "dollars"]),
            ("ninety-nine cents", &["ninety", "nine"]),
            ("twenty euros", &["twenty"]),
            ("foo at bar dot com", &["at", "dot", "com"]),
        ];
        for (src, cues) in cases {
            let out = grammar(src);
            let lower = ascii_lower(&out);
            for cue in *cues {
                assert!(
                    !word_tokens(&lower).into_iter().any(|(_, _, t)| t == *cue),
                    "cue word {cue:?} survived conversion of {src:?} → {out:?}"
                );
            }
        }
    }

    // ── Pipeline integration ──

    #[test]
    fn organized_prose_converts_values_and_still_polishes() {
        let b = organize_local_baseline("meeting moved to three thirty pm", &adaptive());
        assert_eq!(b.rendered(), "Meeting moved to 3:30 pm.");
        let b = organize_local_baseline("I paid fifty dollars for it", &adaptive());
        assert_eq!(b.rendered(), "I paid $50 for it.");
        let b = organize_local_baseline("email foo at bar dot com now", &adaptive());
        assert_eq!(b.rendered(), "Email foo@bar.com now.");
        let b = organize_local_baseline("back in twenty twenty-four", &adaptive());
        assert_eq!(b.rendered(), "Back in 2024.");
    }

    #[test]
    fn email_addresses_keep_case_at_sentence_start() {
        // A technical local part converts mid-sentence; the address at the
        // utterance start must not be sentence-capitalised.
        let b = organize_local_baseline("foo2 at bar dot com is down", &adaptive());
        assert_eq!(b.rendered(), "foo2@bar.com is down.");
    }

    #[test]
    fn literal_identity_converts_value_cues_like_marks() {
        let b = organize_local_baseline("three thirty pm", &literal());
        assert_eq!(b.rendered(), "3:30 pm");
        let b = organize_local_baseline("twenty twenty-four", &literal());
        assert_eq!(b.rendered(), "2024");
        let b = organize_local_baseline("foo at bar dot com", &literal());
        assert_eq!(b.rendered(), "foo@bar.com");
        // No cue, no conversion.
        let b = organize_local_baseline("just some words", &literal());
        assert_eq!(b.rendered(), "just some words");
    }

    #[test]
    fn trailing_punctuation_is_never_deleted() {
        assert_eq!(
            grammar("I need twenty twenty-four, today"),
            "I need 2024, today"
        );
        assert_eq!(grammar("it costs fifty dollars."), "it costs $50.");
        assert_eq!(grammar("meet at three pm!"), "meet at 3 pm!");
    }

    #[test]
    fn spoken_ordinal_lists_still_win_after_grammar() {
        // The first/second/third list machinery must be unaffected.
        let b = organize_local_baseline(
            "first do the deployment second figure out the env variable third report to me",
            &adaptive(),
        );
        assert_eq!(
            b.rendered(),
            "1. Do the deployment\n2. Figure out the env variable\n3. Report to me"
        );
    }

    #[test]
    fn spoken_step_markers_survive_grammar() {
        // Locked DPR corpus: `steps one reproduce two isolate three fix four
        // verify` — single stray number words must stay for the steps pass.
        let b = organize_local_baseline(
            "steps one reproduce two isolate three fix four verify",
            &adaptive(),
        );
        assert!(b.rendered().contains("1. Reproduce"), "{:?}", b.rendered());
        assert!(b.rendered().contains("4. Verify"), "{:?}", b.rendered());
    }

    #[test]
    fn existing_grocery_and_first_time_guardrails_still_hold() {
        let b = organize_local_baseline("Cup, milk, eggs, bread", &adaptive());
        assert_eq!(b.rendered(), "Cup, milk, eggs, bread.");
        let b = organize_local_baseline("The first time I tried this", &adaptive());
        assert_eq!(b.rendered(), "The first time I tried this.");
    }

    #[test]
    fn no_invented_alphabetic_words_property() {
        let samples = [
            "twenty twenty-four season",
            "one hundred five reasons",
            "three point one four approx",
            "meeting at three thirty pm tomorrow",
            "it costs fifty dollars and change",
            "email foo at bar dot com today",
            "quote one hundred unquote",
            "say the words ninety-nine out loud",
        ];
        for src in samples {
            let src_words: Vec<String> = word_tokens(src)
                .into_iter()
                .map(|(_, _, t)| ascii_lower(t))
                .collect();
            let out = organize_local_baseline(src, &adaptive())
                .rendered()
                .to_owned();
            for (_, _, t) in word_tokens(&out) {
                // Every alphabetic fragment of every output token (an
                // address like `foo@bar.com` yields foo/bar/com) must come
                // from the source — nothing is invented.
                let lower = ascii_lower(t);
                let fragments = lower
                    .split(|c: char| !c.is_ascii_alphanumeric())
                    .filter(|f| f.chars().any(|c| c.is_ascii_alphabetic()));
                for fragment in fragments {
                    assert!(
                        src_words.iter().any(|s| s.contains(fragment)),
                        "invented fragment {fragment:?} from source {src:?} → {out:?}"
                    );
                }
            }
        }
    }
}
