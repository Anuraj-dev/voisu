use serde::Serialize;

/// Strict word-level alignment against a reference.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WordError {
    pub deletions: usize,
    pub error_rate: f64,
    pub insertions: usize,
    pub reference_tokens: usize,
    pub substitutions: usize,
}

/// One critical meaning mismatch.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CriticalError {
    pub category: String,
    pub reference_token: String,
}

/// Prefix or body deleted relative to reference or the source that fed the organizer.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SectionLoss {
    pub body: bool,
    pub prefix: bool,
    pub relative_to: Vec<String>,
}

impl SectionLoss {
    pub fn any(&self) -> bool {
        self.prefix || self.body
    }
}

pub fn tokenize(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(normalize_token)
        .filter(|tok| !tok.is_empty())
        .collect()
}

fn normalize_token(raw: &str) -> String {
    let mut tok = raw
        .trim_matches(|c: char| {
            matches!(
                c,
                '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | '!' | '?'
            )
        })
        .to_owned();
    if tok.ends_with('.')
        && !tok.contains("://")
        && tok.matches('.').count() == 1
        && !tok.chars().any(|c| c.is_ascii_digit())
    {
        tok.pop();
    }
    tok.to_ascii_lowercase()
}

pub fn align_words(reference: &str, hypothesis: &str) -> WordError {
    let reference_tokens = tokenize(reference);
    let hypothesis_tokens = tokenize(hypothesis);
    let (insertions, deletions, substitutions) =
        levenshtein_ops(&reference_tokens, &hypothesis_tokens);
    let n = reference_tokens.len();
    let errors = insertions + deletions + substitutions;
    let error_rate = if n == 0 {
        if hypothesis_tokens.is_empty() {
            0.0
        } else {
            1.0
        }
    } else {
        errors as f64 / n as f64
    };
    WordError {
        deletions,
        error_rate,
        insertions,
        reference_tokens: n,
        substitutions,
    }
}

fn levenshtein_ops(reference: &[String], hypothesis: &[String]) -> (usize, usize, usize) {
    let n = reference.len();
    let m = hypothesis.len();
    // The per-cell DP values are only ever read from the previous row, so `dp`
    // rolls on two rows; the traceback, however, needs the full backpointer
    // history, so `back` keeps one byte per cell (flat, one allocation). A full
    // (n+1)x(m+1) u32 matrix would put peak memory at 5 bytes per cell — about
    // 500 MB for a 10k-word alignment versus about 100 MB here.
    let mut dp = vec![vec![0u32; m + 1]; 2];
    let mut back = vec![0u8; (n + 1) * (m + 1)];
    let idx = |i: usize, j: usize| i * (m + 1) + j;
    // 1 = del, 2 = ins, 3 = sub/match
    for j in 1..=m {
        dp[0][j] = j as u32;
        back[idx(0, j)] = 2;
    }
    for i in 1..=n {
        let cur = i % 2;
        let prev = 1 - cur;
        dp[cur][0] = i as u32;
        back[idx(i, 0)] = 1;
        for j in 1..=m {
            let cost = u32::from(reference[i - 1] != hypothesis[j - 1]);
            let del = dp[prev][j] + 1;
            let ins = dp[cur][j - 1] + 1;
            let sub = dp[prev][j - 1] + cost;
            if del <= ins && del <= sub {
                dp[cur][j] = del;
                back[idx(i, j)] = 1;
            } else if ins <= sub {
                dp[cur][j] = ins;
                back[idx(i, j)] = 2;
            } else {
                dp[cur][j] = sub;
                back[idx(i, j)] = 3;
            }
        }
    }
    let mut i = n;
    let mut j = m;
    let mut insertions = 0usize;
    let mut deletions = 0usize;
    let mut substitutions = 0usize;
    while i > 0 || j > 0 {
        match back[idx(i, j)] {
            1 => {
                deletions += 1;
                i -= 1;
            }
            2 => {
                insertions += 1;
                j -= 1;
            }
            3 => {
                if reference[i - 1] != hypothesis[j - 1] {
                    substitutions += 1;
                }
                i -= 1;
                j -= 1;
            }
            _ => break,
        }
    }
    (insertions, deletions, substitutions)
}

pub fn detect_section_loss(reference: &str, source_fed: &str, hypothesis: &str) -> SectionLoss {
    let mut relative_to = Vec::new();
    let (prefix_ref, body_ref) = span_loss(reference, hypothesis);
    let (prefix_src, body_src) = span_loss(source_fed, hypothesis);
    if prefix_ref || body_ref {
        relative_to.push("reference".to_owned());
    }
    if prefix_src || body_src {
        relative_to.push("source".to_owned());
    }
    SectionLoss {
        body: body_ref || body_src,
        prefix: prefix_ref || prefix_src,
        relative_to,
    }
}

fn span_loss(original: &str, hypothesis: &str) -> (bool, bool) {
    let orig = tokenize(original);
    let hyp = tokenize(hypothesis);
    if orig.len() < 3 {
        return (false, false);
    }
    if hyp.is_empty() {
        return (true, true);
    }
    let prefix_len = dropped_prefix_len(&orig, &hyp);
    let prefix = prefix_len >= 3;
    let body = missing_body_run(&orig, &hyp, prefix_len);
    (prefix, body)
}

fn dropped_prefix_len(original: &[String], hypothesis: &[String]) -> usize {
    if original.is_empty() || hypothesis.is_empty() {
        return 0;
    }
    // original.starts_with(hypothesis) with leftover original tokens is a
    // dropped suffix/body, not a dropped prefix.
    let first = &hypothesis[0];
    let Some(start) = original.iter().position(|tok| tok == first) else {
        return 0;
    };
    if start < 3 {
        return 0;
    }
    let rest = &original[start..];
    let matched = rest
        .iter()
        .zip(hypothesis.iter())
        .take_while(|(a, b)| a == b)
        .count();
    // Both conditions return `start`; merged (clippy::if_same_then_else).
    if matched >= hypothesis.len().min(3) || is_suffix_alignment(original, hypothesis, start) {
        start
    } else {
        0
    }
}

fn is_suffix_alignment(original: &[String], hypothesis: &[String], start: usize) -> bool {
    let rest = &original[start..];
    if rest.len() < 3 {
        return false;
    }
    let overlap = rest.iter().filter(|tok| hypothesis.contains(tok)).count();
    overlap * 2 >= rest.len()
}

fn missing_body_run(original: &[String], hypothesis: &[String], prefix_len: usize) -> bool {
    const MIN_RUN: usize = 4;
    if original.len() < prefix_len + MIN_RUN {
        return false;
    }
    let body = &original[prefix_len..];
    let mut run = 0usize;
    for tok in body {
        if hypothesis.contains(tok) {
            run = 0;
        } else {
            run += 1;
            if run >= MIN_RUN {
                return true;
            }
        }
    }
    false
}

pub fn detect_critical_errors(reference: &str, hypothesis: &str) -> Vec<CriticalError> {
    let ref_tokens = tokenize(reference);
    let hyp_tokens = tokenize(hypothesis);
    let hyp_set: std::collections::BTreeSet<&str> = hyp_tokens.iter().map(String::as_str).collect();
    let mut out = Vec::new();

    push_missing_category(&mut out, "negation", negation_tokens(reference), &hyp_set);
    let extra_negation: Vec<String> = negation_tokens(hypothesis)
        .into_iter()
        .filter(|tok| !negation_tokens(reference).contains(tok))
        .collect();
    for token in extra_negation {
        out.push(CriticalError {
            category: "negation".to_owned(),
            reference_token: format!("+{token}"),
        });
    }

    push_missing_category(&mut out, "number", number_tokens(reference), &hyp_set);
    push_missing_category(&mut out, "unit", unit_tokens(&ref_tokens), &hyp_set);
    push_missing_category(&mut out, "name", name_tokens(reference), &hyp_set);
    push_missing_category(&mut out, "command", command_tokens(reference), &hyp_set);
    push_missing_category(&mut out, "path", path_tokens(&ref_tokens), &hyp_set);
    push_missing_category(&mut out, "url", url_tokens(&ref_tokens), &hyp_set);
    push_missing_category(&mut out, "code", code_tokens(reference), &hyp_set);

    for clause in missing_clauses(reference, hypothesis) {
        out.push(CriticalError {
            category: "missing_clause".to_owned(),
            reference_token: clause,
        });
    }

    out.sort_by(|a, b| {
        a.category
            .cmp(&b.category)
            .then_with(|| a.reference_token.cmp(&b.reference_token))
    });
    out.dedup();
    out
}

fn push_missing_category(
    out: &mut Vec<CriticalError>,
    category: &str,
    tokens: Vec<String>,
    hyp_set: &std::collections::BTreeSet<&str>,
) {
    for token in tokens {
        if !hyp_set.contains(token.as_str()) {
            out.push(CriticalError {
                category: category.to_owned(),
                reference_token: token,
            });
        }
    }
}

fn negation_tokens(text: &str) -> Vec<String> {
    tokenize(text)
        .into_iter()
        .filter(|tok| {
            matches!(
                tok.as_str(),
                "not"
                    | "never"
                    | "no"
                    | "don't"
                    | "dont"
                    | "doesn't"
                    | "doesnt"
                    | "didn't"
                    | "didnt"
                    | "cannot"
                    | "can't"
                    | "cant"
                    | "won't"
                    | "wont"
                    | "without"
                    | "n't"
            ) || tok.ends_with("n't")
        })
        .collect()
}

fn number_tokens(text: &str) -> Vec<String> {
    tokenize(text)
        .into_iter()
        .filter(|tok| {
            tok.chars().any(|c| c.is_ascii_digit())
                || matches!(
                    tok.as_str(),
                    "zero"
                        | "one"
                        | "two"
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
                        | "hundred"
                        | "thousand"
                )
        })
        .collect()
}

fn unit_tokens(tokens: &[String]) -> Vec<String> {
    tokens
        .iter()
        .filter(|tok| {
            matches!(
                tok.as_str(),
                "ms" | "s"
                    | "sec"
                    | "secs"
                    | "second"
                    | "seconds"
                    | "minute"
                    | "minutes"
                    | "hour"
                    | "hours"
                    | "kb"
                    | "mb"
                    | "gb"
                    | "percent"
                    | "%"
                    | "px"
                    | "hz"
            ) || tok.ends_with("ms") && tok.chars().next().is_some_and(|c| c.is_ascii_digit())
        })
        .cloned()
        .collect()
}

fn name_tokens(text: &str) -> Vec<String> {
    let skip = [
        "the", "a", "an", "and", "or", "but", "if", "then", "so", "i", "we", "you",
    ];
    text.split_whitespace()
        .filter_map(|raw| {
            let cleaned = raw.trim_matches(|c: char| !c.is_alphanumeric());
            if cleaned.is_empty() {
                return None;
            }
            let first = cleaned.chars().next()?;
            if !first.is_uppercase() {
                return None;
            }
            let lower = cleaned.to_ascii_lowercase();
            if skip.contains(&lower.as_str()) {
                return None;
            }
            Some(lower)
        })
        .collect()
}

fn command_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    if voisu_core::is_command_shaped(text)
        && let Some(first) = tokenize(text).first()
    {
        out.push(first.clone());
    }
    for tok in tokenize(text) {
        let flag = tok.starts_with("--")
            || (tok.starts_with('-')
                && tok.len() > 1
                && tok.chars().nth(1) != Some('-')
                && tok.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
        if flag && tok != "-" {
            out.push(tok);
        }
    }
    out
}

fn path_tokens(tokens: &[String]) -> Vec<String> {
    tokens
        .iter()
        .filter(|tok| {
            (tok.contains('/') || tok.starts_with("./") || tok.starts_with("~/"))
                && !tok.contains("://")
        })
        .cloned()
        .collect()
}

fn url_tokens(tokens: &[String]) -> Vec<String> {
    tokens
        .iter()
        .filter(|tok| {
            tok.contains("://")
                || tok.starts_with("www.")
                || tok.contains(".com")
                || tok.contains(".org")
        })
        .cloned()
        .collect()
}

fn code_tokens(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter_map(|raw| {
            let cleaned = raw.trim_matches(|c: char| {
                !c.is_ascii_alphanumeric() && c != '_' && c != ':' && c != '-'
            });
            if cleaned.is_empty() {
                return None;
            }
            if cleaned.contains('_')
                || cleaned.contains("::")
                || has_internal_lower_to_upper(cleaned)
                || cleaned.starts_with("--")
            {
                Some(cleaned.to_ascii_lowercase())
            } else {
                None
            }
        })
        .collect()
}

fn has_internal_lower_to_upper(token: &str) -> bool {
    let mut prev_lower = false;
    for c in token.chars() {
        if prev_lower && c.is_ascii_uppercase() {
            return true;
        }
        prev_lower = c.is_ascii_lowercase();
    }
    false
}

fn missing_clauses(reference: &str, hypothesis: &str) -> Vec<String> {
    let hyp = tokenize(hypothesis);
    let hyp_set: std::collections::BTreeSet<&str> = hyp.iter().map(String::as_str).collect();
    let mut missing = Vec::new();
    for clause in reference.split(['.', '!', '?', '\n']) {
        let words = tokenize(clause);
        if words.len() < 4 {
            continue;
        }
        let overlap = words
            .iter()
            .filter(|w| hyp_set.contains(w.as_str()))
            .count();
        if overlap * 2 < words.len() {
            missing.push(words.join(" "));
        }
    }
    missing
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn section_loss_flags_dropped_prefix() {
        let source = "please remember this context ship the rust parser files src/main.rs";
        let organized = "Context ship the rust parser. Files src/main.rs.";
        let loss = detect_section_loss(source, source, organized);
        assert!(loss.prefix, "expected prefix section loss, got {loss:?}");
        assert!(loss.relative_to.iter().any(|r| r == "source"));
    }

    #[test]
    fn hypothesis_that_is_a_prefix_of_source_is_body_loss_not_prefix() {
        let source = "please remember this context ship the rust parser files src/main.rs";
        let hypothesis = "please remember this context";
        let loss = detect_section_loss(source, source, hypothesis);
        assert!(
            !loss.prefix,
            "dropped suffix/body must not count as prefix: {loss:?}"
        );
        assert!(loss.body, "expected body/suffix section loss, got {loss:?}");
    }

    #[test]
    fn camel_case_identifiers_count_as_code_token_errors() {
        let errors = detect_critical_errors(
            "call formatValidated then runTool",
            "call format validated then run tool",
        );
        assert!(
            errors
                .iter()
                .any(|err| err.category == "code" && err.reference_token == "formatvalidated"),
            "formatValidated should be a code-token error, got {errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|err| err.category == "code" && err.reference_token == "runtool"),
            "runTool should be a code-token error, got {errors:?}"
        );
    }

    #[test]
    fn sentence_initial_please_is_not_a_code_token() {
        let errors = detect_critical_errors("Please call formatValidated", "call format validated");
        assert!(
            !errors
                .iter()
                .any(|err| err.category == "code" && err.reference_token == "please"),
            "sentence-initial Please must not be a code error, got {errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|err| err.category == "code" && err.reference_token == "formatvalidated"),
            "formatValidated should still be a code-token error, got {errors:?}"
        );
    }

    #[test]
    fn identical_texts_have_zero_word_error() {
        let wer = align_words("open the board", "open the board");
        assert_eq!(wer.insertions, 0);
        assert_eq!(wer.deletions, 0);
        assert_eq!(wer.substitutions, 0);
        assert_eq!(wer.error_rate, 0.0);
    }

    #[test]
    fn word_error_breaks_down_substitutions_deletions_and_insertions() {
        // ref: a b c d — hyp: a x c  -> one substitution (b->x), one deletion (d).
        let wer = align_words("a b c d", "a x c");
        assert_eq!(wer.substitutions, 1);
        assert_eq!(wer.deletions, 1);
        assert_eq!(wer.insertions, 0);
        assert_eq!(wer.reference_tokens, 4);
        // ref: a b — hyp: a b c d -> two insertions.
        let wer = align_words("a b", "a b c d");
        assert_eq!(wer.insertions, 2);
        assert_eq!(wer.substitutions, 0);
        assert_eq!(wer.deletions, 0);
    }
}
