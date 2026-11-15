//! Phonetic Dictionary correction for Local Transcripts.
//!
//! Exact-match correction ([`super::apply_user_vocabulary`]) only fixes a
//! hearing that already spells a Dictionary entry. A Local model that never
//! saw the name writes what it sounded like instead ("voice you" for
//! `Voisu`); Parakeet ignores prompts, so this pass is its only Dictionary
//! lever. A word, or a short run of words, is replaced by an entry when ALL of
//! these hold:
//!
//! * **eligible entry** — the entry has two or more syllables and only ASCII
//!   letters (plus separators). One-syllable names collide with too many
//!   ordinary words;
//! * **same sound code** — the run's Double Metaphone primary code (letters
//!   joined, no length cap) equals the entry's;
//! * **close sound shape** — the Levenshtein distance between the two
//!   [`sound_shape`]s is at most `max(1, shape_len / 4)`. Double Metaphone
//!   drops every non-initial vowel, so its code alone matches "face" to
//!   `Voisu`; the shape keeps where the vowels fall;
//! * **plain run** — the run has 1 to `entry words + 2` words (at most
//!   [`MAX_RUN_WORDS`]) joined only by spaces or hyphens, so it never crosses
//!   sentence or clause punctuation; it holds no digits or non-ASCII letters,
//!   sits outside spoken quotes and say-the-words spans, and is not part of a
//!   URL, path, email or identifier;
//! * **not a common word** — a single word on [`COMMON_WORDS`] is never
//!   swapped ("voice" stays; "voice you" may become `Voisu`);
//! * **unambiguous** — exactly one entry matches at that position, and no
//!   word of the run already spells an entry.
//!
//! The pass writes the entry as the user wrote it, keeping a sentence-start
//! capital for a lowercase entry, and replaces only the run's words, so the
//! punctuation around it survives. It is idempotent: a corrected run spells
//! its entry and is skipped next time.
//!
//! Adapted from VoxType `src/text/vocabulary.rs` (MIT License,
//! Copyright (c) 2025 Peter Jackson, <https://github.com/peteonrails/voxtype>,
//! commit `fbedd62`). Changes: two-syllable minimum instead of a letter
//! minimum, primary-code-only matching without the 4-character cap, a
//! sound-shape distance in place of a letter distance (so split hearings such
//! as "voice you" qualify), punctuation-free runs, a wider common-word guard,
//! and Voisu's quote and say-the-words masking.

use rphonetic::{DoubleMetaphone, Encoder};

use super::{USER_VOCABULARY_TERM_LIMIT, masked_ranges};

/// Longest run of heard words one entry may replace.
const MAX_RUN_WORDS: usize = 4;

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
    let all_terms: Vec<Entry> = terms
        .iter()
        .take(USER_VOCABULARY_TERM_LIMIT)
        .filter_map(|term| Entry::parse(term, &encoder))
        .collect();
    let eligible: Vec<&Entry> = all_terms.iter().filter(|entry| entry.eligible).collect();
    if eligible.is_empty() {
        return Vec::new();
    }
    let words = words(text);
    if words.is_empty() {
        return Vec::new();
    }
    let masked = masked_ranges(text);
    let blocked = spelled_entry_words(text, &words, &all_terms);

    let mut swaps = Vec::new();
    let mut index = 0;
    while index < words.len() {
        match best_match(text, &words, index, &eligible, &blocked, &masked, &encoder) {
            Some((count, entry)) => {
                let start = words[index].start;
                let end = words[index + count - 1].end;
                swaps.push(PhoneticSwap {
                    start,
                    end,
                    heard: text[start..end].to_owned(),
                    replacement: written_form(&entry.display, &text[start..end]),
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
        let words = display
            .split(|character: char| !character.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .count();
        let primary = encoder.encode(&letters);
        let ascii_letters_only = display
            .chars()
            .all(|character| !character.is_alphanumeric() || character.is_ascii_alphabetic());
        let eligible = ascii_letters_only && syllables(display) >= 2 && primary.len() >= 2;
        Some(Self {
            display: display.to_owned(),
            shape: sound_shape(&letters),
            letters,
            words,
            primary,
            eligible,
        })
    }

    fn max_run_words(&self) -> usize {
        (self.words + 2).min(MAX_RUN_WORDS)
    }
}

/// One heard word: alphanumerics with internal apostrophes ("you're").
struct Word {
    start: usize,
    end: usize,
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
        words.push(Word {
            start,
            end: last_start + last_char.len_utf8(),
        });
        index = last + 1;
    }
    words
}

/// Indices of words that already spell a Dictionary entry (any entry, even an
/// ineligible one). Exact-match correction owns them; the phonetic pass never
/// re-targets them.
fn spelled_entry_words(text: &str, words: &[Word], entries: &[Entry]) -> Vec<bool> {
    let mut blocked = vec![false; words.len()];
    for entry in entries {
        for start in 0..words.len() {
            for count in 1..=entry.max_run_words() {
                let Some(last) = words.get(start + count - 1) else {
                    break;
                };
                if letters_only(&text[words[start].start..last.end]) == entry.letters {
                    blocked[start..start + count].fill(true);
                }
            }
        }
    }
    blocked
}

/// The single entry that matches a run starting at `index`, with the run's
/// word count. Within one entry the closest sound shape wins, then the longer
/// run; two different entries matching here is ambiguous and yields none.
fn best_match<'a>(
    text: &str,
    words: &[Word],
    index: usize,
    entries: &[&'a Entry],
    blocked: &[bool],
    masked: &[std::ops::Range<usize>],
    encoder: &DoubleMetaphone,
) -> Option<(usize, &'a Entry)> {
    let mut found: Option<(usize, usize, &Entry)> = None;
    for entry in entries {
        let mut best: Option<(usize, usize)> = None;
        for count in 1..=entry.max_run_words() {
            let Some(last) = words.get(index + count - 1) else {
                break;
            };
            if blocked[index + count - 1] {
                break;
            }
            if count > 1 && !plain_gap(&text[words[index + count - 2].end..last.start]) {
                break;
            }
            let start = words[index].start;
            let end = last.end;
            let heard = &text[start..end];
            if !heard
                .chars()
                .all(|character| !character.is_alphanumeric() || character.is_ascii_alphabetic())
            {
                break;
            }
            if masked
                .iter()
                .any(|range| start < range.end && range.start < end)
                || in_technical_token(text, start, end)
            {
                break;
            }
            let letters = letters_only(heard);
            if count == 1 && is_common_word(&letters) {
                continue;
            }
            if encoder.encode(&letters) != entry.primary {
                continue;
            }
            let distance = strsim::levenshtein(&sound_shape(&letters), &entry.shape);
            if distance > (entry.shape.len() / 4).max(1) {
                continue;
            }
            if best.is_none_or(|(best_distance, _)| distance <= best_distance) {
                best = Some((distance, count));
            }
        }
        if let Some((distance, count)) = best {
            if found.is_some() {
                return None;
            }
            found = Some((distance, count, entry));
        }
    }
    found.map(|(_, count, entry)| (count, entry))
}

/// Words in a run may be joined by whitespace or a hyphen, nothing else.
fn plain_gap(gap: &str) -> bool {
    !gap.is_empty()
        && gap
            .chars()
            .all(|character| character.is_whitespace() || character == '-')
}

/// True when the run sits inside a whitespace-delimited chunk that looks like
/// a URL, path, email or identifier. Trailing sentence punctuation does not
/// count.
fn in_technical_token(text: &str, from: usize, to: usize) -> bool {
    let start = text[..from]
        .rfind(char::is_whitespace)
        .map_or(0, |index| index + 1);
    let end = text[to..]
        .find(char::is_whitespace)
        .map_or(text.len(), |index| to + index);
    let chunk = text[start..end].trim_end_matches(|character: char| {
        matches!(character, '.' | ',' | '!' | '?' | ';' | ':' | ')' | '"')
    });
    chunk.contains(['@', '/', '\\', '_', '.', ':'])
}

fn letters_only(value: &str) -> String {
    value
        .chars()
        .filter(char::is_ascii_alphabetic)
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

/// A coarse sound shape: lowercase letters with each run of vowels (`y`
/// included) collapsed to `a` and doubled consonants collapsed to one.
/// "voice you" and `Voisu` become `vaca` and `vasa`.
fn sound_shape(letters: &str) -> String {
    let mut shape = String::with_capacity(letters.len());
    for character in letters.chars() {
        let mapped = if matches!(character, 'a' | 'e' | 'i' | 'o' | 'u' | 'y') {
            'a'
        } else {
            character
        };
        if !shape.ends_with(mapped) {
            shape.push(mapped);
        }
    }
    shape
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
            groups.max(1)
        })
        .sum()
}

/// The entry as the user wrote it; a lowercase entry takes a capital when the
/// heard run started with one (a sentence start).
fn written_form(display: &str, heard: &str) -> String {
    let heard_capital = heard.chars().next().is_some_and(char::is_uppercase);
    let mut characters = display.chars();
    match characters.next() {
        Some(first) if heard_capital && first.is_lowercase() => {
            first.to_uppercase().chain(characters).collect()
        }
        _ => display.to_owned(),
    }
}

fn is_common_word(letters: &str) -> bool {
    COMMON_WORDS
        .split_ascii_whitespace()
        .any(|word| word == letters)
}

/// Ordinary English words that are never swapped on their own. It holds the
/// most frequent words plus everyday words that share a sound code with
/// typical Dictionary names, whitespace-separated and sorted (a test checks).
const COMMON_WORDS: &str = "\
    a able about above after again against age ago air all almost alone along already also \
    always am among an and anger another answer any anyone anything are area around art as \
    ask at away back bad bar base be became because become been before began begin behind \
    being believe below best better between big body book both box boy bring brought build \
    business but buy by call came can car care carry case cause center certain change check \
    child city class clear close code cold come common company could country course cover \
    current cut data day deal deep did different do does done door down during each early \
    easy eat else end energy enough even ever every eye face fact family far fast feel few \
    file find fine fire first five focus follow for form found four free friend from full \
    fuse fuss fussy game gave get give go going good got great group grow had half hand \
    happen happy hard has have he head hear heard help helper her here high highland him \
    his hold home honor house how however human hyper hyperlink i idea if important in into \
    invoice is issue it its just keep kind knew know land large last late later lead learn \
    least leave left less let letter life light like line list little live long look lot \
    love low made make man many matter may maybe me mean meet might mind money more most \
    move much music must my name near need never new next nice night no noise not nothing \
    now number of off office often oh ok okay old on once one only open or order other our \
    out over own page paper papery part party people peppery per person phase pipe place \
    plan play point power present problem program put question quite rage rather read ready \
    real really realm reason red right room run said same saw say school second see seem \
    send service set several she should show side since small so some something sound speak \
    stand start state still stop story study such sure system tail take talk team tell than \
    thank thanks that the their them then there these they thing think this those though \
    thought three through time to today together told too took toward try turn two under \
    until up upon us use used user using usual usually vase very via vice view visa vise \
    vista voice voiced voices wait walk want was watch water way we week well went were \
    what when where whether which while white who whole why wild will wire with within \
    without word work world would write year yes yet you young your yours";

#[cfg(test)]
mod tests;
