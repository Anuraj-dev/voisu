//! Tone: an optional informal pass over the final rendered transcript.
//!
//! `Formal` is the default and returns the text unchanged, so formal output
//! stays byte-identical to the formatter's. `Informal` swaps a closed table of
//! spoken phrases for chat abbreviations (lowercase, with a capital first
//! letter at a sentence start) and drops the final full stop. It runs once on the text
//! about to be delivered, so cloud and local ASR, and clean and exact Writing
//! Modes, all pass through the same stage.

use serde::{Deserialize, Serialize};

use crate::intent_routing::is_command_shaped;
use crate::local_baseline::{ascii_lower, metalinguistic_mask, quote_pairs_and_skip, word_tokens};

/// User-selected Tone, snapshotted at Recording start.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tone {
    /// Today's output, unchanged.
    #[default]
    Formal,
    /// Chat abbreviations and no final full stop.
    Informal,
}

impl Tone {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Formal => "formal",
            Self::Informal => "informal",
        }
    }

    /// Parses the persisted value. Anything else is `None`; callers fall
    /// back to `Formal`.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "formal" => Some(Self::Formal),
            "informal" => Some(Self::Informal),
            _ => None,
        }
    }
}

/// Applies `tone` to the final rendered text.
#[must_use]
pub fn apply_tone(text: &str, tone: Tone) -> String {
    match tone {
        Tone::Formal => text.to_owned(),
        Tone::Informal => drop_final_full_stop(&casual(text)),
    }
}

/// Where a table phrase may fire. Most phrases are also ordinary English, so
/// each one names the position that marks it as the speaker's chat phrase.
#[derive(Clone, Copy)]
enum Position {
    Anywhere,
    /// A request to the listener: imperative, or after "please" or "can you"
    /// ("he'll let me know" and "he called and let me know" stay).
    Request,
    /// A discourse marker: clause-initial, or right after a conjunction or
    /// interjection ("you to be honest" and "tell by the way he talks" stay).
    ClauseStart,
    /// A `ClauseStart` that must end its clause or precede one of these words
    /// ("I don't know what to do" is `idk`, "I don't know him" stays, and with
    /// no words, "not gonna lie, it works" is `ngl`, "not gonna lie down"
    /// stays).
    ClauseStartBefore(&'static [&'static str]),
    /// A clause of its own ("never mind the cost" and "it's no problem" stay).
    WholeClause,
    /// About the speaker: it absorbs one of these subjects at clause start
    /// ("I'll be right back" is `brb`, "he'll be right back" stays). With no
    /// subject it must open the sentence, since after "and" or a comma it may
    /// continue someone else's clause ("he left and will be right back" stays).
    Subject(&'static [&'static [&'static str]]),
    /// A `Subject` that must stand alone when it has no subject ("on my way
    /// home, I saw a dog" and "laughing out loud is good for you" stay).
    SubjectOrAlone(&'static [&'static [&'static str]]),
    /// The speaker leaving, which must open the sentence and end it ("last
    /// year, I got to go" and "I have to go, right?" stay). `bare` allows the
    /// phrase with no subject.
    Leaving {
        bare: bool,
    },
}

const I_WILL: &[&[&str]] = &[&["i'll"], &["i", "will"], &["will"]];
const I_AM: &[&[&str]] = &[&["i'm"], &["i", "am"]];
const I_AM_OR_WAS: &[&[&str]] = &[&["i'm"], &["i", "am"], &["i", "was"]];
const I_LEAVING: &[&[&str]] = &[&["i"], &["i've"]];
const REQUEST_VERBS: [&str; 4] = ["can", "could", "would", "will"];
const AFTER_IDK: &[&str] = &[
    "what", "where", "when", "who", "why", "how", "if", "whether", "which", "about", "but",
    "though",
];

/// Spoken phrases (lowercase words) and their abbreviations. Closed: add a
/// phrase only through the spec. Matching prefers the longest phrase.
const PHRASES: &[(&[&str], &str, Position)] = &[
    (&["by", "the", "way"], "btw", Position::ClauseStart),
    (&["oh", "my", "god"], "omg", Position::Anywhere),
    (&["oh", "my", "gosh"], "omg", Position::Anywhere),
    (
        &["for", "your", "information"],
        "fyi",
        Position::ClauseStart,
    ),
    (
        &["as", "soon", "as", "possible"],
        "asap",
        Position::Anywhere,
    ),
    (&["to", "be", "honest"], "tbh", Position::ClauseStart),
    (&["to", "be", "fair"], "tbf", Position::ClauseStart),
    (
        &["not", "gonna", "lie"],
        "ngl",
        Position::ClauseStartBefore(&[]),
    ),
    (&["in", "my", "opinion"], "imo", Position::ClauseStart),
    (
        &["in", "my", "humble", "opinion"],
        "imho",
        Position::ClauseStart,
    ),
    (
        &["as", "far", "as", "i", "know"],
        "afaik",
        Position::Anywhere,
    ),
    (
        &["i", "don't", "know"],
        "idk",
        Position::ClauseStartBefore(AFTER_IDK),
    ),
    (&["never", "mind"], "nvm", Position::WholeClause),
    (&["no", "problem"], "np", Position::WholeClause),
    (&["let", "me", "know"], "lmk", Position::Request),
    (&["what", "the", "fuck"], "wtf", Position::Anywhere),
    (&["in", "real", "life"], "irl", Position::Anywhere),
    (&["away", "from", "keyboard"], "afk", Position::Anywhere),
    (&["on", "my", "way"], "omw", Position::SubjectOrAlone(I_AM)),
    (&["be", "right", "back"], "brb", Position::Subject(I_WILL)),
    (
        &["talk", "to", "you", "later"],
        "ttyl",
        Position::Subject(I_WILL),
    ),
    (
        &["shaking", "my", "head"],
        "smh",
        Position::SubjectOrAlone(I_AM),
    ),
    (
        &["laughing", "out", "loud"],
        "lol",
        Position::SubjectOrAlone(I_AM_OR_WAS),
    ),
    (
        &["laughing", "my", "ass", "off"],
        "lmao",
        Position::SubjectOrAlone(I_AM_OR_WAS),
    ),
    (
        &["rolling", "on", "the", "floor", "laughing"],
        "rofl",
        Position::SubjectOrAlone(I_AM_OR_WAS),
    ),
    (
        &["got", "to", "go"],
        "g2g",
        Position::Leaving { bare: true },
    ),
    (&["gotta", "go"], "g2g", Position::Leaving { bare: true }),
    (
        &["have", "to", "go"],
        "g2g",
        Position::Leaving { bare: false },
    ),
];

/// Words that may precede a discourse marker without clause punctuation.
const DISCOURSE_LEADS: [&str; 8] = ["and", "but", "so", "well", "oh", "hey", "ok", "okay"];

/// Words that may also lead a sentence before a phrase about the speaker
/// ("sorry, I got to go"). Mid-sentence they mark nothing ("I'm sorry for your
/// information being leaked").
const SENTENCE_OPENERS: [&str; 3] = ["sorry", "anyway", "alright"];

/// Punctuation that may trail a phrase's last word. It is ignored for
/// matching and kept after the abbreviation.
const TRAILING_PUNCT: [char; 6] = [',', '.', '!', '?', ';', ':'];

/// Closes a quote or bracket after a word's own punctuation.
const CLOSERS: [char; 3] = ['"', '\u{201d}', ')'];

/// A trailing period after one of these closes the abbreviation, not the
/// sentence.
const ABBREVIATION_STEMS: [&str; 13] = [
    "etc", "mr", "mrs", "ms", "dr", "vs", "inc", "jr", "sr", "st", "ltd", "co", "prof",
];

fn casual(text: &str) -> String {
    // Lines inside a ``` fence are code and stay untouched.
    let mut in_fence = false;
    text.split('\n')
        .map(|line| {
            if line.trim_start().starts_with("```") {
                in_fence = !in_fence;
                line.to_owned()
            } else if in_fence || is_command_shaped(line) {
                line.to_owned()
            } else {
                case_abbreviations(&abbreviate_line(line))
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn abbreviate_line(line: &str) -> String {
    let tokens = word_tokens(line);
    let skip = protected_mask(&tokens);
    let lower: Vec<String> = tokens
        .iter()
        .map(|(_, _, tok)| ascii_lower(tok).replace('\u{2019}', "'"))
        .collect();
    let words = Words {
        tokens: &tokens,
        lower: &lower,
        skip: &skip,
    };

    let mut out = String::with_capacity(line.len());
    let mut cursor = 0usize;
    let mut floor = 0usize;
    let mut i = 0usize;
    while i < tokens.len() {
        let Some(hit) = words.match_at(i, floor) else {
            i += 1;
            continue;
        };
        let last = i + hit.len - 1;
        out.push_str(&line[cursor..tokens[i - hit.subject].0]);
        out.push_str(hit.abbreviation);
        out.push_str(hit.suffix);
        cursor = tokens[last].1;
        i = last + 1;
        floor = i;
    }
    out.push_str(&line[cursor..]);
    out
}

/// A phrase found at a token: its token count, the subject tokens it absorbs
/// before it, its abbreviation, and the punctuation that trailed its last word.
struct Hit<'a> {
    len: usize,
    subject: usize,
    abbreviation: &'static str,
    suffix: &'a str,
}

struct Words<'t, 'a> {
    tokens: &'t [(usize, usize, &'a str)],
    lower: &'t [String],
    skip: &'t [bool],
}

impl<'a> Words<'_, 'a> {
    /// Longest table phrase starting at token `i`. A subject it absorbs may
    /// not reach back before `floor`, the end of the previous phrase.
    fn match_at(&self, i: usize, floor: usize) -> Option<Hit<'a>> {
        let mut best: Option<Hit<'a>> = None;
        for &(words, abbreviation, position) in PHRASES {
            let n = words.len();
            if i + n > self.tokens.len() || best.as_ref().is_some_and(|hit| hit.len >= n) {
                continue;
            }
            if self.skip[i..i + n].iter().any(|&s| s) {
                continue;
            }
            let last = i + n - 1;
            if words[..n - 1]
                .iter()
                .zip(&self.lower[i..last])
                .any(|(word, tok)| word != tok)
            {
                continue;
            }
            if self.lower[last].trim_end_matches(TRAILING_PUNCT) != words[n - 1] {
                continue;
            }
            let raw_last = self.tokens[last].2;
            let suffix = &raw_last[raw_last.trim_end_matches(TRAILING_PUNCT).len()..];
            let Some(subject) = self.position_allows(position, i, last, floor) else {
                continue;
            };
            best = Some(Hit {
                len: n,
                subject,
                abbreviation,
                suffix,
            });
        }
        best
    }

    /// The subject tokens to absorb if `position` allows a phrase spanning
    /// tokens `i..=last`.
    fn position_allows(
        &self,
        position: Position,
        i: usize,
        last: usize,
        floor: usize,
    ) -> Option<usize> {
        let leads_clause = || self.starts_clause(i) || self.opens_sentence(i);
        match position {
            Position::Anywhere => Some(0),
            Position::Request => self.is_request(i).then_some(0),
            Position::ClauseStart => self.starts_clause(i).then_some(0),
            Position::ClauseStartBefore(words) => (leads_clause()
                && (self.ends_clause(last) || self.next_word_in(last, words)))
            .then_some(0),
            // "Never mind I found it" without a comma stays unchanged, so a
            // discourse lead does not count here.
            Position::WholeClause => {
                (self.ends_clause(last) && (i == 0 || self.after_punctuation(i))).then_some(0)
            }
            Position::Subject(subjects) => self
                .subject_before(i, floor, subjects, false)
                .or_else(|| (self.opens_sentence(i) || self.after_one_word(i)).then_some(0)),
            Position::SubjectOrAlone(subjects) => self
                .subject_before(i, floor, subjects, false)
                .or_else(|| (self.opens_sentence(i) && self.ends_thought(last)).then_some(0)),
            Position::Leaving { bare } => {
                if !self.ends_thought(last) {
                    return None;
                }
                self.subject_before(i, floor, I_LEAVING, true)
                    .or_else(|| (bare && self.opens_sentence(i)).then_some(0))
            }
        }
    }

    /// How many tokens of an allowed subject sit before `i`, with the subject
    /// itself leading a clause. A subject naming the speaker may follow "and"
    /// or a comma unless `strict`; bare "will" may not, because "he left and
    /// will be right back" is someone else's clause.
    fn subject_before(
        &self,
        i: usize,
        floor: usize,
        subjects: &[&[&str]],
        strict: bool,
    ) -> Option<usize> {
        subjects.iter().find_map(|subject| {
            let n = subject.len();
            if i < n || i - n < floor {
                return None;
            }
            let start = i - n;
            let matches = self.skip[start..i].iter().all(|&s| !s)
                && subject
                    .iter()
                    .zip(&self.lower[start..i])
                    .all(|(w, t)| w == t);
            let names_speaker = subject[0].starts_with('i');
            let leads = self.opens_sentence(start)
                || (!strict && names_speaker && self.starts_clause(start));
            (matches && leads).then_some(n)
        })
    }

    fn is_request(&self, i: usize) -> bool {
        if i == 0 || self.after_punctuation(i) || self.opens_sentence(i) {
            return true;
        }
        match self.lower[i - 1].as_str() {
            "please" => true,
            "just" => self.opens_sentence(i - 1),
            "you" => i >= 2 && REQUEST_VERBS.contains(&self.lower[i - 2].as_str()),
            _ => false,
        }
    }

    fn starts_clause(&self, i: usize) -> bool {
        i == 0 || self.after_punctuation(i) || DISCOURSE_LEADS.contains(&self.lower[i - 1].as_str())
    }

    /// Whether token `i` begins a sentence, allowing leading interjections
    /// ("sorry, I got to go") but no other words before it.
    fn opens_sentence(&self, i: usize) -> bool {
        if self.after_sentence_mark(i) {
            return true;
        }
        let lead = self.lower[i - 1].trim_end_matches(TRAILING_PUNCT);
        (DISCOURSE_LEADS.contains(&lead) || SENTENCE_OPENERS.contains(&lead))
            && self.opens_sentence(i - 1)
    }

    fn after_sentence_mark(&self, i: usize) -> bool {
        if i == 0 {
            return true;
        }
        let token = self.tokens[i - 1].2;
        ends_sentence(token, i - 1) || token.trim_end_matches(CLOSERS).ends_with([';', ':'])
    }

    /// Whether a one-word clause and a comma lead into token `i` ("thanks, talk
    /// to you later"), as opposed to a longer clause ("she walked in, laughing
    /// out loud").
    fn after_one_word(&self, i: usize) -> bool {
        i > 0 && self.tokens[i - 1].2.ends_with(',') && self.after_sentence_mark(i - 1)
    }

    fn after_punctuation(&self, i: usize) -> bool {
        self.tokens[i - 1]
            .2
            .trim_end_matches(CLOSERS)
            .ends_with(TRAILING_PUNCT)
    }

    fn ends_clause(&self, last: usize) -> bool {
        last + 1 == self.tokens.len() || self.tokens[last].2.ends_with(TRAILING_PUNCT)
    }

    /// Whether the phrase ending at `last` is a statement that is over: a comma
    /// may continue it ("on my way, I saw a dog"), and a question is not one.
    fn ends_thought(&self, last: usize) -> bool {
        let token = self.tokens[last].2;
        !token.ends_with('?') && (last + 1 == self.tokens.len() || token.ends_with(['.', '!', ';']))
    }

    fn next_word_in(&self, last: usize, words: &[&str]) -> bool {
        self.lower
            .get(last + 1)
            .is_some_and(|next| words.contains(&next.trim_end_matches(TRAILING_PUNCT)))
    }
}

/// Tokens a phrase may not touch: spoken `quote … unquote` and
/// `words … out loud` spans, and anything inside or carrying a quotation
/// mark or backtick. Technical tokens (URLs, paths, flags) never match a
/// phrase word, since phrase words are plain lowercase letters.
fn protected_mask(tokens: &[(usize, usize, &str)]) -> Vec<bool> {
    let (_, mut skip) = quote_pairs_and_skip(tokens);
    for (slot, meta) in skip.iter_mut().zip(metalinguistic_mask(tokens)) {
        *slot |= meta;
    }
    let (mut straight, mut curly, mut code) = (false, false, false);
    for (slot, (_, _, tok)) in skip.iter_mut().zip(tokens) {
        let mut marked = straight || curly || code;
        for ch in tok.chars() {
            match ch {
                '"' => straight = !straight,
                '\u{201c}' => curly = true,
                '\u{201d}' => curly = false,
                '`' => code = !code,
                _ => continue,
            }
            marked = true;
        }
        *slot |= marked;
    }
    skip
}

/// Cases the chat abbreviations: lowercase mid-sentence, a capital first
/// letter at a sentence start (`Btw`). Everything else keeps the formatter's
/// case, so sentence starts, the pronoun `I` and names stay as written.
/// Quotes and code are untouched.
fn case_abbreviations(line: &str) -> String {
    let tokens = word_tokens(line);
    let skip = protected_mask(&tokens);
    let mut out = String::with_capacity(line.len());
    let mut cursor = 0usize;
    for (i, &(start, end, token)) in tokens.iter().enumerate() {
        out.push_str(&line[cursor..start]);
        let starts_sentence = i == 0 || ends_sentence(tokens[i - 1].2, i - 1);
        if skip[i] || !is_abbreviation(token) {
            out.push_str(token);
        } else if starts_sentence {
            let lower = token.to_ascii_lowercase();
            let mut chars = lower.chars();
            if let Some(first) = chars.next() {
                out.extend(first.to_uppercase().chain(chars));
            }
        } else {
            out.push_str(&token.to_ascii_lowercase());
        }
        cursor = end;
    }
    out.push_str(&line[cursor..]);
    out
}

/// A table abbreviation, in any case, with its trailing punctuation.
fn is_abbreviation(token: &str) -> bool {
    let word = token.trim_end_matches(TRAILING_PUNCT);
    PHRASES
        .iter()
        .any(|(_, abbreviation, _)| word.eq_ignore_ascii_case(abbreviation))
}

/// Whether `token`, at index `index` of its line, ends a sentence. A period
/// closing an abbreviation (`Mr.`, `p.m.`) or a leading list number (`1.`) does
/// not.
fn ends_sentence(token: &str, index: usize) -> bool {
    let end = token.trim_end_matches(CLOSERS);
    if end.ends_with(['!', '?']) {
        return true;
    }
    if !end.ends_with('.') {
        return false;
    }
    let stem = end.trim_end_matches('.');
    let is_list_number = index == 0 && !stem.is_empty() && stem.chars().all(|c| c.is_ascii_digit());
    !stem.contains('.')
        && !is_list_number
        && !ABBREVIATION_STEMS.contains(&stem.to_ascii_lowercase().as_str())
}

/// Drops the message's last full stop. Earlier periods, `?` and `!` stay.
/// A period that closes an abbreviation or file name (`p.m.`, `e.g.`,
/// `config.toml.`), an ellipsis, a quote, or a command line also stays.
fn drop_final_full_stop(text: &str) -> String {
    let trimmed = text.trim_end();
    let trail = &text[trimmed.len()..];
    let Some(body) = trimmed.strip_suffix('.') else {
        return text.to_owned();
    };
    let Some(last_word) = body.split_whitespace().next_back() else {
        return text.to_owned();
    };
    let last_line = trimmed.rsplit('\n').next().unwrap_or(trimmed);
    if last_word.contains('.')
        || ABBREVIATION_STEMS.contains(&last_word.to_ascii_lowercase().as_str())
        || !body.ends_with(last_word)
        || is_command_shaped(last_line)
    {
        return text.to_owned();
    }
    format!("{body}{trail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn informal(text: &str) -> String {
        apply_tone(text, Tone::Informal)
    }

    /// Every table phrase, as a formatted sentence and the expected output.
    const PHRASES: &[(&str, &str)] = &[
        ("By the way, it shipped.", "Btw, it shipped"),
        ("Oh my god, it shipped.", "Omg, it shipped"),
        ("Oh my gosh, it shipped.", "Omg, it shipped"),
        ("For your information, it shipped.", "Fyi, it shipped"),
        ("Send it as soon as possible.", "Send it asap"),
        ("To be honest, it shipped.", "Tbh, it shipped"),
        ("Not gonna lie, it shipped.", "Ngl, it shipped"),
        ("In my opinion, it shipped.", "Imo, it shipped"),
        ("In my humble opinion, it shipped.", "Imho, it shipped"),
        ("As far as I know, it shipped.", "Afaik, it shipped"),
        ("I don't know.", "Idk"),
        ("Never mind.", "Nvm"),
        ("No problem.", "Np"),
        ("Let me know if it works.", "Lmk if it works"),
        ("I'm on my way.", "Omw"),
        ("What the fuck is this?", "Wtf is this?"),
        ("I'll be right back.", "Brb"),
        ("Talk to you later.", "Ttyl"),
        ("We met in real life.", "We met irl"),
        ("I was away from keyboard.", "I was afk"),
        ("I am shaking my head.", "Smh"),
        ("I was laughing out loud.", "Lol"),
        ("I was laughing my ass off.", "Lmao"),
        ("I was rolling on the floor laughing.", "Rofl"),
        ("Sorry, I got to go.", "Sorry, g2g"),
        ("I gotta go.", "G2g"),
        ("I have to go.", "G2g"),
        ("To be fair, it shipped.", "Tbf, it shipped"),
    ];

    #[test]
    fn formal_is_byte_identical_for_the_phrase_corpus() {
        for (input, _) in PHRASES {
            assert_eq!(apply_tone(input, Tone::Formal), *input);
        }
        let messy = "  by the way , never mind the cost.\n\nTalk to you later!  I think.  ";
        assert_eq!(apply_tone(messy, Tone::Formal), messy);
    }

    #[test]
    fn informal_converts_every_phrase() {
        for (input, expected) in PHRASES {
            assert_eq!(informal(input), *expected, "input: {input}");
        }
    }

    #[test]
    fn matching_ignores_case_and_keeps_trailing_punctuation() {
        assert_eq!(informal("by the way, it shipped"), "Btw, it shipped");
        assert_eq!(informal("BY THE WAY it shipped"), "Btw it shipped");
        assert_eq!(informal("It shipped, by the way."), "It shipped, btw");
        assert_eq!(informal("Did it ship, by the way?"), "Did it ship, btw?");
        assert_eq!(informal("Oh my god!"), "Omg!");
        assert_eq!(informal("To be honest; it shipped"), "Tbh; it shipped");
    }

    #[test]
    fn partial_phrases_and_lone_words_never_fire() {
        for (input, expected) in [
            ("By the river.", "By the river"),
            ("Is it by the", "Is it by the"),
            ("The way it works.", "The way it works"),
            ("Be honest with me", "Be honest with me"),
            ("As far as Paris", "As far as Paris"),
            ("We got to the station", "We got to the station"),
            ("A real life story", "A real life story"),
        ] {
            assert_eq!(informal(input), expected, "input: {input}");
        }
    }

    #[test]
    fn phrases_need_word_boundaries() {
        assert_eq!(informal("Standby the way"), "Standby the way");
        assert_eq!(informal("by the wayside"), "by the wayside");
        // Punctuation inside the phrase breaks it.
        assert_eq!(informal("By the, way"), "By the, way");
    }

    #[test]
    fn discourse_markers_need_a_clause_position() {
        assert_eq!(
            informal("I want you to be honest."),
            "I want you to be honest"
        );
        assert_eq!(
            informal("It's important to be fair to everyone."),
            "It's important to be fair to everyone"
        );
        assert_eq!(
            informal("You can tell by the way he talks."),
            "You can tell by the way he talks"
        );
        assert_eq!(
            informal("Thanks for your information."),
            "Thanks for your information"
        );
        assert_eq!(informal("By the way, it works."), "Btw, it works");
        assert_eq!(informal("And by the way it works."), "And btw it works");
        assert_eq!(informal("It was, to be honest, fine."), "It was, tbh, fine");
    }

    #[test]
    fn i_dont_know_needs_a_clause_position() {
        assert_eq!(informal("I don't know what to do."), "Idk what to do");
        assert_eq!(informal("I don\u{2019}t know."), "Idk");
        assert_eq!(informal("Honestly, I don't know."), "Honestly, idk");
        assert_eq!(informal("But I don't know."), "But idk");
        assert_eq!(
            informal("That is what I don't know."),
            "That is what I don't know"
        );
    }

    #[test]
    fn fenced_code_is_unchanged_and_abbreviations_keep_their_stop() {
        assert_eq!(informal("```\nBy the way\n```"), "```\nBy the way\n```");
        assert_eq!(
            informal("Bring apples, pears, etc."),
            "Bring apples, pears, etc."
        );
        assert_eq!(informal("Say hi to Mr."), "Say hi to Mr.");
    }

    #[test]
    fn a_subject_before_the_phrase_is_absorbed_only_at_clause_start() {
        assert_eq!(informal("Sorry, I'll be right back."), "Sorry, brb");
        assert_eq!(informal("I will be right back."), "Brb");
        assert_eq!(informal("Be right back."), "Brb");
        assert_eq!(informal("I'll talk to you later."), "Ttyl");
        assert_eq!(informal("I'm on my way to work."), "Omw to work");
        assert_eq!(informal("I\u{2019}m on my way."), "Omw");
        assert_eq!(informal("On my way."), "Omw");
        assert_eq!(informal("I'm shaking my head."), "Smh");
        assert_eq!(informal("I'm laughing my ass off."), "Lmao");
        assert_eq!(informal("And I'm on my way."), "And omw");
    }

    #[test]
    fn phrases_about_someone_else_stay_as_words() {
        assert_eq!(informal("He'll be right back."), "He'll be right back");
        assert_eq!(
            informal("I want to talk to you later."),
            "I want to talk to you later"
        );
        assert_eq!(informal("It's on my way."), "It's on my way");
        assert_eq!(
            informal("I'll stop by on my way home."),
            "I'll stop by on my way home"
        );
        assert_eq!(
            informal("He was laughing out loud."),
            "He was laughing out loud"
        );
        assert_eq!(
            informal("So I said what I'm on my way."),
            "So I said what I'm on my way"
        );
    }

    #[test]
    fn let_me_know_stays_when_it_is_about_someone_else() {
        assert_eq!(informal("Please let me know."), "Please lmk");
        assert_eq!(informal("Can you let me know?"), "Can you lmk?");
        assert_eq!(
            informal("He forgot to let me know."),
            "He forgot to let me know"
        );
        assert_eq!(
            informal("She didn't let me know."),
            "She didn't let me know"
        );
        for unchanged in [
            "my manager let me know she was out",
            "he'll let me know",
            "he hasn't let me know",
            "she just let me know",
            "the email let me know it failed",
            "he called and let me know",
        ] {
            assert_eq!(informal(unchanged), unchanged);
        }
        assert_eq!(informal("Just let me know."), "Just lmk");
        assert_eq!(informal("Could you let me know?"), "Could you lmk?");
        assert_eq!(informal("If it works, let me know."), "If it works, lmk");
        assert_eq!(informal("Ok let me know."), "Ok lmk");
    }

    #[test]
    fn a_coordinated_verb_about_someone_else_stays_as_words() {
        for unchanged in [
            "he left and will be right back",
            "he went to lunch but will be right back",
            "he'll grab it and be right back",
            "she'll finish up and talk to you later",
            "he stepped out and will talk to you later",
            "john is out, will be right back",
            "she walked in, laughing out loud",
            "he kept dancing and laughing out loud",
        ] {
            assert_eq!(informal(unchanged), unchanged);
        }
        assert_eq!(informal("Will be right back."), "Brb");
        assert_eq!(
            informal("He left and I'll be right back."),
            "He left and brb"
        );
        assert_eq!(informal("Thanks, talk to you later."), "Thanks, ttyl");
        assert_eq!(informal("Ok, be right back."), "Ok, brb");
        assert_eq!(informal("Sorry I'll be right back."), "Sorry brb");
    }

    #[test]
    fn on_my_way_without_a_subject_must_stand_alone() {
        for (input, expected) in [
            (
                "On my way home, I saw a dog.",
                "On my way home, I saw a dog",
            ),
            ("On my way, I saw a dog.", "On my way, I saw a dog"),
            (
                "I went to the store and on my way back I saw him.",
                "I went to the store and on my way back I saw him",
            ),
            (
                "So on my way here I got lost.",
                "So on my way here I got lost",
            ),
            (
                "He went home, on my way here.",
                "He went home, on my way here",
            ),
        ] {
            assert_eq!(informal(input), expected, "input: {input}");
        }
        assert_eq!(informal("Ok, on my way!"), "Ok, omw!");
    }

    #[test]
    fn got_to_go_as_a_past_chance_stays_as_words() {
        for (input, expected) in [
            ("Last year, I got to go.", "Last year, I got to go"),
            ("Finally, I got to go.", "Finally, I got to go"),
            (
                "My dad had tickets and I got to go!",
                "My dad had tickets and I got to go!",
            ),
            (
                "I got to go, and it was great.",
                "I got to go, and it was great",
            ),
            (
                "So I got to go, which was great.",
                "So I got to go, which was great",
            ),
            (
                "We went to the park and got to go.",
                "We went to the park and got to go",
            ),
            ("Wait, I have to go?", "Wait, I have to go?"),
        ] {
            assert_eq!(informal(input), expected, "input: {input}");
        }
        assert_eq!(informal("Anyway, I gotta go."), "Anyway, g2g");
        assert_eq!(informal("Ok, I have to go."), "Ok, g2g");
    }

    #[test]
    fn literal_lie_and_know_stay_as_words() {
        assert_eq!(
            informal("Not going to lie down yet."),
            "Not going to lie down yet"
        );
        assert_eq!(informal("Not gonna lie to you."), "Not gonna lie to you");
        assert_eq!(
            informal("Not gonna lie I love it."),
            "Not gonna lie I love it"
        );
        assert_eq!(
            informal("Not going to lie anymore."),
            "Not going to lie anymore"
        );
        assert_eq!(
            informal("Not going to lie under oath."),
            "Not going to lie under oath"
        );
        assert_eq!(informal("Not gonna lie."), "Ngl");
        assert_eq!(
            informal("She is honest and not going to lie."),
            "She is honest and not going to lie"
        );
        assert_eq!(
            informal("Not going to lie, it shipped."),
            "Not going to lie, it shipped"
        );
        assert_eq!(informal("I don't know him."), "I don't know him");
        assert_eq!(informal("I don't know John."), "I don't know John");
        assert_eq!(
            informal("I don't know anyone here."),
            "I don't know anyone here"
        );
        assert_eq!(informal("I don't know, maybe."), "Idk, maybe");
        assert_eq!(informal("I don't know if it works."), "Idk if it works");
        assert_eq!(informal("Sorry I don't know."), "Sorry idk");
    }

    #[test]
    fn a_goodbye_must_end_the_sentence() {
        for (input, expected) in [
            ("I have to go?", "I have to go?"),
            ("I have to go, right?", "I have to go, right?"),
            ("I have to go, don't I?", "I have to go, don't I?"),
            (
                "I have to go, unless it rains.",
                "I have to go, unless it rains",
            ),
            (
                "I got to go, thanks to my dad.",
                "I got to go, thanks to my dad",
            ),
            ("I have to go. But I'll call you.", "G2g. But I'll call you"),
            ("Gotta go. So talk to you later.", "G2g. So ttyl"),
        ] {
            assert_eq!(informal(input), expected, "input: {input}");
        }
    }

    #[test]
    fn bare_laughing_phrases_must_stand_alone() {
        for (input, expected) in [
            (
                "Laughing out loud is good for you.",
                "Laughing out loud is good for you",
            ),
            (
                "Laughing out loud, she walked in.",
                "Laughing out loud, she walked in",
            ),
            (
                "She, laughing out loud, walked in.",
                "She, laughing out loud, walked in",
            ),
            (
                "Rolling on the floor laughing is not normal.",
                "Rolling on the floor laughing is not normal",
            ),
            ("Shaking my head, she left.", "Shaking my head, she left"),
            ("Laughing out loud.", "Lol"),
            ("I was laughing out loud at it.", "Lol at it"),
        ] {
            assert_eq!(informal(input), expected, "input: {input}");
        }
    }

    #[test]
    fn an_abbreviation_stop_does_not_open_a_sentence() {
        assert_eq!(
            informal("The Dr. will be right back."),
            "The Dr. will be right back"
        );
        assert_eq!(
            informal("Acme Inc. will talk to you later."),
            "Acme Inc. will talk to you later"
        );
        assert_eq!(informal("Wait. Will be right back."), "Wait. Brb");
    }

    #[test]
    fn a_question_about_letting_me_know_stays_as_words() {
        assert_eq!(informal("Did you let me know?"), "Did you let me know?");
        assert_eq!(informal("On my way?"), "On my way?");
    }

    #[test]
    fn sorry_mid_sentence_is_not_a_clause_lead() {
        assert_eq!(
            informal("I'm sorry for your information being leaked."),
            "I'm sorry for your information being leaked"
        );
        assert_eq!(informal("I'm sorry I got to go."), "I'm sorry I got to go");
    }

    #[test]
    fn no_problem_fires_only_when_it_is_a_whole_clause() {
        assert_eq!(informal("Thanks, no problem."), "Thanks, np");
        assert_eq!(informal("It's no problem."), "It's no problem");
        assert_eq!(informal("No problem with that."), "No problem with that");
    }

    #[test]
    fn leaving_phrases_fire_only_when_they_end_a_clause_for_the_speaker() {
        assert_eq!(
            informal("I got to go to the store."),
            "I got to go to the store"
        );
        assert_eq!(
            informal("I have to go to the store."),
            "I have to go to the store"
        );
        assert_eq!(
            informal("We got to go home, then slept."),
            "We got to go home, then slept"
        );
        assert_eq!(
            informal("We got to go, and it was great."),
            "We got to go, and it was great"
        );
        assert_eq!(informal("I got to go, bye."), "I got to go, bye");
        assert_eq!(informal("Gotta go, bye."), "Gotta go, bye");
        assert_eq!(informal("I gotta go, right?"), "I gotta go, right?");
        assert_eq!(
            informal("Gotta go, but I'll call you later."),
            "Gotta go, but I'll call you later"
        );
        assert_eq!(informal("Gotta go?"), "Gotta go?");
        assert_eq!(informal("Sorry I got to go."), "Sorry g2g");
        assert_eq!(informal("I've got to go."), "G2g");
        assert_eq!(informal("Gotta go."), "G2g");
        assert_eq!(informal("Got to go."), "G2g");
        assert_eq!(informal("I never got to go."), "I never got to go");
        assert_eq!(informal("You have to go."), "You have to go");
        assert_eq!(informal("You got to go."), "You got to go");
        assert_eq!(informal("Do I have to go?"), "Do I have to go?");
    }

    #[test]
    fn never_mind_fires_only_when_it_is_a_whole_clause() {
        assert_eq!(informal("Never mind the cost."), "Never mind the cost");
        assert_eq!(informal("Never mind that"), "Never mind that");
        assert_eq!(informal("I never mind waiting."), "I never mind waiting");
        assert_eq!(informal("I never mind."), "I never mind");
        assert_eq!(informal("Never mind"), "Nvm");
        assert_eq!(informal("never mind."), "Nvm");
        assert_eq!(informal("Never mind, I found it."), "Nvm, I found it");
        assert_eq!(informal("Oh, never mind."), "Oh, nvm");
        assert_eq!(informal("It broke. Never mind!"), "It broke. Nvm!");
        assert_eq!(
            informal("Wait.\nNever mind, it works."),
            "Wait.\nNvm, it works"
        );
    }

    #[test]
    fn quoted_spans_are_unchanged() {
        assert_eq!(
            informal(r#"She wrote "By the way, I agree." twice."#),
            r#"She wrote "By the way, I agree." twice"#
        );
        assert_eq!(
            informal("She wrote \u{201c}To be honest\u{201d} twice."),
            "She wrote \u{201c}To be honest\u{201d} twice"
        );
        assert_eq!(
            informal(r#"He said "never mind." By the way, hi."#),
            r#"He said "never mind." Btw, hi"#
        );
        assert_eq!(
            informal("Type `I think. So` here."),
            "Type `I think. So` here"
        );
        assert_eq!(
            informal("Say quote I think unquote now"),
            "Say quote I think unquote now"
        );
    }

    #[test]
    fn command_lines_are_unchanged() {
        let command = "git commit -m by the way.";
        assert_eq!(informal(command), command);
        assert_eq!(informal("Git commit -m hello"), "Git commit -m hello");
        assert_eq!(
            informal("By the way, run this.\ncargo test --by the way"),
            "Btw, run this.\ncargo test --by the way"
        );
        assert_eq!(
            informal("Hello\nGit commit -m hi"),
            "Hello\nGit commit -m hi"
        );
    }

    #[test]
    fn only_the_final_full_stop_is_dropped() {
        assert_eq!(informal("It shipped. It works."), "It shipped. It works");
        assert_eq!(
            informal("It shipped. Does it work?"),
            "It shipped. Does it work?"
        );
        assert_eq!(informal("It shipped. It works!"), "It shipped. It works!");
        assert_eq!(informal("It shipped. It works"), "It shipped. It works");
        assert_eq!(informal("First.\n\nSecond.\n"), "First.\n\nSecond\n");
        assert_eq!(informal("IMO we shouldn't wait."), "Imo we shouldn't wait");
        assert_eq!(informal("I said no."), "I said no");
        assert_eq!(informal(""), "");
        assert_eq!(informal("."), ".");
    }

    #[test]
    fn final_full_stop_on_abbreviations_ellipses_and_quotes_stays() {
        assert_eq!(informal("Meet at 5 p.m."), "Meet at 5 p.m.");
        assert_eq!(informal("Bring fruit, e.g."), "Bring fruit, e.g.");
        assert_eq!(informal("Open config.toml."), "Open config.toml.");
        assert_eq!(informal("Well..."), "Well...");
        assert_eq!(informal(r#"He said "hi.""#), r#"He said "hi.""#);
    }

    #[test]
    fn sentence_starts_and_every_i_keep_their_capital() {
        assert_eq!(informal("Hello there."), "Hello there");
        assert_eq!(informal("A quick one."), "A quick one");
        assert_eq!(informal("Élan matters."), "Élan matters");
        assert_eq!(informal("  Hello there."), "  Hello there");
        assert_eq!(informal("Hi. How are you?"), "Hi. How are you?");
        assert_eq!(informal("Hi! How are you?"), "Hi! How are you?");
        assert_eq!(informal("Why? Because."), "Why? Because");
        assert_eq!(informal("Well... Then what?"), "Well... Then what?");
        assert_eq!(informal("No. Thanks."), "No. Thanks");
        assert_eq!(informal("First.\nSecond."), "First.\nSecond");
        assert_eq!(
            informal("I think I should go, and I'm sure I'll be fine."),
            "I think I should go, and I'm sure I'll be fine"
        );
        assert_eq!(
            informal("Yes, I\u{2019}ve seen it."),
            "Yes, I\u{2019}ve seen it"
        );
        assert_eq!(informal("So did I."), "So did I");
        assert_eq!(informal("So did I, and you?"), "So did I, and you?");
    }

    #[test]
    fn typed_abbreviations_are_lowercase_except_at_a_sentence_start() {
        assert_eq!(informal("BTW it works"), "Btw it works");
        assert_eq!(informal("OMG!"), "Omg!");
        assert_eq!(informal("LOL, it works"), "Lol, it works");
        assert_eq!(informal("Send it ASAP."), "Send it asap");
        assert_eq!(informal("G2G"), "G2g");
        assert_eq!(informal("It works IMO, really."), "It works imo, really");
    }

    #[test]
    fn names_mid_sentence_stay_capitalized() {
        assert_eq!(
            informal("Sent it to Raja. He agreed."),
            "Sent it to Raja. He agreed"
        );
        assert_eq!(informal("Meet Mr. Smith today."), "Meet Mr. Smith today");
        assert_eq!(informal("See e.g. Python docs."), "See e.g. Python docs");
        assert_eq!(
            informal("At 5 p.m. Monday works."),
            "At 5 p.m. Monday works"
        );
    }

    #[test]
    fn protected_first_words_keep_their_case() {
        for unchanged in [
            "API keys rotated",
            "OK, it works",
            "GitHub is down",
            "iPhone works",
            "U.S. news",
            "Cargo.toml needs a fix",
            "Https://example.com is down",
            "\"Hello\" she said",
            "1. Buy milk\n2. Call mom",
            "- Buy milk",
            "```\nHello\n```",
            "नमस्ते दोस्त",
        ] {
            assert_eq!(informal(unchanged), unchanged);
        }
        assert_eq!(
            informal("It's down. API keys rotated."),
            "It's down. API keys rotated"
        );
    }

    #[test]
    fn tone_parses_only_known_values() {
        assert_eq!(Tone::parse("formal"), Some(Tone::Formal));
        assert_eq!(Tone::parse("informal"), Some(Tone::Informal));
        assert_eq!(Tone::parse("Informal"), None);
        assert_eq!(Tone::parse("casual"), None);
        assert_eq!(Tone::default(), Tone::Formal);
        for tone in [Tone::Formal, Tone::Informal] {
            assert_eq!(Tone::parse(tone.as_str()), Some(tone));
        }
    }
}
