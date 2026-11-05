//! Tone: an optional informal pass over the final rendered transcript.
//!
//! `Formal` is the default and returns the text unchanged, so formal output
//! stays byte-identical to the formatter's. `Informal` swaps a closed table of
//! spoken phrases for their uppercase chat abbreviations and drops the final
//! full stop. It runs once on the text about to be delivered, so cloud and
//! local ASR, and clean and exact Writing Modes, all pass through the same
//! stage.

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
    /// Tier 1 abbreviations and no final full stop.
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
        Tone::Informal => drop_final_full_stop(&abbreviate(text)),
    }
}

/// Tier 1 phrases (lowercase words) and their abbreviations. Closed: add a
/// phrase only through the spec. Matching prefers the longest phrase.
const TIER_ONE: &[(&[&str], &str)] = &[
    (&["rolling", "on", "the", "floor", "laughing"], "ROFL"),
    (&["for", "your", "information"], "FYI"),
    (&["as", "soon", "as", "possible"], "ASAP"),
    (&["as", "far", "as", "i", "know"], "AFAIK"),
    (&["laughing", "my", "ass", "off"], "LMAO"),
    (&["by", "the", "way"], "BTW"),
    (&["oh", "my", "god"], "OMG"),
    (&["to", "be", "honest"], "TBH"),
    (&["not", "gonna", "lie"], "NGL"),
    (&["in", "my", "opinion"], "IMO"),
    (&["never", "mind"], "NVM"),
    (&["be", "right", "back"], "BRB"),
    (&["talk", "to", "you", "later"], "TTYL"),
    (&["in", "real", "life"], "IRL"),
    (&["away", "from", "keyboard"], "AFK"),
    (&["shaking", "my", "head"], "SMH"),
    (&["got", "to", "go"], "G2G"),
    (&["to", "be", "fair"], "TBF"),
];

/// Abbreviations that only fire as a clause-initial discourse marker.
const CLAUSE_MARKERS: [&str; 6] = ["BTW", "FYI", "TBH", "TBF", "NGL", "IMO"];

/// Words that may precede a discourse marker without clause punctuation.
const DISCOURSE_LEADS: [&str; 8] = ["and", "but", "so", "well", "oh", "hey", "ok", "okay"];

/// Words before "got to go" that make it past-tense "had the chance".
const G2G_PAST_MARKERS: [&str; 5] = ["never", "ever", "finally", "not", "just"];

/// Punctuation that may trail a phrase's last word. It is ignored for
/// matching and kept after the abbreviation.
const TRAILING_PUNCT: [char; 6] = [',', '.', '!', '?', ';', ':'];

fn abbreviate(text: &str) -> String {
    // Lines inside a ``` fence are code and stay untouched.
    let mut in_fence = false;
    text.split('\n')
        .map(|line| {
            if line.trim_start().starts_with("```") {
                in_fence = !in_fence;
                line.to_owned()
            } else if in_fence {
                line.to_owned()
            } else {
                abbreviate_line(line)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn abbreviate_line(line: &str) -> String {
    if is_command_shaped(line) {
        return line.to_owned();
    }
    let tokens = word_tokens(line);
    let skip = protected_mask(&tokens);
    let lower: Vec<String> = tokens.iter().map(|(_, _, tok)| ascii_lower(tok)).collect();

    let mut out = String::with_capacity(line.len());
    let mut cursor = 0usize;
    let mut i = 0usize;
    while i < tokens.len() {
        let Some((len, abbreviation, suffix)) = match_at(&tokens, &lower, &skip, i) else {
            i += 1;
            continue;
        };
        let last = i + len - 1;
        out.push_str(&line[cursor..tokens[i].0]);
        out.push_str(abbreviation);
        out.push_str(suffix);
        cursor = tokens[last].1;
        i = last + 1;
    }
    out.push_str(&line[cursor..]);
    out
}

/// Longest Tier 1 phrase starting at token `i`: its token count, its
/// abbreviation, and the punctuation that trailed its last word.
fn match_at<'a>(
    tokens: &[(usize, usize, &'a str)],
    lower: &[String],
    skip: &[bool],
    i: usize,
) -> Option<(usize, &'static str, &'a str)> {
    let mut best: Option<(usize, &'static str, &'a str)> = None;
    for (words, abbreviation) in TIER_ONE {
        let n = words.len();
        if i + n > tokens.len() || best.is_some_and(|(len, _, _)| len >= n) {
            continue;
        }
        if skip[i..i + n].iter().any(|&s| s) {
            continue;
        }
        let last = i + n - 1;
        if words[..n - 1]
            .iter()
            .zip(&lower[i..last])
            .any(|(word, tok)| word != tok)
        {
            continue;
        }
        let last_lower = lower[last].trim_end_matches(TRAILING_PUNCT);
        if last_lower != words[n - 1] {
            continue;
        }
        let raw_last = tokens[last].2;
        let suffix = &raw_last[raw_last.trim_end_matches(TRAILING_PUNCT).len()..];
        if *abbreviation == "NVM" && !never_mind_is_whole_clause(tokens, i, last, suffix) {
            continue;
        }
        // "got to go" is also ordinary English ("I got to go to the store").
        // It becomes G2G only when it ends its clause and is not "never got
        // to go" / "finally got to go".
        if *abbreviation == "G2G"
            && ((suffix.is_empty() && last + 1 != tokens.len())
                || (i > 0 && G2G_PAST_MARKERS.contains(&lower[i - 1].as_str())))
        {
            continue;
        }
        // These double as ordinary phrases ("you to be honest", "tell by the
        // way he talks"). They become abbreviations only as a discourse
        // marker: clause-initial, or right after a conjunction/interjection.
        if CLAUSE_MARKERS.contains(abbreviation)
            && !(i == 0
                || tokens[i - 1]
                    .2
                    .trim_end_matches(['"', '\u{201d}', ')'])
                    .ends_with(TRAILING_PUNCT)
                || DISCOURSE_LEADS.contains(&lower[i - 1].as_str()))
        {
            continue;
        }
        best = Some((n, abbreviation, suffix));
    }
    best
}

/// `never mind` doubles as a verb phrase ("never mind the cost", "I never
/// mind waiting"). It becomes NVM only as a clause of its own: it starts the
/// line or follows clause punctuation, and it ends the line or carries
/// punctuation. "Never mind I found it" without a comma stays unchanged.
fn never_mind_is_whole_clause(
    tokens: &[(usize, usize, &str)],
    first: usize,
    last: usize,
    suffix: &str,
) -> bool {
    let starts_clause = first == 0 || tokens[first - 1].2.ends_with(TRAILING_PUNCT);
    let ends_clause = !suffix.is_empty() || last + 1 == tokens.len();
    starts_clause && ends_clause
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
    const ABBREVIATION_STEMS: [&str; 14] = [
        "etc", "mr", "mrs", "ms", "dr", "vs", "inc", "jr", "sr", "st", "ltd", "co", "prof", "no",
    ];
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

    /// Every Tier 1 phrase, as a formatted sentence and the expected output.
    const TIER_ONE: &[(&str, &str)] = &[
        ("By the way, it shipped.", "BTW, it shipped"),
        ("Oh my god, it shipped.", "OMG, it shipped"),
        ("For your information, it shipped.", "FYI, it shipped"),
        ("Send it as soon as possible.", "Send it ASAP"),
        ("To be honest, it shipped.", "TBH, it shipped"),
        ("Not gonna lie, it shipped.", "NGL, it shipped"),
        ("In my opinion, it shipped.", "IMO, it shipped"),
        ("As far as I know, it shipped.", "AFAIK, it shipped"),
        ("Never mind.", "NVM"),
        ("I'll be right back.", "I'll BRB"),
        ("Talk to you later.", "TTYL"),
        ("We met in real life.", "We met IRL"),
        ("I was away from keyboard.", "I was AFK"),
        ("I am shaking my head.", "I am SMH"),
        ("I was laughing my ass off.", "I was LMAO"),
        ("I was rolling on the floor laughing.", "I was ROFL"),
        ("Sorry, I got to go.", "Sorry, I G2G"),
        ("To be fair, it shipped.", "TBF, it shipped"),
    ];

    #[test]
    fn formal_is_byte_identical_for_the_tier_one_corpus() {
        for (input, _) in TIER_ONE {
            assert_eq!(apply_tone(input, Tone::Formal), *input);
        }
        let messy = "  by the way , never mind the cost.\n\nTalk to you later!  ";
        assert_eq!(apply_tone(messy, Tone::Formal), messy);
    }

    #[test]
    fn informal_converts_every_tier_one_phrase() {
        for (input, expected) in TIER_ONE {
            assert_eq!(informal(input), *expected, "input: {input}");
        }
    }

    #[test]
    fn matching_ignores_case_and_keeps_trailing_punctuation() {
        assert_eq!(informal("by the way, it shipped"), "BTW, it shipped");
        assert_eq!(informal("BY THE WAY it shipped"), "BTW it shipped");
        assert_eq!(informal("It shipped, by the way."), "It shipped, BTW");
        assert_eq!(informal("Did it ship, by the way?"), "Did it ship, BTW?");
        assert_eq!(informal("Oh my god!"), "OMG!");
        assert_eq!(informal("To be honest; it shipped"), "TBH; it shipped");
    }

    #[test]
    fn partial_phrases_and_lone_words_never_fire() {
        for input in [
            "By the river.",
            "Is it by the",
            "The way it works.",
            "Be honest with me",
            "As far as Paris",
            "We got to the station",
            "A real life story",
        ] {
            let expected = input.strip_suffix('.').unwrap_or(input);
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
        assert_eq!(informal("By the way, it works."), "BTW, it works");
        assert_eq!(informal("And by the way it works."), "And BTW it works");
        assert_eq!(informal("It was, to be honest, fine."), "It was, TBH, fine");
        assert_eq!(informal("I never got to go."), "I never got to go");
    }

    #[test]
    fn fenced_code_is_unchanged_and_abbreviations_keep_their_stop() {
        assert_eq!(informal("```\nby the way\n```"), "```\nby the way\n```");
        assert_eq!(
            informal("Bring apples, pears, etc."),
            "Bring apples, pears, etc."
        );
        assert_eq!(informal("Say hi to Mr."), "Say hi to Mr.");
    }

    #[test]
    fn got_to_go_fires_only_when_it_ends_its_clause() {
        assert_eq!(
            informal("I got to go to the store."),
            "I got to go to the store"
        );
        assert_eq!(
            informal("We got to go home, then slept."),
            "We got to go home, then slept"
        );
        assert_eq!(informal("Sorry, I got to go."), "Sorry, I G2G");
        assert_eq!(informal("I got to go, bye."), "I G2G, bye");
    }

    #[test]
    fn never_mind_fires_only_when_it_is_a_whole_clause() {
        assert_eq!(informal("Never mind the cost."), "Never mind the cost");
        assert_eq!(informal("Never mind that"), "Never mind that");
        assert_eq!(informal("I never mind waiting."), "I never mind waiting");
        assert_eq!(informal("I never mind."), "I never mind");
        assert_eq!(informal("Never mind"), "NVM");
        assert_eq!(informal("never mind."), "NVM");
        assert_eq!(informal("Never mind, I found it."), "NVM, I found it");
        assert_eq!(informal("Oh, never mind."), "Oh, NVM");
        assert_eq!(informal("It broke. Never mind!"), "It broke. NVM!");
        assert_eq!(
            informal("Wait.\nNever mind, it works."),
            "Wait.\nNVM, it works"
        );
    }

    #[test]
    fn quoted_spans_are_unchanged() {
        assert_eq!(
            informal(r#"She wrote "by the way" twice."#),
            r#"She wrote "by the way" twice"#
        );
        assert_eq!(
            informal("She wrote \u{201c}to be honest\u{201d} twice."),
            "She wrote \u{201c}to be honest\u{201d} twice"
        );
        assert_eq!(
            informal(r#"He said "never mind." By the way, hi."#),
            r#"He said "never mind." BTW, hi"#
        );
        assert_eq!(
            informal("Type `by the way` here."),
            "Type `by the way` here"
        );
        assert_eq!(
            informal("Say quote by the way unquote now"),
            "Say quote by the way unquote now"
        );
    }

    #[test]
    fn command_lines_are_unchanged() {
        let command = "git commit -m by the way.";
        assert_eq!(informal(command), command);
        assert_eq!(
            informal("By the way, run this.\ncargo test --by the way"),
            "BTW, run this.\ncargo test --by the way"
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
