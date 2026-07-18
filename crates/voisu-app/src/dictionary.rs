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
//! * [`deepgram_keyterms`] — the merged terms truncated to Deepgram's streaming
//!   keyterm token and count budgets, preserving user-term priority.
//!
//! The user dictionary is plain text at `$XDG_CONFIG_HOME/voisu/dictionary.txt`
//! (default `~/.config/voisu/dictionary.txt`): one term per line, `#` starts a
//! comment, blank lines are ignored, and a missing file means built-ins only.

use std::collections::HashSet;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

/// Whisper honours only ~224 tokens of prompt. The prompt builder appends terms
/// until adding the next one would cross this budget, then stops.
pub const WHISPER_PROMPT_TOKEN_BUDGET: usize = 224;

/// Deepgram rejects streaming connections whose keyterms exceed this provable
/// token bound. The daemon must stay within it because a 400 response kills
/// the entire streaming connection.
pub const DEEPGRAM_KEYTERM_TOKEN_BUDGET: usize = 500;

/// Keep Deepgram keyterm boosting well below its recommended term count.
pub const DEEPGRAM_KEYTERM_COUNT_LIMIT: usize = 100;

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

/// Adds a user term to the end of the personal dictionary. Existing terms are
/// compared case-insensitively, so the user's first spelling and ordering win.
pub fn add_user_term(term: &str) -> Result<bool, String> {
    let term = validated_term(term)?;
    let path = user_dictionary_path();
    let existing = read_dictionary_contents(&path)?;
    if parse_user_terms(&existing)
        .iter()
        .any(|existing_term| existing_term.to_lowercase() == term.to_lowercase())
    {
        return Ok(false);
    }

    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(term);
    updated.push('\n');
    write_user_dictionary(&path, &updated)?;
    Ok(true)
}

/// Removes every case-insensitive occurrence of a user term while leaving
/// comments, blank lines, and the order of all other lines byte-for-byte intact.
pub fn remove_user_term(term: &str) -> Result<bool, String> {
    let term = validated_term(term)?;
    let path = user_dictionary_path();
    let existing = read_dictionary_contents(&path)?;
    let mut removed = false;
    let mut updated = String::with_capacity(existing.len());
    for line in existing.split_inclusive('\n') {
        let content = line.strip_suffix('\n').unwrap_or(line).trim_end_matches('\r');
        let matches = strip_comment(content)
            .trim()
            .to_lowercase()
            == term.to_lowercase();
        if matches {
            removed = true;
        } else {
            updated.push_str(line);
        }
    }
    if !removed {
        return Ok(false);
    }
    write_user_dictionary(&path, &updated)?;
    Ok(true)
}

/// Reads only the personal dictionary terms, in their stored order. Built-in
/// vocabulary is deliberately excluded so the CLI manages exactly what users
/// have written.
pub fn user_terms() -> Result<Vec<String>, String> {
    read_user_terms(&user_dictionary_path())
}

/// The Deepgram keyterm vocabulary: `terms` truncated in order to the
/// [`DEEPGRAM_KEYTERM_TOKEN_BUDGET`] and [`DEEPGRAM_KEYTERM_COUNT_LIMIT`].
/// Deepgram returns a 400 response when its keyterm token cap is exceeded,
/// which kills the whole streaming connection. [`token_upper_bound`] assumes
/// token count never exceeds UTF-8 byte count when tokens consume at least one
/// input byte and normalization does not expand it. Deepgram does not document
/// its tokenizer, so this is a conservative engineering assumption for typical
/// short ASCII technical terms, not a proof.
pub fn deepgram_keyterms(terms: &[String]) -> Vec<String> {
    let mut keyterms = Vec::new();
    let mut token_count = 0;

    for term in terms {
        let candidate_token_count = token_count + token_upper_bound(term.len());
        if keyterms.len() == DEEPGRAM_KEYTERM_COUNT_LIMIT
            || candidate_token_count > DEEPGRAM_KEYTERM_TOKEN_BUDGET
        {
            break;
        }
        keyterms.push(term.clone());
        token_count = candidate_token_count;
    }

    keyterms
}

/// The Whisper vocabulary prompt: a natural comma-separated glossary of the
/// merged terms, truncated to the [`WHISPER_PROMPT_TOKEN_BUDGET`]. It carries no
/// instructions — Whisper biases toward prompt vocabulary, it is not an
/// instruction channel.
pub fn whisper_prompt() -> String {
    whisper_prompt_for_terms(&merged_terms())
}

/// Builds a Whisper vocabulary prompt from an already-resolved dictionary
/// snapshot. The daemon uses this with the same terms used for Deepgram so one
/// Recording cannot mix vocabulary versions across Providers.
pub fn whisper_prompt_for_terms(terms: &[String]) -> String {
    whisper_prompt_from_terms(terms)
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

fn validated_term(term: &str) -> Result<&str, String> {
    let term = term.trim();
    if term.is_empty() || term.contains(['\n', '\r']) {
        return Err("dictionary term must be one non-empty line".to_owned());
    }
    Ok(term)
}

fn read_dictionary_contents(path: &Path) -> Result<String, String> {
    match std::fs::read_to_string(path) {
        Ok(contents) => Ok(contents),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(format!(
            "cannot read user dictionary {}: {error}",
            path.display()
        )),
    }
}

/// Replaces the dictionary via a fully written same-directory temporary file,
/// so readers see either the old file or the complete new file, never a torn
/// edit.
fn write_user_dictionary(path: &Path, contents: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("dictionary path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create dictionary directory {}: {error}", parent.display()))?;
    let mut file = tempfile::Builder::new()
        .prefix(".dictionary.txt.")
        .tempfile_in(parent)
        .map_err(|error| format!("cannot stage dictionary write in {}: {error}", parent.display()))?;
    file.write_all(contents.as_bytes())
        .and_then(|()| file.as_file().sync_all())
        .map_err(|error| format!("cannot write user dictionary {}: {error}", path.display()))?;
    file.persist(path)
        .map_err(|error| format!("cannot persist user dictionary {}: {}", path.display(), error.error))?;
    Ok(())
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

/// A provable upper bound on the real BPE token count of text that is
/// `byte_len` UTF-8 bytes long. Byte-level BPE starts from one token per input
/// byte and only ever merges adjacent tokens, so the emitted token count never
/// exceeds the byte count. Char/4-style heuristics under-count multibyte terms
/// (emoji, punctuation, CJK); the byte length never does, so truncating against
/// it can only over-count, never over-run a provider's token budget.
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

    #[test]
    fn deepgram_keyterms_truncate_oversized_lists_within_both_budgets() {
        let terms: Vec<String> = (0..600).map(|index| format!("term{index:03}")).collect();

        let keyterms = deepgram_keyterms(&terms);

        assert!(keyterms.len() <= DEEPGRAM_KEYTERM_COUNT_LIMIT);
        assert!(
            keyterms
                .iter()
                .map(|term| token_upper_bound(term.len()))
                .sum::<usize>()
                <= DEEPGRAM_KEYTERM_TOKEN_BUDGET
        );
        assert_eq!(keyterms, terms[..keyterms.len()]);
        assert!(keyterms.len() < terms.len(), "late terms are truncated");
    }

    #[test]
    fn deepgram_keyterms_keep_user_terms_ahead_of_builtins_when_truncated() {
        let user: Vec<String> = (0..DEEPGRAM_KEYTERM_COUNT_LIMIT)
            .map(|index| format!("user{index:03}"))
            .collect();
        let merged = merged_terms_with(user.clone());

        let keyterms = deepgram_keyterms(&merged);

        assert_eq!(keyterms, user[..keyterms.len()]);
        assert!(keyterms.len() < user.len(), "late user terms are truncated");
        assert!(!keyterms.iter().any(|term| term == "Claude"));
    }

    #[test]
    fn deepgram_keyterms_leave_lists_within_both_budgets_unchanged() {
        let terms = vec!["Voisu".to_owned(), "Deepgram".to_owned(), "systemctl".to_owned()];

        assert_eq!(deepgram_keyterms(&terms), terms);
    }

    #[test]
    fn deepgram_keyterms_exclude_the_first_term_that_crosses_the_token_budget() {
        let terms = vec!["a".repeat(499), "bc".to_owned(), "later".to_owned()];

        assert_eq!(deepgram_keyterms(&terms), vec!["a".repeat(499)]);
    }

    #[test]
    fn deepgram_keyterms_accept_exactly_the_token_budget_and_drop_the_next_byte() {
        let exactly_at_budget = vec!["a".repeat(499), "b".to_owned()];
        let one_byte_over_budget = vec!["a".repeat(500), "b".to_owned()];

        assert_eq!(deepgram_keyterms(&exactly_at_budget), exactly_at_budget);
        assert_eq!(
            deepgram_keyterms(&one_byte_over_budget),
            vec!["a".repeat(500)]
        );
    }

    #[test]
    fn deepgram_keyterms_accept_exactly_the_count_limit_and_drop_the_next_term() {
        let terms: Vec<String> = (0..=DEEPGRAM_KEYTERM_COUNT_LIMIT)
            .map(|index| format!("t{index:03}"))
            .collect();

        assert_eq!(
            deepgram_keyterms(&terms[..DEEPGRAM_KEYTERM_COUNT_LIMIT]),
            terms[..DEEPGRAM_KEYTERM_COUNT_LIMIT]
        );
        assert_eq!(
            deepgram_keyterms(&terms),
            terms[..DEEPGRAM_KEYTERM_COUNT_LIMIT]
        );
    }

    #[test]
    fn deepgram_keyterms_stop_when_the_first_term_exceeds_the_token_budget() {
        let terms = vec!["a".repeat(DEEPGRAM_KEYTERM_TOKEN_BUDGET + 1), "later".to_owned()];

        assert_eq!(deepgram_keyterms(&terms), Vec::<String>::new());
    }
}
