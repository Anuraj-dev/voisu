//! Boost words: the user's own vocabulary, biasing both providers and the
//! reconcile step toward their spellings. The budgets and term rules mirror
//! the Linux dictionary (`crates/voisu-app/src/dictionary.rs`), so a list
//! behaves the same on both platforms. Windows has no built-in terms: only
//! what the user typed is sent.

use std::collections::HashSet;

/// Groq (Whisper) `prompt` budget, in tokens.
pub const WHISPER_PROMPT_TOKEN_BUDGET: usize = 224;
/// Deepgram nova-3 keyterm budget across all keyterms, in tokens. Crossing it
/// makes Deepgram refuse the whole stream.
pub const DEEPGRAM_KEYTERM_TOKEN_BUDGET: usize = 500;
/// Deepgram's keyterm count limit.
pub const DEEPGRAM_KEYTERM_COUNT_LIMIT: usize = 100;

/// Trims each term, drops empty and multi-line ones, and removes
/// case-insensitive duplicates, keeping the first-seen casing and order.
pub fn normalize<S: AsRef<str>>(terms: impl IntoIterator<Item = S>) -> Vec<String> {
    let mut seen = HashSet::new();
    terms
        .into_iter()
        .filter_map(|term| {
            let term = term.as_ref().trim();
            let valid = !term.is_empty() && !term.contains(['\n', '\r']);
            (valid && seen.insert(term.to_lowercase())).then(|| term.to_owned())
        })
        .collect()
}

/// The leading terms that fit both Deepgram limits. Stops at the first term
/// that does not fit, so earlier terms always win.
pub fn deepgram_keyterms(terms: &[String]) -> Vec<String> {
    let mut keyterms = Vec::new();
    let mut tokens = 0;
    for term in terms {
        let candidate = tokens + token_upper_bound(term.len());
        if keyterms.len() == DEEPGRAM_KEYTERM_COUNT_LIMIT
            || candidate > DEEPGRAM_KEYTERM_TOKEN_BUDGET
        {
            break;
        }
        keyterms.push(term.clone());
        tokens = candidate;
    }
    keyterms
}

/// A comma-separated glossary of the leading terms that fit the Whisper
/// budget. A glossary, not instructions: Whisper biases toward prompt words.
pub fn whisper_prompt(terms: &[String]) -> String {
    let mut prompt = String::new();
    for term in terms {
        let candidate = if prompt.is_empty() {
            term.len()
        } else {
            prompt.len() + ", ".len() + term.len()
        };
        if token_upper_bound(candidate) > WHISPER_PROMPT_TOKEN_BUDGET {
            break;
        }
        if !prompt.is_empty() {
            prompt.push_str(", ");
        }
        prompt.push_str(term);
    }
    prompt
}

/// Byte-level BPE starts at one token per byte and only merges, so the byte
/// length bounds the token count from above, multibyte text included.
fn token_upper_bound(byte_len: usize) -> usize {
    byte_len
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(list: &[&str]) -> Vec<String> {
        list.iter().map(|term| (*term).to_owned()).collect()
    }

    #[test]
    fn normalize_trims_drops_blank_and_multiline_and_dedupes_keeping_first_casing() {
        let normalized = normalize([
            "  Tauri ",
            "",
            "   ",
            "tauri",
            "Deepgram",
            "two\nlines",
            "cr\rterm",
            "TAURI",
            "C#",
        ]);
        assert_eq!(normalized, terms(&["Tauri", "Deepgram", "C#"]));
    }

    #[test]
    fn normalize_keeps_unicode_terms_and_compares_them_case_insensitively() {
        assert_eq!(
            normalize(["Ärger", "ärger", "नमस्ते"]),
            terms(&["Ärger", "नमस्ते"])
        );
    }

    #[test]
    fn keyterms_stop_at_the_count_limit() {
        let many: Vec<String> = (0..150).map(|i| format!("t{i}")).collect();
        let keyterms = deepgram_keyterms(&many);
        assert_eq!(keyterms.len(), DEEPGRAM_KEYTERM_COUNT_LIMIT);
        assert_eq!(keyterms, many[..DEEPGRAM_KEYTERM_COUNT_LIMIT]);
    }

    #[test]
    fn keyterms_accept_exactly_the_token_budget_and_stop_at_the_first_overflow() {
        let exact = vec!["a".repeat(250), "b".repeat(250), "c".to_owned()];
        assert_eq!(deepgram_keyterms(&exact), exact[..2]);
        // A term that does not fit ends the list; later short terms do not
        // jump the queue.
        let blocked = vec!["a".repeat(400), "b".repeat(200), "c".to_owned()];
        assert_eq!(deepgram_keyterms(&blocked), blocked[..1]);
    }

    #[test]
    fn keyterms_count_multibyte_terms_by_bytes() {
        let wide = vec!["😀".repeat(100), "😀".repeat(30)]; // 400 + 120 bytes
        assert_eq!(deepgram_keyterms(&wide), wide[..1]);
    }

    #[test]
    fn whisper_prompt_is_a_glossary_within_its_budget() {
        assert_eq!(whisper_prompt(&terms(&["Voisu", "Tauri"])), "Voisu, Tauri");
        assert_eq!(whisper_prompt(&[]), "");
        let many: Vec<String> = (0..100).map(|i| format!("term{i}")).collect();
        let prompt = whisper_prompt(&many);
        assert!(prompt.len() <= WHISPER_PROMPT_TOKEN_BUDGET);
        assert!(prompt.starts_with("term0, term1"));
        assert!(!prompt.ends_with(", "));
        assert!(
            prompt.len() > WHISPER_PROMPT_TOKEN_BUDGET - 10,
            "fills the budget"
        );
    }

    #[test]
    fn an_oversized_first_term_leaves_the_prompt_empty() {
        assert_eq!(whisper_prompt(&["x".repeat(300), "y".to_owned()]), "");
    }
}
