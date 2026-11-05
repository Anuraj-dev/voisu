//! Tone: an optional informal pass over the final rendered transcript.
//!
//! `Formal` is the default and returns the text unchanged, so formal output
//! stays byte-identical to the formatter's. `Informal` swaps a closed table of
//! spoken phrases for lowercase chat abbreviations, lowercases sentence starts
//! and the pronoun `I`, and drops the final full stop. It runs once on the text
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
    /// Chat abbreviations, lowercase sentence starts, no final full stop.
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
    /// Anywhere except right after one of these words.
    NotAfter(&'static [&'static str]),
    /// A discourse marker: clause-initial, or right after a conjunction or
    /// interjection ("you to be honest" and "tell by the way he talks" stay).
    ClauseStart,
    /// A clause of its own ("never mind the cost" and "it's no problem" stay).
    WholeClause,
    /// About the speaker: at clause start, where it also absorbs one of these
    /// subjects ("I'll be right back" is `brb`, "he'll be right back" stays).
    Subject(&'static [&'static [&'static str]]),
    /// The speaker leaving: a `Subject` that must also end its clause ("I got
    /// to go to the store" stays). `bare` allows the phrase with no subject.
    Leaving {
        bare: bool,
    },
}

const I_WILL: &[&[&str]] = &[&["i'll"], &["i", "will"], &["will"]];
const I_AM: &[&[&str]] = &[&["i'm"], &["i", "am"]];
const I_AM_OR_WAS: &[&[&str]] = &[&["i'm"], &["i", "am"], &["i", "was"]];
const I_LEAVING: &[&[&str]] = &[&["i"], &["i've"]];
const LET_ME_KNOW_BLOCKERS: &[&str] = &[
    "to", "he", "she", "they", "it", "not", "never", "didn't", "doesn't", "don't", "won't",
    "couldn't", "wouldn't",
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
    (&["not", "gonna", "lie"], "ngl", Position::ClauseStart),
    (&["not", "going", "to", "lie"], "ngl", Position::ClauseStart),
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
    (&["i", "don't", "know"], "idk", Position::ClauseStart),
    (&["never", "mind"], "nvm", Position::WholeClause),
    (&["no", "problem"], "np", Position::WholeClause),
    (
        &["let", "me", "know"],
        "lmk",
        Position::NotAfter(LET_ME_KNOW_BLOCKERS),
    ),
    (&["what", "the", "fuck"], "wtf", Position::Anywhere),
    (&["in", "real", "life"], "irl", Position::Anywhere),
    (&["away", "from", "keyboard"], "afk", Position::Anywhere),
    (&["on", "my", "way"], "omw", Position::Subject(I_AM)),
    (&["be", "right", "back"], "brb", Position::Subject(I_WILL)),
    (
        &["talk", "to", "you", "later"],
        "ttyl",
        Position::Subject(I_WILL),
    ),
    (&["shaking", "my", "head"], "smh", Position::Subject(I_AM)),
    (
        &["laughing", "out", "loud"],
        "lol",
        Position::Subject(I_AM_OR_WAS),
    ),
    (
        &["laughing", "my", "ass", "off"],
        "lmao",
        Position::Subject(I_AM_OR_WAS),
    ),
    (
        &["rolling", "on", "the", "floor", "laughing"],
        "rofl",
        Position::Subject(I_AM_OR_WAS),
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
const DISCOURSE_LEADS: [&str; 9] = [
    "and", "but", "so", "well", "oh", "hey", "ok", "okay", "sorry",
];

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
                lowercase_line(&abbreviate_line(line))
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
            let ends_clause = !suffix.is_empty() || last + 1 == self.tokens.len();
            let Some(subject) = self.position_allows(position, i, floor, ends_clause) else {
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

    /// The subject tokens to absorb if `position` allows a phrase at `i`.
    fn position_allows(
        &self,
        position: Position,
        i: usize,
        floor: usize,
        ends_clause: bool,
    ) -> Option<usize> {
        match position {
            Position::Anywhere => Some(0),
            Position::NotAfter(words) => {
                (i == 0 || !words.contains(&self.lower[i - 1].as_str())).then_some(0)
            }
            Position::ClauseStart => self.starts_clause(i).then_some(0),
            // "Never mind I found it" without a comma stays unchanged, so a
            // discourse lead does not count here.
            Position::WholeClause => {
                (ends_clause && (i == 0 || self.after_punctuation(i))).then_some(0)
            }
            Position::Subject(subjects) => self.subject_before(i, floor, subjects),
            Position::Leaving { bare } => {
                if !ends_clause {
                    return None;
                }
                self.subject_before(i, floor, I_LEAVING)
                    .filter(|&subject| bare || subject > 0)
            }
        }
    }

    /// How many tokens of an allowed subject sit before `i`, with the subject
    /// itself at clause start; `0` if the phrase needs none and is at clause
    /// start; `None` if neither holds.
    fn subject_before(&self, i: usize, floor: usize, subjects: &[&[&str]]) -> Option<usize> {
        for subject in subjects {
            let n = subject.len();
            if i < n || i - n < floor {
                continue;
            }
            let start = i - n;
            if self.skip[start..i].iter().all(|&s| !s)
                && subject
                    .iter()
                    .zip(&self.lower[start..i])
                    .all(|(w, t)| w == t)
                && self.starts_clause(start)
            {
                return Some(n);
            }
        }
        self.starts_clause(i).then_some(0)
    }

    fn starts_clause(&self, i: usize) -> bool {
        i == 0 || self.after_punctuation(i) || DISCOURSE_LEADS.contains(&self.lower[i - 1].as_str())
    }

    fn after_punctuation(&self, i: usize) -> bool {
        self.tokens[i - 1]
            .2
            .trim_end_matches(CLOSERS)
            .ends_with(TRAILING_PUNCT)
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

/// Lowercases what chat leaves lowercase: sentence starts, the pronoun `I`, and
/// abbreviations the transcript already spelled in capitals. Quotes, code and
/// names mid-sentence keep their case.
fn lowercase_line(line: &str) -> String {
    let tokens = word_tokens(line);
    let skip = protected_mask(&tokens);
    let mut out = String::with_capacity(line.len());
    let mut cursor = 0usize;
    for (i, &(start, end, token)) in tokens.iter().enumerate() {
        out.push_str(&line[cursor..start]);
        if skip[i] {
            out.push_str(token);
        } else {
            let starts_sentence = i == 0 || ends_sentence(tokens[i - 1].2, i - 1);
            out.push_str(&lowercase_token(token, starts_sentence));
        }
        cursor = end;
    }
    out.push_str(&line[cursor..]);
    out
}

fn lowercase_token(token: &str, starts_sentence: bool) -> String {
    let word = token.trim_end_matches(TRAILING_PUNCT);
    if PHRASES
        .iter()
        .any(|(_, abbreviation, _)| word.eq_ignore_ascii_case(abbreviation))
    {
        return token.to_ascii_lowercase();
    }
    if !is_first_person(word) && !(starts_sentence && is_plain_capitalized(word)) {
        return token.to_owned();
    }
    let mut chars = token.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    first.to_lowercase().chain(chars).collect()
}

/// `I`, `I'm`, `I'll`, `I've` or `I'd`, with a straight or curly apostrophe.
fn is_first_person(word: &str) -> bool {
    let Some(rest) = word.strip_prefix('I') else {
        return false;
    };
    rest.is_empty()
        || rest
            .strip_prefix(['\'', '\u{2019}'])
            .is_some_and(|tail| matches!(tail, "m" | "ll" | "ve" | "d"))
}

/// A word safe to lowercase at a sentence start. Another capital marks an
/// acronym or brand (`API`, `GitHub`), and these marks a file name or address
/// (`Cargo.toml`, `Https://example.com`).
fn is_plain_capitalized(word: &str) -> bool {
    let mut chars = word.chars();
    chars.next().is_some_and(char::is_uppercase)
        && !chars.any(char::is_uppercase)
        && !word.contains(['.', '/', '\\', '_', '@', ':', '=', '#'])
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
        ("By the way, it shipped.", "btw, it shipped"),
        ("Oh my god, it shipped.", "omg, it shipped"),
        ("Oh my gosh, it shipped.", "omg, it shipped"),
        ("For your information, it shipped.", "fyi, it shipped"),
        ("Send it as soon as possible.", "send it asap"),
        ("To be honest, it shipped.", "tbh, it shipped"),
        ("Not gonna lie, it shipped.", "ngl, it shipped"),
        ("Not going to lie, it shipped.", "ngl, it shipped"),
        ("In my opinion, it shipped.", "imo, it shipped"),
        ("In my humble opinion, it shipped.", "imho, it shipped"),
        ("As far as I know, it shipped.", "afaik, it shipped"),
        ("I don't know.", "idk"),
        ("Never mind.", "nvm"),
        ("No problem.", "np"),
        ("Let me know if it works.", "lmk if it works"),
        ("I'm on my way.", "omw"),
        ("What the fuck is this?", "wtf is this?"),
        ("I'll be right back.", "brb"),
        ("Talk to you later.", "ttyl"),
        ("We met in real life.", "we met irl"),
        ("I was away from keyboard.", "i was afk"),
        ("I am shaking my head.", "smh"),
        ("I was laughing out loud.", "lol"),
        ("I was laughing my ass off.", "lmao"),
        ("I was rolling on the floor laughing.", "rofl"),
        ("Sorry, I got to go.", "sorry, g2g"),
        ("I gotta go.", "g2g"),
        ("I have to go.", "g2g"),
        ("To be fair, it shipped.", "tbf, it shipped"),
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
        assert_eq!(informal("by the way, it shipped"), "btw, it shipped");
        assert_eq!(informal("BY THE WAY it shipped"), "btw it shipped");
        assert_eq!(informal("It shipped, by the way."), "it shipped, btw");
        assert_eq!(informal("Did it ship, by the way?"), "did it ship, btw?");
        assert_eq!(informal("Oh my god!"), "omg!");
        assert_eq!(informal("To be honest; it shipped"), "tbh; it shipped");
    }

    #[test]
    fn partial_phrases_and_lone_words_never_fire() {
        for (input, expected) in [
            ("By the river.", "by the river"),
            ("Is it by the", "is it by the"),
            ("The way it works.", "the way it works"),
            ("Be honest with me", "be honest with me"),
            ("As far as Paris", "as far as Paris"),
            ("We got to the station", "we got to the station"),
            ("A real life story", "a real life story"),
        ] {
            assert_eq!(informal(input), expected, "input: {input}");
        }
    }

    #[test]
    fn phrases_need_word_boundaries() {
        assert_eq!(informal("Standby the way"), "standby the way");
        assert_eq!(informal("by the wayside"), "by the wayside");
        // Punctuation inside the phrase breaks it.
        assert_eq!(informal("By the, way"), "by the, way");
    }

    #[test]
    fn discourse_markers_need_a_clause_position() {
        assert_eq!(
            informal("I want you to be honest."),
            "i want you to be honest"
        );
        assert_eq!(
            informal("It's important to be fair to everyone."),
            "it's important to be fair to everyone"
        );
        assert_eq!(
            informal("You can tell by the way he talks."),
            "you can tell by the way he talks"
        );
        assert_eq!(
            informal("Thanks for your information."),
            "thanks for your information"
        );
        assert_eq!(informal("By the way, it works."), "btw, it works");
        assert_eq!(informal("And by the way it works."), "and btw it works");
        assert_eq!(informal("It was, to be honest, fine."), "it was, tbh, fine");
    }

    #[test]
    fn i_dont_know_needs_a_clause_position() {
        assert_eq!(informal("I don't know what to do."), "idk what to do");
        assert_eq!(informal("I don\u{2019}t know."), "idk");
        assert_eq!(informal("Honestly, I don't know."), "honestly, idk");
        assert_eq!(informal("But I don't know."), "but idk");
        assert_eq!(
            informal("That is what I don't know."),
            "that is what i don't know"
        );
    }

    #[test]
    fn fenced_code_is_unchanged_and_abbreviations_keep_their_stop() {
        assert_eq!(informal("```\nBy the way\n```"), "```\nBy the way\n```");
        assert_eq!(
            informal("Bring apples, pears, etc."),
            "bring apples, pears, etc."
        );
        assert_eq!(informal("Say hi to Mr."), "say hi to Mr.");
    }

    #[test]
    fn a_subject_before_the_phrase_is_absorbed_only_at_clause_start() {
        assert_eq!(informal("Sorry, I'll be right back."), "sorry, brb");
        assert_eq!(informal("I will be right back."), "brb");
        assert_eq!(informal("Be right back."), "brb");
        assert_eq!(informal("I'll talk to you later."), "ttyl");
        assert_eq!(informal("I'm on my way to work."), "omw to work");
        assert_eq!(informal("I\u{2019}m on my way."), "omw");
        assert_eq!(informal("On my way."), "omw");
        assert_eq!(informal("I'm shaking my head."), "smh");
        assert_eq!(informal("I'm laughing my ass off."), "lmao");
        assert_eq!(informal("And I'm on my way."), "and omw");
    }

    #[test]
    fn phrases_about_someone_else_stay_as_words() {
        assert_eq!(informal("He'll be right back."), "he'll be right back");
        assert_eq!(
            informal("I want to talk to you later."),
            "i want to talk to you later"
        );
        assert_eq!(informal("It's on my way."), "it's on my way");
        assert_eq!(
            informal("I'll stop by on my way home."),
            "i'll stop by on my way home"
        );
        assert_eq!(
            informal("He was laughing out loud."),
            "he was laughing out loud"
        );
        assert_eq!(
            informal("So I said what I'm on my way."),
            "so i said what i'm on my way"
        );
    }

    #[test]
    fn let_me_know_stays_when_it_is_about_someone_else() {
        assert_eq!(informal("Please let me know."), "please lmk");
        assert_eq!(informal("Can you let me know?"), "can you lmk?");
        assert_eq!(
            informal("He forgot to let me know."),
            "he forgot to let me know"
        );
        assert_eq!(
            informal("She didn't let me know."),
            "she didn't let me know"
        );
    }

    #[test]
    fn no_problem_fires_only_when_it_is_a_whole_clause() {
        assert_eq!(informal("Thanks, no problem."), "thanks, np");
        assert_eq!(informal("It's no problem."), "it's no problem");
        assert_eq!(informal("No problem with that."), "no problem with that");
    }

    #[test]
    fn leaving_phrases_fire_only_when_they_end_a_clause_for_the_speaker() {
        assert_eq!(
            informal("I got to go to the store."),
            "i got to go to the store"
        );
        assert_eq!(
            informal("I have to go to the store."),
            "i have to go to the store"
        );
        assert_eq!(
            informal("We got to go home, then slept."),
            "we got to go home, then slept"
        );
        assert_eq!(
            informal("We got to go, and it was great."),
            "we got to go, and it was great"
        );
        assert_eq!(informal("I got to go, bye."), "g2g, bye");
        assert_eq!(informal("Sorry I got to go."), "sorry g2g");
        assert_eq!(informal("I've got to go."), "g2g");
        assert_eq!(informal("Gotta go."), "g2g");
        assert_eq!(informal("Got to go."), "g2g");
        assert_eq!(informal("I never got to go."), "i never got to go");
        assert_eq!(informal("You have to go."), "you have to go");
        assert_eq!(informal("You got to go."), "you got to go");
        assert_eq!(informal("Do I have to go?"), "do i have to go?");
    }

    #[test]
    fn never_mind_fires_only_when_it_is_a_whole_clause() {
        assert_eq!(informal("Never mind the cost."), "never mind the cost");
        assert_eq!(informal("Never mind that"), "never mind that");
        assert_eq!(informal("I never mind waiting."), "i never mind waiting");
        assert_eq!(informal("I never mind."), "i never mind");
        assert_eq!(informal("Never mind"), "nvm");
        assert_eq!(informal("never mind."), "nvm");
        assert_eq!(informal("Never mind, I found it."), "nvm, i found it");
        assert_eq!(informal("Oh, never mind."), "oh, nvm");
        assert_eq!(informal("It broke. Never mind!"), "it broke. nvm!");
        assert_eq!(
            informal("Wait.\nNever mind, it works."),
            "wait.\nnvm, it works"
        );
    }

    #[test]
    fn quoted_spans_are_unchanged() {
        assert_eq!(
            informal(r#"She wrote "By the way, I agree." twice."#),
            r#"she wrote "By the way, I agree." twice"#
        );
        assert_eq!(
            informal("She wrote \u{201c}To be honest\u{201d} twice."),
            "she wrote \u{201c}To be honest\u{201d} twice"
        );
        assert_eq!(
            informal(r#"He said "never mind." By the way, hi."#),
            r#"he said "never mind." btw, hi"#
        );
        assert_eq!(
            informal("Type `I think. So` here."),
            "type `I think. So` here"
        );
        assert_eq!(
            informal("Say quote I think unquote now"),
            "say quote I think unquote now"
        );
    }

    #[test]
    fn command_lines_are_unchanged() {
        let command = "git commit -m by the way.";
        assert_eq!(informal(command), command);
        assert_eq!(informal("Git commit -m hello"), "Git commit -m hello");
        assert_eq!(
            informal("By the way, run this.\ncargo test --by the way"),
            "btw, run this.\ncargo test --by the way"
        );
        assert_eq!(
            informal("Hello\nGit commit -m hi"),
            "hello\nGit commit -m hi"
        );
    }

    #[test]
    fn only_the_final_full_stop_is_dropped() {
        assert_eq!(informal("It shipped. It works."), "it shipped. it works");
        assert_eq!(
            informal("It shipped. Does it work?"),
            "it shipped. does it work?"
        );
        assert_eq!(informal("It shipped. It works!"), "it shipped. it works!");
        assert_eq!(informal("It shipped. It works"), "it shipped. it works");
        assert_eq!(informal("First.\n\nSecond.\n"), "first.\n\nsecond\n");
        assert_eq!(informal("IMO we shouldn't wait."), "imo we shouldn't wait");
        assert_eq!(informal("I said no."), "i said no");
        assert_eq!(informal(""), "");
        assert_eq!(informal("."), ".");
    }

    #[test]
    fn final_full_stop_on_abbreviations_ellipses_and_quotes_stays() {
        assert_eq!(informal("Meet at 5 p.m."), "meet at 5 p.m.");
        assert_eq!(informal("Bring fruit, e.g."), "bring fruit, e.g.");
        assert_eq!(informal("Open config.toml."), "open config.toml.");
        assert_eq!(informal("Well..."), "well...");
        assert_eq!(informal(r#"He said "hi.""#), r#"he said "hi.""#);
    }

    #[test]
    fn sentence_starts_and_every_i_are_lowercase() {
        assert_eq!(informal("Hello there."), "hello there");
        assert_eq!(informal("A quick one."), "a quick one");
        assert_eq!(informal("Élan matters."), "élan matters");
        assert_eq!(informal("  Hello there."), "  hello there");
        assert_eq!(informal("Hi. How are you?"), "hi. how are you?");
        assert_eq!(informal("Hi! How are you?"), "hi! how are you?");
        assert_eq!(informal("Why? Because."), "why? because");
        assert_eq!(informal("Well... Then what?"), "well... then what?");
        assert_eq!(informal("No. Thanks."), "no. thanks");
        assert_eq!(informal("First.\nSecond."), "first.\nsecond");
        assert_eq!(
            informal("I think I should go, and I'm sure I'll be fine."),
            "i think i should go, and i'm sure i'll be fine"
        );
        assert_eq!(
            informal("Yes, I\u{2019}ve seen it."),
            "yes, i\u{2019}ve seen it"
        );
        assert_eq!(informal("So did I."), "so did i");
        assert_eq!(informal("So did I, and you?"), "so did i, and you?");
    }

    #[test]
    fn typed_abbreviations_from_the_transcript_go_lowercase() {
        assert_eq!(informal("BTW it works"), "btw it works");
        assert_eq!(informal("OMG!"), "omg!");
        assert_eq!(informal("LOL, it works"), "lol, it works");
        assert_eq!(informal("Send it ASAP."), "send it asap");
        assert_eq!(informal("G2G"), "g2g");
        assert_eq!(informal("It works IMO, really."), "it works imo, really");
    }

    #[test]
    fn names_mid_sentence_stay_capitalized() {
        assert_eq!(
            informal("Sent it to Raja. He agreed."),
            "sent it to Raja. he agreed"
        );
        assert_eq!(informal("Meet Mr. Smith today."), "meet Mr. Smith today");
        assert_eq!(informal("See e.g. Python docs."), "see e.g. Python docs");
        assert_eq!(
            informal("At 5 p.m. Monday works."),
            "at 5 p.m. Monday works"
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
            "it's down. API keys rotated"
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
