use super::*;

fn terms(list: &[&str]) -> Vec<String> {
    list.iter().map(|term| (*term).to_owned()).collect()
}

/// A realistic personal Dictionary: product, desktop, provider, tool and
/// person names of the kind Voisu users add.
fn realistic() -> Vec<String> {
    terms(&["Voisu", "Hyprland", "Deepgram", "Kubernetes", "Anuraj"])
}

#[test]
fn a_split_hearing_of_a_name_is_replaced_by_the_entry() {
    assert_eq!(
        apply_phonetic_dictionary("I use voice you every day", &terms(&["Voisu"])),
        "I use Voisu every day"
    );
}

#[test]
fn multi_word_runs_are_replaced() {
    assert_eq!(
        apply_phonetic_dictionary("switch to hyper land now", &realistic()),
        "switch to Hyprland now"
    );
    assert_eq!(
        apply_phonetic_dictionary("deploy it on cuber net ease today", &realistic()),
        "deploy it on Kubernetes today"
    );
}

#[test]
fn a_multi_word_entry_matches_its_run() {
    assert_eq!(
        apply_phonetic_dictionary("ask clawed code to fix it", &terms(&["Claude Code"])),
        "ask Claude Code to fix it"
    );
}

#[test]
fn one_syllable_entries_are_never_used() {
    for (text, entry) in [
        ("grok said hello", "Groq"),
        ("the claud is here", "Claude"),
        ("rest in peace", "Rust"),
    ] {
        assert_eq!(apply_phonetic_dictionary(text, &terms(&[entry])), text);
    }
}

#[test]
fn runs_never_cross_sentence_or_clause_punctuation() {
    for text in [
        "Raise your voice. You can do it.",
        "Was that your voice? You sounded tired.",
        "What a voice! You should sing.",
        "Hyper, land the plane.",
    ] {
        assert_eq!(
            apply_phonetic_dictionary(text, &realistic()),
            text,
            "{text}"
        );
    }
}

#[test]
fn a_span_that_already_spells_an_entry_is_left_alone() {
    // Exact-match correction runs first in the Local tail; the phonetic pass
    // must never re-target what already spells a Dictionary entry.
    let dictionary = terms(&["Voisu", "voice you"]);
    assert_eq!(
        apply_phonetic_dictionary("say voice you twice", &dictionary),
        "say voice you twice"
    );
    assert_eq!(
        apply_phonetic_dictionary("Voisu is great", &dictionary),
        "Voisu is great"
    );
}

#[test]
fn surrounding_capitals_and_punctuation_survive() {
    assert_eq!(
        apply_phonetic_dictionary("Voice you, please start.", &realistic()),
        "Voisu, please start."
    );
    assert_eq!(
        apply_phonetic_dictionary("(hyper land)!", &realistic()),
        "(Hyprland)!"
    );
    // A lowercase entry keeps a sentence-start capital.
    assert_eq!(
        apply_phonetic_dictionary("Cube control is down.", &terms(&["kubecontrol"])),
        "Kubecontrol is down."
    );
}

#[test]
fn the_edit_distance_bound_rejects_codes_that_only_collide() {
    // Each hearing shares the entry's Double Metaphone primary code but is
    // too far from the entry's sound shape to swap.
    let cases = [
        ("face", "Voisu"),
        ("fuss", "Voisu"),
        ("fees", "Voisu"),
        ("phase you", "Voisu"),
        ("cooper nets", "Kubernetes"),
        ("caper notice", "Kubernetes"),
    ];
    let encoder = DoubleMetaphone::new(None);
    for (heard, entry) in cases {
        assert_eq!(
            encoder.encode(&letters_only(heard)),
            encoder.encode(&letters_only(entry)),
            "precondition: {heard} shares {entry}'s primary code"
        );
    }
    for (heard, entry) in cases {
        let text = format!("we saw {heard} today");
        assert_eq!(
            apply_phonetic_dictionary(&text, &terms(&[entry])),
            text,
            "{heard} -> {entry}"
        );
    }
}

#[test]
fn common_english_words_are_never_swapped_against_a_realistic_dictionary() {
    let dictionary = realistic();
    let common = "the be to of and a in that have I it for not on with he as you do at \
        this but his by from they we say her she or an will my one all would there their \
        what so up out if about who get which go me when make can like time no just him \
        know take people into year your good some could them see other than then now look \
        only come its over think also back after use two how our work first well way even \
        new want because any these give day most us voice voices face phase vice vase fuss \
        focus deep program happy realm hyper land cover governess cabinets current anger \
        energy injury honor enrage and you new rage hyperlink pipe wire tail scale grams \
        diagram deacon digger dagger hippie helper harland holland highland heartland \
        kerberos cubans cabernet copernicus vista visa voicemail voiceover invoice noise";
    for word in common.split_whitespace() {
        assert_eq!(
            apply_phonetic_dictionary(word, &dictionary),
            word,
            "{word} must stay"
        );
    }
    let sentence = "Raise your voice and face the deep program; a happy hyperlink can help \
        the highland governess cover current energy.";
    assert_eq!(apply_phonetic_dictionary(sentence, &dictionary), sentence);
}

#[test]
fn urls_paths_emails_and_identifiers_are_never_rewritten() {
    for text in [
        "open voice.you/docs",
        "mail voice@you.dev",
        "see /opt/voice you",
        "call voice_you now",
    ] {
        assert_eq!(
            apply_phonetic_dictionary(text, &realistic()),
            text,
            "{text}"
        );
    }
}

#[test]
fn an_ambiguous_hearing_is_left_alone() {
    let dictionary = terms(&["Hyprland", "Hiperland"]);
    let text = "hyper land";
    assert_eq!(apply_phonetic_dictionary(text, &dictionary), text);
}

#[test]
fn spoken_quotes_are_never_rewritten() {
    let text = "he said quote voice you unquote";
    assert_eq!(apply_phonetic_dictionary(text, &realistic()), text);
}

#[test]
fn swaps_report_what_was_heard_and_are_idempotent() {
    let dictionary = realistic();
    let text = "voice you runs on hyper land";
    let swaps = phonetic_dictionary_swaps(text, &dictionary);
    let heard: Vec<(&str, &str)> = swaps
        .iter()
        .map(|swap| (swap.heard.as_str(), swap.replacement.as_str()))
        .collect();
    assert_eq!(heard, [("voice you", "Voisu"), ("hyper land", "Hyprland")]);
    let once = apply_phonetic_dictionary(text, &dictionary);
    assert_eq!(once, "Voisu runs on Hyprland");
    assert_eq!(apply_phonetic_dictionary(&once, &dictionary), once);
}

#[test]
fn an_empty_dictionary_is_byte_identical() {
    let text = "voice you, hyper land.";
    assert_eq!(apply_phonetic_dictionary(text, &[]), text);
}

#[test]
fn the_common_word_list_is_sorted_and_unique() {
    let words: Vec<&str> = COMMON_WORDS.split_ascii_whitespace().collect();
    assert!(words.windows(2).all(|pair| pair[0] < pair[1]));
}
