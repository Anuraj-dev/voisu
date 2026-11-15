//! Phonetic Dictionary correction for Local Transcripts.
//!
//! Exact-match correction ([`super::apply_user_vocabulary`]) only fixes a
//! hearing that already spells a Dictionary entry word for word. A Local
//! model that never saw the name writes what it sounded like instead ("voice
//! you" for `Voisu`, "deep gram" for `Deepgram`); Parakeet ignores prompts, so
//! this pass is its only Dictionary lever. It exists to lower word error rate,
//! and a false swap (correct words turned into a name) costs more than a
//! missed one, so every rule leans toward leaving words alone. A word, or a
//! short run of words, is replaced by an entry when ALL of these hold:
//!
//! * **eligible entry** — the entry has two or more syllables and only ASCII
//!   letters (plus separators). One-syllable names collide with too many
//!   ordinary words;
//! * **same sound code** — the run's Double Metaphone primary code (letters
//!   joined, no length cap) equals the entry's;
//! * **close sound shape** — the Levenshtein distance between the run's
//!   [`sound_shape`] and the entry's is at most `max(1, shape_len / 4)`.
//!   Double Metaphone drops every non-initial vowel, so its code alone matches
//!   "face" to `Voisu`; the shape keeps each vowel letter where it falls. The
//!   hearing must also have at least as many vowel groups as the entry, so a
//!   syllable is never invented ("isk" is not `Isaac`, "luks" not `Lucas`);
//!   a run whose letters spell the entry exactly counts as distance zero;
//! * **exact shape for ordinary words** — when every heard word is in the
//!   embedded [`ENGLISH_WORDS`] lexicon the distance must be zero, either
//!   word by word ("voice you" and `Voisu` are both `voisu`) or with the
//!   words' letters joined ("deep gram" spells `Deepgram`). Vowels are not
//!   folded together, so "meet you" (`metu`) is not `Matteo` (`mateo`) and
//!   "had son" is not `Hudson`. A two-word English hearing of a name that
//!   needs any vowel change ("cooper nets", "jonah than") therefore stays;
//! * **not everyday speech** — a run made only of words ranked below
//!   [`EVERYDAY_RANK`] is never swapped, even when it sounds or spells exactly
//!   like an entry ("way land", "east one", "out then", "all over");
//! * **plain run** — the run has 1 to `entry words + 2` words (at most
//!   [`MAX_RUN_WORDS`]) joined only by same-line whitespace or a bare hyphen
//!   ("voice-you"), so it never crosses punctuation, a spaced dash or a line
//!   break. Each word has two or more ASCII letters and nothing else, is not
//!   one of the [`FUNCTION_WORDS`] ("dock her", "are in", "log in" stay), is
//!   not written in capitals past its first letter (acronyms such as "LUKS",
//!   names such as "GitHub"), and the run never starts with "you" ("you bunt
//!   too" stays). It sits outside spoken quotes and say-the-words spans, and
//!   is not part of a URL, path, email or identifier;
//! * **not an English word** — a single heard word in the lexicon is never
//!   swapped ("sorry" stays `sorry` with `Sarah` in the Dictionary);
//! * **unambiguous** — exactly one entry matches at that position, and no
//!   word of the run is part of a span exact-match correction rewrites.
//!
//! The pass writes the entry as the user wrote it; an all-lowercase entry
//! takes a capital only at a sentence start the speaker capitalized. It
//! replaces only the run's words, so the
//! punctuation around it survives. It is idempotent: a corrected run spells
//! its entry and is skipped next time.
//!
//! Adapted from VoxType `src/text/vocabulary.rs` (MIT License,
//! Copyright (c) 2025 Peter Jackson, <https://github.com/peteonrails/voxtype>,
//! commit `fbedd62`). Changes: two-syllable minimum instead of a letter
//! minimum, primary-code-only matching without the 4-character cap, a
//! sound-shape distance in place of a letter distance (so split hearings such
//! as "voice you" qualify), punctuation-free runs, an English-lexicon guard,
//! and Voisu's quote and say-the-words masking.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use rphonetic::{DoubleMetaphone, Encoder};

use super::{USER_VOCABULARY_TERM_LIMIT, VocabularyTerm, alphanumeric_runs, masked_ranges};

/// Longest run of heard words one entry may replace.
const MAX_RUN_WORDS: usize = 4;

/// The English lexicon: the 30,000 most frequent lowercase ASCII words of
/// English Wikipedia (contractions without their apostrophe), from
/// `IlyaSemenov/wikipedia-word-frequency` `results/enwiki-2023-04-13.txt` at
/// commit `d11c4fab211156e96ab5943850dd6f98604707fb` (MIT License,
/// Copyright (c) 2015 Ilya Semenov; see `THIRD_PARTY_NOTICES.md`). 30,000
/// rather than 10,000 because Wikipedia under-ranks conversational words:
/// "sorry" ranks 13,050 and "jams" 20,694. Lines starting with `#` are the
/// file's header; the rest are in rank order.
const ENGLISH_WORDS: &str = include_str!("english-words.txt");

/// Lexicon ranks below this are everyday words. A run made only of them is
/// ordinary speech even when it spells or sounds exactly like an entry: "way
/// land" (ranks 346 and 318) is not `Wayland`, "east one" is not `Easton`.
/// "voice" ranks 1,304, so "voice you" stays a `Voisu` hearing.
const EVERYDAY_RANK: usize = 1_000;

/// Each lexicon word with its frequency rank, most frequent first (0).
fn english_lexicon() -> &'static HashMap<&'static str, usize> {
    static LEXICON: OnceLock<HashMap<&'static str, usize>> = OnceLock::new();
    LEXICON.get_or_init(|| {
        ENGLISH_WORDS
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .enumerate()
            .map(|(rank, word)| (word, rank))
            .collect()
    })
}

/// One phonetic Dictionary swap: the heard byte span of the input text and
/// the entry written in its place.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PhoneticSwap {
    pub start: usize,
    pub end: usize,
    pub heard: String,
    pub replacement: String,
}

/// Applies phonetic Dictionary correction to `text` (see the module docs).
/// Run it after exact-match correction.
pub fn apply_phonetic_dictionary(text: &str, terms: &[String]) -> String {
    let swaps = phonetic_dictionary_swaps(text, terms);
    if swaps.is_empty() {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for swap in swaps {
        out.push_str(&text[cursor..swap.start]);
        out.push_str(&swap.replacement);
        cursor = swap.end;
    }
    out.push_str(&text[cursor..]);
    out
}

/// The non-overlapping swaps [`apply_phonetic_dictionary`] would make, in
/// text order. Exposed so evidence tools can list every swap.
pub fn phonetic_dictionary_swaps(text: &str, terms: &[String]) -> Vec<PhoneticSwap> {
    let encoder = DoubleMetaphone::new(None);
    // Entries that spell the same letters ("Claude Code", "claude-code") are
    // one entry, first wins; otherwise each would make the other ambiguous.
    let mut seen = HashSet::new();
    let entries: Vec<Entry> = terms
        .iter()
        .take(USER_VOCABULARY_TERM_LIMIT)
        .filter_map(|term| Entry::parse(term, &encoder))
        .filter(|entry| seen.insert(entry.letters.clone()))
        .collect();
    let mut by_code: HashMap<&str, Vec<&Entry>> = HashMap::new();
    for entry in entries.iter().filter(|entry| entry.eligible) {
        by_code.entry(&entry.primary).or_default().push(entry);
    }
    if by_code.is_empty() {
        return Vec::new();
    }
    let words = words(text);
    if words.is_empty() {
        return Vec::new();
    }
    let context = Context {
        text,
        blocked: exact_match_words(text, &words, terms),
        usable: usable_words(text, &words),
        sounds: words.iter().map(|word| word_sound(&word.letters)).collect(),
        words,
        by_code,
        lexicon: english_lexicon(),
        encoder,
    };

    let mut swaps = Vec::new();
    let mut index = 0;
    while index < context.words.len() {
        match context.best_match(index) {
            Some((count, entry)) => {
                let start = context.words[index].start;
                let end = context.words[index + count - 1].end;
                swaps.push(PhoneticSwap {
                    start,
                    end,
                    heard: text[start..end].to_owned(),
                    replacement: written_form(&entry.display, text, start),
                });
                index += count;
            }
            None => index += 1,
        }
    }
    swaps
}

/// A Dictionary entry prepared for phonetic matching.
struct Entry {
    display: String,
    letters: String,
    words: usize,
    primary: String,
    shape: String,
    eligible: bool,
}

impl Entry {
    fn parse(term: &str, encoder: &DoubleMetaphone) -> Option<Self> {
        let display = term.trim();
        let letters = letters_only(display);
        if letters.is_empty() {
            return None;
        }
        let entry_words: Vec<String> = display
            .split(|character: char| !character.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .map(letters_only)
            .collect();
        let primary = encoder.encode(&letters);
        let ascii_letters_only = display
            .chars()
            .all(|character| !character.is_alphanumeric() || character.is_ascii_alphabetic());
        let eligible = ascii_letters_only && syllables(display) >= 2 && primary.len() >= 2;
        Some(Self {
            display: display.to_owned(),
            shape: sound_shape(entry_words.iter().map(|word| word_sound(word))),
            letters,
            words: entry_words.len(),
            primary,
            eligible,
        })
    }

    fn max_run_words(&self) -> usize {
        (self.words + 2).min(MAX_RUN_WORDS)
    }
}

/// One heard word: alphanumerics with internal apostrophes ("you're"), and
/// its lowercase ASCII letters.
struct Word {
    start: usize,
    end: usize,
    letters: String,
}

fn words(text: &str) -> Vec<Word> {
    let mut words = Vec::new();
    let characters: Vec<(usize, char)> = text.char_indices().collect();
    let mut index = 0;
    while index < characters.len() {
        if !characters[index].1.is_alphanumeric() {
            index += 1;
            continue;
        }
        let start = characters[index].0;
        let mut last = index;
        let mut next = index + 1;
        while next < characters.len() {
            let character = characters[next].1;
            if character.is_alphanumeric() {
                last = next;
                next += 1;
            } else if matches!(character, '\'' | '\u{2019}')
                && characters
                    .get(next + 1)
                    .is_some_and(|(_, after)| after.is_alphanumeric())
            {
                next += 1;
            } else {
                break;
            }
        }
        let (last_start, last_char) = characters[last];
        let end = last_start + last_char.len_utf8();
        words.push(Word {
            start,
            end,
            letters: letters_only(&text[start..end]),
        });
        index = last + 1;
    }
    words
}

/// Words inside a span exact-match correction rewrites: the entry's word
/// sequence, case-insensitive, as [`super::apply_user_vocabulary`] matches
/// it. Exact matching owns those spans; the phonetic pass never re-targets
/// them. A run whose letters merely join to an entry ("deep gram" for
/// `Deepgram`) is not such a span: exact matching leaves it, so it stays
/// open here as strong evidence.
fn exact_match_words(text: &str, words: &[Word], terms: &[String]) -> Vec<bool> {
    let runs = alphanumeric_runs(text);
    let mut spans: Vec<std::ops::Range<usize>> = Vec::new();
    for term in terms
        .iter()
        .take(USER_VOCABULARY_TERM_LIMIT)
        .filter_map(|term| VocabularyTerm::parse(term))
    {
        let count = term.runs.len();
        for start in 0..(runs.len() + 1).saturating_sub(count) {
            let window = &runs[start..start + count];
            if window
                .iter()
                .zip(&term.runs)
                .all(|(run, expected)| run.lowered == *expected)
            {
                spans.push(window[0].start..window[count - 1].end);
            }
        }
    }
    words
        .iter()
        .map(|word| {
            spans
                .iter()
                .any(|span| word.start < span.end && span.start < word.end)
        })
        .collect()
}

/// Pronouns, articles, short prepositions and forms of "be" a run never
/// includes: a speech model writes them confidently, so "dock her" is a verb
/// and its object, not `Docker`, "are in" is not `Aaron`, and "log in" is
/// not `Login`. "you" is deliberately absent: it is also the sound of many
/// name endings ("voice you" for `Voisu`), so it may end a run but never
/// start one.
const FUNCTION_WORDS: [&str; 33] = [
    "a", "am", "an", "and", "are", "as", "at", "be", "he", "her", "him", "his", "i", "in", "is",
    "it", "its", "me", "my", "of", "on", "or", "our", "she", "the", "them", "they", "to", "us",
    "was", "we", "were", "with",
];

/// Words a run may include (see the module docs' plain-run rule): two or
/// more ASCII letters and nothing else, not a [`FUNCTION_WORDS`] entry, no
/// capital past the first letter, outside spoken quotes and say-the-words
/// spans, and not part of a URL, path, email or identifier.
fn usable_words(text: &str, words: &[Word]) -> Vec<bool> {
    let masked = masked_ranges(text);
    let technical = technical_words(text, words);
    words
        .iter()
        .zip(technical)
        .map(|(word, technical)| {
            let written = &text[word.start..word.end];
            !technical
                && word.letters.len() >= 2
                && !FUNCTION_WORDS.contains(&word.letters.as_str())
                // Letters only: no digits, non-ASCII letters or apostrophes
                // ("Voisu's" keeps its possessive).
                && written.chars().all(|character| character.is_ascii_alphabetic())
                // A capital past the first letter means the model already
                // wrote an acronym ("LUKS", "CVE") or a cased name.
                && !written.chars().skip(1).any(|character| character.is_ascii_uppercase())
                && !masked
                    .iter()
                    .any(|range| word.start < range.end && range.start < word.end)
        })
        .collect()
}

/// Per word: does its whitespace-delimited chunk look like a URL, path, email
/// or identifier? Trailing sentence punctuation does not count. Chunk bounds
/// come from `char_indices`, so multibyte whitespace (NBSP, U+2009, U+3000)
/// is handled.
fn technical_words(text: &str, words: &[Word]) -> Vec<bool> {
    let mut chunks: Vec<std::ops::Range<usize>> = Vec::new();
    let mut chunk_start: Option<usize> = None;
    for (index, character) in text.char_indices() {
        if character.is_whitespace() {
            if let Some(start) = chunk_start.take() {
                chunks.push(start..index);
            }
        } else if chunk_start.is_none() {
            chunk_start = Some(index);
        }
    }
    if let Some(start) = chunk_start {
        chunks.push(start..text.len());
    }
    let technical: Vec<bool> = chunks
        .iter()
        .map(|range| {
            text[range.clone()]
                .trim_end_matches(|character: char| {
                    matches!(character, '.' | ',' | '!' | '?' | ';' | ':' | ')' | '"')
                })
                .contains(['@', '/', '\\', '_', '.', ':'])
        })
        .collect();
    words
        .iter()
        .map(|word| {
            let chunk = chunks.partition_point(|range| range.end <= word.start);
            technical.get(chunk).copied().unwrap_or(false)
        })
        .collect()
}

/// Everything one pass needs, computed once per Transcript.
struct Context<'a> {
    text: &'a str,
    words: Vec<Word>,
    blocked: Vec<bool>,
    usable: Vec<bool>,
    /// Each word's [`word_sound`].
    sounds: Vec<String>,
    by_code: HashMap<&'a str, Vec<&'a Entry>>,
    lexicon: &'static HashMap<&'static str, usize>,
    encoder: DoubleMetaphone,
}

impl<'a> Context<'a> {
    /// The single entry that matches a run starting at `index`, with the
    /// run's word count. Each run's letters, code and shapes are computed
    /// once. Within one entry the closest sound shape wins, then the longer
    /// run; two different entries matching here is ambiguous and yields none.
    fn best_match(&self, index: usize) -> Option<(usize, &'a Entry)> {
        // "you" may end a run ("voice you"), never start one: "you bunt too"
        // is a question, not `Ubuntu`.
        if self.words[index].letters == "you" {
            return None;
        }
        let mut found: Vec<(&'a Entry, usize, usize)> = Vec::new();
        let mut joined = String::new();
        let mut all_english = true;
        let mut all_everyday = true;
        for count in 1..=MAX_RUN_WORDS {
            let last = index + count - 1;
            let Some(word) = self.words.get(last) else {
                break;
            };
            if self.blocked[last] || !self.usable[last] {
                break;
            }
            if count > 1 && !plain_gap(&self.text[self.words[last - 1].end..word.start]) {
                break;
            }
            joined.push_str(&word.letters);
            let rank = self.lexicon.get(word.letters.as_str()).copied();
            all_english &= rank.is_some();
            all_everyday &= rank.is_some_and(|rank| rank < EVERYDAY_RANK);
            if (count == 1 && rank.is_some()) || all_everyday {
                continue;
            }
            let primary = self.encoder.encode(&joined);
            let Some(entries) = self.by_code.get(primary.as_str()) else {
                continue;
            };
            // Two readings of the run: word by word ("voice you" -> `voisu`,
            // each word's silent e and "you" handled), and as one written
            // word ("cube control" -> `kubekontrol`, as `kubecontrol` is).
            let readings = [
                sound_shape(self.sounds[index..=last].iter()),
                sound_shape(std::iter::once(word_sound(&joined))),
            ];
            for &entry in entries {
                if count > entry.max_run_words() {
                    continue;
                }
                // A run made only of English words is ordinary speech unless
                // it sounds exactly like the entry: "voice you" is `Voisu`,
                // "post grace" is not `Postgres` and "had son" is not
                // `Hudson`. This also keeps two-English-word hearings that
                // differ by a vowel ("cooper nets" for `Kubernetes`, "jonah
                // than" for `Jonathan`) unswapped: letting them through needs
                // a vowel tolerance that turns "meet you" into `Matteo`.
                let bound = if all_english {
                    0
                } else {
                    (entry.shape.len() / 4).max(1)
                };
                // Letters that join to the entry ("deep gram") are the
                // strongest evidence there is; exact matching leaves them
                // because it matches whole words. Otherwise a hearing never
                // drops a syllable the entry spells: "isk" is not `Isaac` and
                // "luks" is not `Lucas`, while extra vowels are fine ("hyper
                // land" for `Hyprland`).
                let distance = if joined == entry.letters {
                    Some(0)
                } else {
                    readings
                        .iter()
                        .filter(|shape| vowel_groups(shape) >= vowel_groups(&entry.shape))
                        .map(|shape| crate::char_edit_distance(shape, &entry.shape))
                        .min()
                };
                let Some(distance) = distance.filter(|&distance| distance <= bound) else {
                    continue;
                };
                match found
                    .iter_mut()
                    .find(|(seen, _, _)| std::ptr::eq(*seen, entry))
                {
                    Some(best) if distance <= best.1 => *best = (entry, distance, count),
                    Some(_) => {}
                    None => found.push((entry, distance, count)),
                }
            }
        }
        match found.as_slice() {
            [(entry, _, count)] => Some((*count, *entry)),
            _ => None,
        }
    }
}

/// Words in a run may be joined by same-line whitespace, or by a bare hyphen
/// with no space around it ("voice-you"). A spaced dash or a line break is a
/// boundary.
fn plain_gap(gap: &str) -> bool {
    gap == "-"
        || (!gap.is_empty()
            && gap.chars().all(|character| {
                character.is_whitespace()
                    && !matches!(
                        character,
                        '\n' | '\r' | '\u{0B}' | '\u{0C}' | '\u{85}' | '\u{2028}' | '\u{2029}'
                    )
            }))
}

fn letters_only(value: &str) -> String {
    value
        .chars()
        .filter(char::is_ascii_alphabetic)
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

fn is_vowel(byte: u8) -> bool {
    matches!(byte, b'a' | b'e' | b'i' | b'o' | b'u')
}

/// One word's letters respelled closer to how they sound, keeping every
/// vowel letter: "you" is `u` and "eye" is `i` (each says its letter), a
/// silent final `e` after a consonant is dropped ("voice" -> `vois`), a soft
/// `c` (before e, i or y) is `s` and a hard one `k`, `ph` is `f`, `y` is the
/// vowel `i` unless a vowel follows it, and `w` after a vowel and before a
/// consonant is `u` ("new" -> `neu`).
fn word_sound(letters: &str) -> String {
    match letters {
        "you" => return "u".to_owned(),
        "eye" => return "i".to_owned(),
        _ => {}
    }
    let bytes = letters.as_bytes();
    let mut length = bytes.len();
    if length >= 3
        && bytes[length - 1] == b'e'
        && !is_vowel(bytes[length - 2])
        && bytes[length - 2] != b'y'
        && bytes[..length - 2]
            .iter()
            .any(|&byte| is_vowel(byte) || byte == b'y')
    {
        length -= 1;
    }
    let mut sound = String::with_capacity(length);
    let mut index = 0;
    while index < length {
        let byte = bytes[index];
        let next = (index + 1 < length).then(|| bytes[index + 1]);
        let after_vowel = sound.ends_with(['a', 'e', 'i', 'o', 'u']);
        let mapped = match byte {
            // The dropped silent e still softens the c before it ("voice").
            b'c' if matches!(bytes.get(index + 1), Some(b'e' | b'i' | b'y')) => 's',
            b'c' => 'k',
            b'p' if next == Some(b'h') => {
                index += 1;
                'f'
            }
            b'y' if !next.is_some_and(is_vowel) => 'i',
            b'w' if after_vowel && !next.is_some_and(is_vowel) => 'u',
            other => char::from(other),
        };
        sound.push(mapped);
        index += 1;
    }
    sound
}

/// A run's sound shape: its words' [`word_sound`]s joined, with every
/// doubled letter collapsed to one ("post gress" and `Postgres` both
/// `postgres`). Vowels keep their letters, so "meet you" (`metu`) and
/// `Matteo` (`mateo`) differ.
fn sound_shape<S: AsRef<str>>(sounds: impl Iterator<Item = S>) -> String {
    let mut shape = String::new();
    for sound in sounds {
        for character in sound.as_ref().chars() {
            if !shape.ends_with(character) {
                shape.push(character);
            }
        }
    }
    shape
}

/// Runs of vowels in a [`sound_shape`].
fn vowel_groups(shape: &str) -> usize {
    let mut groups = 0;
    let mut previous_vowel = false;
    for &byte in shape.as_bytes() {
        let vowel = is_vowel(byte);
        if vowel && !previous_vowel {
            groups += 1;
        }
        previous_vowel = vowel;
    }
    groups
}

/// Estimated syllables: vowel groups per word, a silent final `e` dropped
/// (except after a consonant + `l`, as in "table"), at least one per word.
fn syllables(display: &str) -> usize {
    display
        .split(|character: char| !character.is_ascii_alphabetic())
        .filter(|word| !word.is_empty())
        .map(|word| {
            let word = word.to_ascii_lowercase();
            let bytes = word.as_bytes();
            let is_vowel = |byte: u8| matches!(byte, b'a' | b'e' | b'i' | b'o' | b'u' | b'y');
            let mut groups = 0;
            let mut previous_vowel = false;
            for &byte in bytes {
                let vowel = is_vowel(byte);
                if vowel && !previous_vowel {
                    groups += 1;
                }
                previous_vowel = vowel;
            }
            let length = bytes.len();
            if groups > 1
                && length >= 3
                && bytes[length - 1] == b'e'
                && !is_vowel(bytes[length - 2])
                && !(bytes[length - 2] == b'l' && !is_vowel(bytes[length - 3]))
            {
                groups -= 1;
            }
            // A one-syllable base plus a silent "-es" or "-ed" (James, Wales,
            // Charles). After a sibilant ("Moses") or, for "-ed", after t or d
            // the ending is voiced, and so is "-es" after a consonant + r or l
            // ("Postgres"). Longer words keep the count; at three or more
            // groups eligibility does not change anyway.
            if groups == 2 && length >= 4 && !is_vowel(bytes[length - 3]) {
                let before = bytes[length - 3];
                let cluster = matches!(before, b'r' | b'l')
                    && length >= 5
                    && !is_vowel(bytes[length - 4])
                    && !matches!(bytes[length - 4], b'r' | b'l');
                let silent = !cluster
                    && match &bytes[length - 2..] {
                        b"es" => !matches!(before, b's' | b'z' | b'x' | b'c' | b'g' | b'h'),
                        b"ed" => !matches!(before, b't' | b'd'),
                        _ => false,
                    };
                if silent {
                    groups -= 1;
                }
            }
            groups.max(1)
        })
        .sum()
}

/// The entry as the user wrote it. An all-lowercase entry takes a capital
/// only when the heard run started a sentence with one; any other casing
/// ("iPhone") is kept exactly.
fn written_form(display: &str, text: &str, start: usize) -> String {
    let heard_capital = text[start..].chars().next().is_some_and(char::is_uppercase);
    let before = &text[..start];
    let sentence = before.trim_end();
    let sentence_start = sentence.is_empty()
        || sentence.ends_with(['.', '!', '?'])
        || before[sentence.len()..].contains(['\n', '\r']);
    if !(heard_capital && sentence_start) || display.chars().any(char::is_uppercase) {
        return display.to_owned();
    }
    let mut characters = display.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().chain(characters).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests;
