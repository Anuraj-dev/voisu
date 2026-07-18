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
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// Whisper honours only ~224 tokens of prompt. The prompt builder appends terms
/// until adding the next one would cross this budget, then stops.
pub const WHISPER_PROMPT_TOKEN_BUDGET: usize = 224;

/// AI tooling vocabulary. Ordinary English words (token, prompt, inference…)
/// are deliberately absent: Whisper and nova-3 already transcribe them, and
/// every byte spent on a common word pushes a distinctive CLI term past the
/// Whisper prompt budget.
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
];

/// Linux and system vocabulary, most-distinctive terms first: the live WER
/// suite showed misses exactly on the CLI compounds ("rpmbuild" -> "RPM
/// build", "changelog" -> "channel log", "dist tag" -> "disk tag",
/// "daemon-reload" -> "--daemon"), so those must survive prompt truncation
/// ahead of words the models already know.
const LINUX_SYSTEM: &[&str] = &[
    "systemctl",
    "daemon-reload",
    "journalctl",
    "rpmbuild",
    "audit2allow",
    "xkbcommon",
    "SELinux",
    "changelog",
    "dist tag",
    "voisu",
    "voisu-daemon",
    "dnf",
    "grep",
    "systemd",
    "Wayland",
    "KDE",
    "Plasma",
    "PipeWire",
    "RPM",
    "Fedora",
    "chmod",
    "kernel",
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

/// Loads user dictionary terms, logging a local diagnostic on a genuine read
/// failure. A missing file is not an error (built-ins only); a permission
/// denial or invalid UTF-8 is surfaced to stderr rather than silently dropping
/// all user vocabulary.
fn load_user_terms(path: &Path) -> Vec<String> {
    match read_user_terms(path) {
        Ok(terms) => terms,
        Err(diagnostic) => {
            eprintln!("{diagnostic}");
            Vec::new()
        }
    }
}

/// Reads and parses a user dictionary file. A missing file yields `Ok(empty)`;
/// any other read failure (permission, invalid UTF-8) yields `Err(diagnostic)`
/// so the caller can surface it instead of masquerading it as a missing file.
/// Separated from logging so it is testable directly.
fn read_user_terms(path: &Path) -> Result<Vec<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(contents) => Ok(parse_user_terms(&contents)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(format!(
            "voisu: ignoring unreadable user dictionary at {}: {error}",
            path.display()
        )),
    }
}

/// Parses user dictionary text into terms, applying the comment and blank-line
/// rules. Separated from the filesystem so it is testable directly.
fn parse_user_terms(contents: &str) -> Vec<String> {
    contents
        .lines()
        .filter_map(|line| {
            let term = strip_comment(line).trim();
            if term.is_empty() {
                None
            } else {
                Some(term.to_owned())
            }
        })
        .collect()
}

/// Returns `line` with any trailing comment removed. A `#` begins a comment only
/// at line start or when preceded by whitespace, so terms like `C#` and `F#`
/// (where `#` follows a non-space character) are preserved intact.
fn strip_comment(line: &str) -> &str {
    let mut preceded_by_whitespace = true;
    for (index, character) in line.char_indices() {
        if character == '#' && preceded_by_whitespace {
            return &line[..index];
        }
        preceded_by_whitespace = character.is_whitespace();
    }
    line
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
/// until adding the next one would cross [`WHISPER_PROMPT_TOKEN_BUDGET`] real
/// tokens. Truncation uses the conservative [`token_upper_bound`] so the result
/// provably never exceeds the budget.
fn whisper_prompt_from_terms(terms: &[String]) -> String {
    let mut prompt = String::new();
    for term in terms {
        let candidate_len = if prompt.is_empty() {
            term.len()
        } else {
            prompt.len() + ", ".len() + term.len()
        };
        if token_upper_bound(candidate_len) > WHISPER_PROMPT_TOKEN_BUDGET {
            break;
        }
        if !prompt.is_empty() {
            prompt.push_str(", ");
        }
        prompt.push_str(term);
    }
    prompt
}

/// A provable upper bound on the real Whisper token count of a glossary that is
/// `byte_len` UTF-8 bytes long. Whisper's byte-level BPE (like GPT-2) starts
/// from one token per input byte and only ever merges adjacent tokens, so the
/// emitted token count never exceeds the byte count. Char/4-style heuristics
/// under-count multibyte terms (emoji, punctuation, CJK); the byte length never
/// does, so truncating against it can only over-count, never over-run the
/// ~224-token prompt window Whisper honours.
fn token_upper_bound(byte_len: usize) -> usize {
    byte_len
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
        assert!(token_upper_bound(prompt.len()) <= WHISPER_PROMPT_TOKEN_BUDGET);
        assert!(prompt.contains("term0000"), "early user terms survive");
        assert!(!prompt.contains("term1999"), "late terms are truncated away");
        // Truncation dropped the built-ins entirely (they sort after the user terms).
        assert!(!prompt.contains("Kubernetes"));
    }

    #[test]
    fn cli_compound_terms_survive_prompt_truncation() {
        // The 2026-07-18 live WER suite missed exactly these CLI compounds;
        // they must fit inside the truncated Whisper prompt with builtins
        // only, not merely exist somewhere in the merged list.
        let prompt = whisper_prompt_from_terms(&merged_terms_with(Vec::new()));
        for term in ["daemon-reload", "rpmbuild", "changelog", "dist tag", "voisu-daemon"] {
            assert!(prompt.contains(term), "{term:?} truncated out of: {prompt}");
        }
    }

    #[test]
    fn whisper_prompt_from_terms_never_starts_with_a_separator() {
        let prompt = whisper_prompt_from_terms(&["Rust".to_owned(), "cargo".to_owned()]);
        assert_eq!(prompt, "Rust, cargo");
    }

    #[test]
    fn whisper_prompt_stays_within_the_real_token_budget_for_multibyte_terms() {
        // Byte-level BPE (Whisper/GPT-2) never emits more tokens than the input
        // has UTF-8 bytes, so the byte length is a provable upper bound on the
        // real token count. Emoji and other multibyte terms tokenize far above a
        // char/4 estimate; the built prompt must still be provably under budget.
        let user: Vec<String> = (0..2000).map(|i| format!("😀ζterm{i}")).collect();
        let prompt = whisper_prompt_from_terms(&user);
        // The provable upper bound on real tokens is the UTF-8 byte length.
        assert!(
            prompt.len() <= WHISPER_PROMPT_TOKEN_BUDGET,
            "prompt is {} bytes, over the {}-token provable bound",
            prompt.len(),
            WHISPER_PROMPT_TOKEN_BUDGET
        );
        assert!(!prompt.is_empty(), "some multibyte terms still fit");
    }

    #[test]
    fn reading_a_present_user_dictionary_returns_its_terms() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dictionary.txt");
        std::fs::write(&path, "serde\nTokio\n").unwrap();
        assert_eq!(read_user_terms(&path), Ok(vec!["serde".to_owned(), "Tokio".to_owned()]));
    }

    #[test]
    fn a_missing_user_dictionary_is_not_an_error() {
        let terms = read_user_terms(Path::new("/nonexistent/voisu/dictionary.txt"));
        assert_eq!(terms, Ok(Vec::new()));
    }

    #[test]
    fn an_unreadable_user_dictionary_surfaces_a_diagnostic_not_silent_emptiness() {
        // Invalid UTF-8 must not masquerade as a missing file: it silently drops
        // all user vocabulary. It has to surface as a diagnostic instead.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dictionary.txt");
        std::fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
        let result = read_user_terms(&path);
        assert!(result.is_err(), "an unreadable dictionary is a diagnostic, not empty terms");
        assert!(
            result.unwrap_err().contains("dictionary"),
            "the diagnostic names the dictionary"
        );
    }

    #[test]
    fn a_hash_only_starts_a_comment_at_a_boundary_so_c_sharp_survives() {
        // "C#" is a real term: the '#' is not preceded by whitespace. A '#' only
        // begins a comment at line start or after whitespace.
        let terms = parse_user_terms("C#\nF#\nTokio # trailing comment\n# whole-line\n");
        assert_eq!(terms, vec!["C#", "F#", "Tokio"]);
    }
}
