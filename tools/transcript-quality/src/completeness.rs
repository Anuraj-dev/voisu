use voisu_core::sanitize_source_transcript_text;

use crate::metrics::tokenize;

/// Which Source Transcript the completeness heuristic selected.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SourceProvider {
    Deepgram,
    Groq,
}

impl SourceProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deepgram => "deepgram",
            Self::Groq => "groq",
        }
    }
}

/// Completeness-aware Source Transcript choice (evaluator-only).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompletenessChoice {
    Selected {
        provider: SourceProvider,
        text: String,
    },
    Missing {
        reason: String,
    },
}

/// Prefer the materially fuller safe Source Transcript.
///
/// Discounts repeated filler, duplicated loops, and known outro garbage.
/// A short coherent fragment must not beat a longer non-repetitive sibling.
/// This is not product source selection (ticket 02).
pub fn select_completeness_aware(
    groq: Option<&str>,
    deepgram: Option<&str>,
) -> CompletenessChoice {
    let groq = usable(groq);
    let deepgram = usable(deepgram);
    match (groq, deepgram) {
        (None, None) => CompletenessChoice::Missing {
            reason: "no usable Source Transcript (Groq and Deepgram missing or empty)".to_owned(),
        },
        (Some(text), None) => CompletenessChoice::Selected {
            provider: SourceProvider::Groq,
            text,
        },
        (None, Some(text)) => CompletenessChoice::Selected {
            provider: SourceProvider::Deepgram,
            text,
        },
        (Some(groq_text), Some(deepgram_text)) => {
            let groq_score = score(&groq_text);
            let deepgram_score = score(&deepgram_text);
            if groq_score.beats(&deepgram_score) {
                CompletenessChoice::Selected {
                    provider: SourceProvider::Groq,
                    text: groq_text,
                }
            } else if deepgram_score.beats(&groq_score) {
                CompletenessChoice::Selected {
                    provider: SourceProvider::Deepgram,
                    text: deepgram_text,
                }
            } else if groq_score.unique_content_words >= deepgram_score.unique_content_words {
                CompletenessChoice::Selected {
                    provider: SourceProvider::Groq,
                    text: groq_text,
                }
            } else {
                CompletenessChoice::Selected {
                    provider: SourceProvider::Deepgram,
                    text: deepgram_text,
                }
            }
        }
    }
}

fn usable(text: Option<&str>) -> Option<String> {
    let raw = text.map(str::trim).filter(|s| !s.is_empty())?;
    let sanitized = sanitize_source_transcript_text(raw);
    if sanitized.trim().is_empty() {
        None
    } else {
        Some(raw.to_owned())
    }
}

struct ContentScore {
    unique_content_words: usize,
    tokens: Vec<String>,
}

impl ContentScore {
    fn beats(&self, other: &ContentScore) -> bool {
        if is_contiguous_fragment(&other.tokens, &self.tokens)
            && self.unique_content_words > other.unique_content_words
        {
            return true;
        }
        let extra = self
            .unique_content_words
            .saturating_sub(other.unique_content_words);
        if extra >= 4 {
            return true;
        }
        if other.unique_content_words == 0 {
            return self.unique_content_words > 0;
        }
        let ratio =
            self.unique_content_words as f64 / other.unique_content_words.max(1) as f64;
        ratio >= 1.25 && self.unique_content_words > other.unique_content_words
    }
}

fn score(text: &str) -> ContentScore {
    let sanitized = sanitize_source_transcript_text(text);
    let tokens: Vec<String> = tokenize(&sanitized)
        .into_iter()
        .filter(|tok| !is_filler(tok))
        .collect();
    let collapsed = collapse_loops(&tokens);
    let mut unique = collapsed.clone();
    unique.sort();
    unique.dedup();
    ContentScore {
        unique_content_words: unique.len(),
        tokens: collapsed,
    }
}

fn is_filler(tok: &str) -> bool {
    matches!(
        tok,
        "um" | "uh" | "er" | "ah" | "hmm" | "huh" | "mhm" | "uh-huh" | "uhhuh"
    )
}

fn collapse_loops(tokens: &[String]) -> Vec<String> {
    if tokens.len() < 4 {
        return tokens.to_vec();
    }
    let mut out = Vec::with_capacity(tokens.len());
    let mut i = 0usize;
    while i < tokens.len() {
        let mut collapsed = false;
        let remaining = tokens.len() - i;
        let max_window = 8.min(remaining / 2);
        for window in (1..=max_window).rev() {
            let pattern = &tokens[i..i + window];
            let mut repeats = 1usize;
            let mut cursor = i + window;
            while cursor + window <= tokens.len() && &tokens[cursor..cursor + window] == pattern {
                repeats += 1;
                cursor += window;
            }
            if repeats >= 2 {
                out.extend_from_slice(pattern);
                i = cursor;
                collapsed = true;
                break;
            }
        }
        if !collapsed {
            out.push(tokens[i].clone());
            i += 1;
        }
    }
    out
}

fn is_contiguous_fragment(short: &[String], long: &[String]) -> bool {
    if short.is_empty() || short.len() >= long.len() {
        return false;
    }
    long.windows(short.len()).any(|window| window == short)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_fragment_does_not_beat_fuller_sibling() {
        let long = "open the pravah board and move the eval ticket to review";
        let short = "open the pravah board";
        let choice = select_completeness_aware(Some(long), Some(short));
        match choice {
            CompletenessChoice::Selected { provider, text } => {
                assert_eq!(provider, SourceProvider::Groq);
                assert_eq!(text, long);
            }
            CompletenessChoice::Missing { reason } => {
                panic!("expected a selected source, got missing: {reason}")
            }
        }
    }

    #[test]
    fn loops_do_not_count_as_fuller() {
        let looped = "ship the parser ship the parser ship the parser ship the parser";
        let fuller = "ship the parser before Friday";
        let choice = select_completeness_aware(Some(looped), Some(fuller));
        match choice {
            CompletenessChoice::Selected { provider, .. } => {
                assert_eq!(provider, SourceProvider::Deepgram);
            }
            CompletenessChoice::Missing { reason } => panic!("unexpected missing: {reason}"),
        }
    }
}
