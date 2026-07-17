//! Shared vocabulary dictionary.
//!
//! A built-in developer glossary merged with an optional user dictionary,
//! exposed two ways:
//!
//! * [`merged_terms`] — the merged term list (user terms first, then the
//!   built-in categories, de-duplicated). This is the single source of truth
//!   seam: ticket 05 consumes it read-only to feed Deepgram keyterm boosting.
//! * [`whisper_prompt`] — a natural comma-separated glossary truncated to the
//!   ~224-token Whisper prompt budget, fed to the Groq/Whisper `prompt` field.
//!
//! The user dictionary is plain text at `$XDG_CONFIG_HOME/voisu/dictionary.txt`
//! (default `~/.config/voisu/dictionary.txt`): one term per line, `#` starts a
//! comment, blank lines are ignored, and a missing file means built-ins only.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Whisper honours only ~224 tokens of prompt. The prompt builder appends terms
/// until adding the next one would cross this budget, then stops.
pub const WHISPER_PROMPT_TOKEN_BUDGET: usize = 224;

/// AI tooling vocabulary.
const AI_TOOLING: &[&str] = &[
    "Claude",
    "Claude Code",
    "Codex",
    "OpenAI",
    "Anthropic",
    "GPT",
    "LLM",
    "Groq",
    "Deepgram",
    "Whisper",
    "token",
    "inference",
    "embedding",
    "prompt",
    "transcription",
];

/// Linux and system vocabulary.
const LINUX_SYSTEM: &[&str] = &[
    "systemd",
    "systemctl",
    "journalctl",
    "SELinux",
    "Wayland",
    "KDE",
    "Plasma",
    "PipeWire",
    "xkbcommon",
    "RPM",
    "dnf",
    "grep",
    "chmod",
    "kernel",
    "Fedora",
    "audit2allow",
    "rpmbuild",
    "daemon",
];

/// Programming vocabulary.
const PROGRAMMING: &[&str] = &[
    "async",
    "await",
    "serde",
    "Tokio",
    "enum",
    "mutex",
    "Rust",
    "cargo",
    "TypeScript",
    "npm",
    "closure",
    "trait",
    "struct",
    "borrow",
    "compiler",
    "JSON",
];

/// Infrastructure and full-stack vocabulary.
const INFRA_FULLSTACK: &[&str] = &[
    "Kubernetes",
    "Docker",
    "Postgres",
    "Redis",
    "pub-sub",
    "WebSocket",
    "API",
    "HTTP",
    "TLS",
    "CI/CD",
    "deployment",
    "frontend",
    "backend",
    "full-stack",
    "latency",
    "p99",
    "gateway",
    "cache",
];

/// The built-in categories in priority order. Categories earlier in this list
/// survive prompt truncation ahead of later ones.
const BUILTIN_CATEGORIES: &[&[&str]] =
    &[AI_TOOLING, LINUX_SYSTEM, PROGRAMMING, INFRA_FULLSTACK];

/// The flattened built-in developer dictionary, in category priority order.
fn builtin_terms() -> Vec<String> {
    BUILTIN_CATEGORIES
        .iter()
        .flat_map(|category| category.iter())
        .map(|term| (*term).to_owned())
        .collect()
}

/// The merged vocabulary: user dictionary terms first (highest priority), then
/// the built-in developer dictionary, de-duplicated case-insensitively while
/// preserving first-seen order.
///
/// This is the shared read-only seam consumed by both the Whisper prompt
/// builder and, in ticket 05, Deepgram keyterm boosting.
pub fn merged_terms() -> Vec<String> {
    merged_terms_with(load_user_terms(&user_dictionary_path()))
}

/// The Whisper vocabulary prompt: a natural comma-separated glossary of the
/// merged terms, truncated to the [`WHISPER_PROMPT_TOKEN_BUDGET`]. It carries no
/// instructions — Whisper biases toward prompt vocabulary, it is not an
/// instruction channel.
pub fn whisper_prompt() -> String {
    whisper_prompt_from_terms(&merged_terms())
}

/// The resolved user dictionary path: `$XDG_CONFIG_HOME/voisu/dictionary.txt`,
/// falling back to `~/.config/voisu/dictionary.txt`.
fn user_dictionary_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    base.join("voisu").join("dictionary.txt")
}

/// Parses a user dictionary file into terms. One term per line; `#` starts a
/// comment (whole-line or trailing); blank lines are ignored. A missing or
/// unreadable file yields no terms (built-ins only).
fn load_user_terms(path: &Path) -> Vec<String> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    parse_user_terms(&contents)
}

/// Parses user dictionary text into terms, applying the comment and blank-line
/// rules. Separated from the filesystem so it is testable directly.
fn parse_user_terms(contents: &str) -> Vec<String> {
    contents
        .lines()
        .filter_map(|line| {
            let term = match line.split_once('#') {
                Some((before, _comment)) => before,
                None => line,
            }
            .trim();
            if term.is_empty() {
                None
            } else {
                Some(term.to_owned())
            }
        })
        .collect()
}

/// Merges `user` terms ahead of the built-in dictionary, de-duplicating
/// case-insensitively and preserving the first-seen casing and order.
fn merged_terms_with(user: Vec<String>) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut merged: Vec<String> = Vec::new();
    for term in user.into_iter().chain(builtin_terms()) {
        if seen.insert(term.to_lowercase()) {
            merged.push(term);
        }
    }
    merged
}

/// Builds a comma-separated glossary from `terms`, appending terms in order
/// until adding the next one would exceed [`WHISPER_PROMPT_TOKEN_BUDGET`].
fn whisper_prompt_from_terms(terms: &[String]) -> String {
    let mut prompt = String::new();
    for term in terms {
        let candidate_len = if prompt.is_empty() {
            term.chars().count()
        } else {
            prompt.chars().count() + 2 + term.chars().count()
        };
        if estimate_tokens(candidate_len) > WHISPER_PROMPT_TOKEN_BUDGET {
            break;
        }
        if !prompt.is_empty() {
            prompt.push_str(", ");
        }
        prompt.push_str(term);
    }
    prompt
}

/// Approximates the Whisper token count of a glossary `char_len` characters
/// long using the standard ~4-characters-per-token heuristic.
fn estimate_tokens(char_len: usize) -> usize {
    char_len.div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merged_terms_place_user_terms_before_builtins() {
        let merged = merged_terms_with(vec!["Voisu".to_owned(), "wayfinder".to_owned()]);
        assert_eq!(merged[0], "Voisu");
        assert_eq!(merged[1], "wayfinder");
        // A built-in term still appears, after the user terms.
        let claude = merged.iter().position(|t| t == "Claude").expect("built-ins present");
        assert!(claude > 1, "built-ins follow the user terms");
    }

    #[test]
    fn merged_terms_dedupe_case_insensitively_keeping_user_casing() {
        // "groq" is a user term that collides with the built-in "Groq".
        let merged = merged_terms_with(vec!["groq".to_owned()]);
        let hits: Vec<&String> = merged.iter().filter(|t| t.eq_ignore_ascii_case("groq")).collect();
        assert_eq!(hits.len(), 1, "the duplicate is collapsed");
        assert_eq!(hits[0], "groq", "the user's casing wins");
    }

    #[test]
    fn missing_user_file_yields_builtins_only() {
        let terms = load_user_terms(Path::new("/nonexistent/voisu/dictionary.txt"));
        assert!(terms.is_empty());
        let merged = merged_terms_with(terms);
        assert_eq!(merged, builtin_terms());
    }

    #[test]
    fn user_dictionary_parsing_honours_comments_and_blank_lines() {
        let contents = "\
# a header comment
serde

Tokio   # inline comment
   # indented comment
   spaced term  \n";
        let terms = parse_user_terms(contents);
        assert_eq!(terms, vec!["serde", "Tokio", "spaced term"]);
    }

    #[test]
    fn whisper_prompt_is_a_comma_separated_glossary_without_instructions() {
        let prompt = whisper_prompt();
        assert!(prompt.contains("Claude"));
        assert!(prompt.contains(", "), "terms are comma-separated");
        // No instruction-channel phrasing leaks into the prompt.
        let lowered = prompt.to_lowercase();
        assert!(!lowered.contains("transcribe"));
        assert!(!lowered.contains("please"));
        assert!(!lowered.contains("the following"));
    }

    #[test]
    fn whisper_prompt_truncates_at_the_token_budget() {
        // A user dictionary far larger than the budget must be truncated, and a
        // known late term must be dropped while an early one survives.
        let user: Vec<String> = (0..2000).map(|i| format!("term{i:04}")).collect();
        let prompt = whisper_prompt_from_terms(&merged_terms_with(user));
        assert!(estimate_tokens(prompt.chars().count()) <= WHISPER_PROMPT_TOKEN_BUDGET);
        assert!(prompt.contains("term0000"), "early user terms survive");
        assert!(!prompt.contains("term1999"), "late terms are truncated away");
        // Truncation dropped the built-ins entirely (they sort after the user terms).
        assert!(!prompt.contains("Kubernetes"));
    }

    #[test]
    fn whisper_prompt_from_terms_never_starts_with_a_separator() {
        let prompt = whisper_prompt_from_terms(&["Rust".to_owned(), "cargo".to_owned()]);
        assert_eq!(prompt, "Rust, cargo");
    }
}
