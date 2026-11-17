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
        // A real split hearing, refused because both words are English and
        // the shapes differ by a vowel (see the bound in `best_match`).
        ("cooper nets", "Kubernetes"),
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
    // Refused for the same reason: allowing a vowel change between English
    // words is what turned "meet you" into `Matteo`.
    let text = "ask jonah than about it";
    assert_eq!(apply_phonetic_dictionary(text, &terms(&["Jonathan"])), text);
}

/// Tech entries of the kind Voisu users add, including two that are also
/// ordinary words when split ("in box", "log in").
const TECH: &[&str] = &[
    "Voisu",
    "Hyprland",
    "Deepgram",
    "Kubernetes",
    "Tokio",
    "Wayland",
    "Mistral",
    "Redis",
    "Kotlin",
    "Neovim",
    "Postgres",
    "Anthropic",
    "Docker",
    "Ubuntu",
    "Fedora",
    "Parakeet",
    "Pipewire",
    "Systemd",
    "Grafana",
    "Terraform",
    "Ansible",
    "Nginx",
    "Kafka",
    "Elixir",
    "Erlang",
    "Haskell",
    "Python",
    "Golang",
    "Rustacean",
    "TypeScript",
    "JavaScript",
    "Webpack",
    "Svelte",
    "Tailwind",
    "Vercel",
    "Netlify",
    "Supabase",
    "Firebase",
    "Mongo",
    "MySQL",
    "SQLite",
    "Cassandra",
    "Prometheus",
    "Jenkins",
    "GitHub",
    "GitLab",
    "Bitbucket",
    "Jira",
    "Confluence",
    "Figma",
    "Notion",
    "Obsidian",
    "Raycast",
    "Alacritty",
    "Kitty",
    "Wezterm",
    "Zellij",
    "Tmux",
    "Ollama",
    "Llama",
    "Gemini",
    "Claude Code",
    "OpenAI",
    "Whisper",
    "Qwen",
    "Groq",
    "Mixtral",
    "Pytorch",
    "TensorFlow",
    "Jupyter",
    "Numpy",
    "Pandas",
    "Matplotlib",
    "Fastapi",
    "Django",
    "Flask",
    "Rails",
    "Laravel",
    "Symfony",
    "Kubectl",
    "Helm",
    "Istio",
    "Envoy",
    "Traefik",
    "Caddy",
    "Cloudflare",
    "Tailscale",
    "Wireguard",
    "OpenSSH",
    "Gnome",
    "Plasma",
    "Omarchy",
    "Arch Linux",
    "Pacman",
    "Flatpak",
    "Snapcraft",
    "Inbox",
    "Login",
];

/// Common first names across several languages, led by the ones everyday
/// phrases once turned into ("nice to meet you" became `Matteo`).
const NAMES: &[&str] = &[
    "Matteo",
    "Harry",
    "Nora",
    "Lucia",
    "Anna",
    "Oliver",
    "Sofia",
    "Hudson",
    "Ethan",
    "Priyanka",
    "Anuraj",
    "Jonathan",
    "Rebecca",
    "Sarah",
    "Aaron",
    "Daniel",
    "Maria",
    "Olivia",
    "Ellen",
    "Emma",
    "Ava",
    "Isabella",
    "Mia",
    "Amelia",
    "Harper",
    "Evelyn",
    "Abigail",
    "Emily",
    "Ella",
    "Elizabeth",
    "Camila",
    "Luna",
    "Mila",
    "Aria",
    "Scarlett",
    "Penelope",
    "Layla",
    "Chloe",
    "Victoria",
    "Madison",
    "Eleanor",
    "Grace",
    "Hazel",
    "Violet",
    "Aurora",
    "Savannah",
    "Audrey",
    "Brooklyn",
    "Bella",
    "Claire",
    "Skylar",
    "Lucy",
    "Paisley",
    "Everly",
    "Liam",
    "Noah",
    "William",
    "Elijah",
    "Lucas",
    "Mason",
    "Logan",
    "Alexander",
    "Jacob",
    "Michael",
    "Sebastian",
    "Mateo",
    "Jackson",
    "Levi",
    "Henry",
    "Owen",
    "Wyatt",
    "Dylan",
    "Luke",
    "Gabriel",
    "Anthony",
    "Isaac",
    "Grayson",
    "Julian",
    "Christopher",
    "Joshua",
    "Andrew",
    "Lincoln",
    "Ezra",
    "Hunter",
    "Connor",
    "Jeremiah",
    "Easton",
    "Kai",
    "Arjun",
    "Rohan",
    "Ananya",
    "Deepika",
    "Vikram",
    "Rahul",
    "Aditi",
    "Kavya",
    "Ishaan",
    "Neha",
    "Pooja",
    "Sanjay",
    "Tanvi",
    "Yusuf",
];

/// A full 200-entry Dictionary: every tech entry, then names up to the limit.
fn names_and_tech() -> Vec<String> {
    let dictionary: Vec<String> = TECH
        .iter()
        .chain(NAMES)
        .take(USER_VOCABULARY_TERM_LIMIT)
        .map(|term| (*term).to_owned())
        .collect();
    assert_eq!(dictionary.len(), USER_VOCABULARY_TERM_LIMIT);
    dictionary
}

/// Everyday dictation that once swapped to a name or a tool ("I hear you"
/// became `Harry`, "will end" became `Wayland`), plus more of the same kind.
const EVERYDAY_PHRASES: &[&str] = &[
    "It was nice to meet you",
    "I hear you",
    "Here you go",
    "Is there a pharmacy near you?",
    "I met you last year",
    "I don't want to lose you",
    "Which one you want",
    "There was paint all over the floor",
    "so if you delete it",
    "I'll take you to the station",
    "He took you there",
    "The meeting will end at five",
    "I had son and daughter",
    "We went out then came back",
    "the most royal family",
    "road days are long",
    "Thank you so much",
    "See you later",
    "Can you hear me",
    "I miss you",
    "Did you get my message",
    "Are you coming tonight",
    "Let me know when you arrive",
    "I'll call you back",
    "Nice to see you again",
    "What do you think",
    "Do you want some coffee",
    "Could you send me the file",
    "I love you",
    "I told you so",
    "Before you go",
    "After you finish",
    "Bless you",
    "Good to hear from you",
    "I'll text you tomorrow",
    "Will you marry me",
    "Here we go again",
    "Give you a call",
    "Got you covered",
    "Mind you",
    "Show you around",
    "Catch you later",
    "Hold on a second",
    "Let me check my notes",
    "Please review the pull request",
    "The rain fell all over the town",
    "Pick you up at noon",
    "Wait for you outside",
    "Lend you a hand",
    "Owe you one",
    "Remind you later",
    "Ask you something",
    "Tell you what",
    "Teach you how",
    "Join you there",
    "Follow you home",
    "Meet you there",
    "Trust you completely",
    "Pay you back",
    "Beat you to it",
    "Need you here",
    "Visit you soon",
    "Hear you loud and clear",
    "Near you now",
    "Keep you posted",
    "Leave you alone",
    "Help you out",
    "Make you happy",
    "Write you a note",
    "Read you the list",
    "Forgive you",
    "Believe you",
    "the road we took was way longer",
    "we walked east one more block",
    "check your in box first",
    "please log in again",
    "the LUKS header is fine",
    "set the isk value",
    "Raise your voice and face the deep program",
    "a happy hyperlink can help the highland governess cover current energy",
];

#[test]
fn everyday_phrases_are_never_swapped_against_a_full_dictionary() {
    let dictionary = names_and_tech();
    for &phrase in EVERYDAY_PHRASES {
        assert_eq!(
            apply_phonetic_dictionary(phrase, &dictionary),
            phrase,
            "{phrase:?} must stay"
        );
    }
}

#[test]
fn true_hearings_still_swap_against_a_full_dictionary() {
    let dictionary = names_and_tech();
    for (heard, expected) in [
        ("voice you is great", "Voisu is great"),
        ("switch on hyper land now", "switch on Hyprland now"),
        ("Voice you is open", "Voisu is open"),
        ("try voice-you now", "try Voisu now"),
        ("ask pre yanka about it", "ask Priyanka about it"),
        ("write it in cot lin", "write it in Kotlin"),
        ("I use new vim daily", "I use Neovim daily"),
        ("the post gress database", "the Postgres database"),
        (
            "deploy on cuber net ease today",
            "deploy on Kubernetes today",
        ),
        ("ask clawed code to fix it", "ask Claude Code to fix it"),
        // Split hearings spelled exactly like the entry.
        ("the deep gram model is fine", "the Deepgram model is fine"),
        ("deploy on kuber netes", "deploy on Kubernetes"),
        ("call anu raj later", "call Anuraj later"),
        ("install fed ora today", "install Fedora today"),
        ("tell re becca hello", "tell Rebecca hello"),
        ("run dock er compose", "run Docker compose"),
    ] {
        assert_eq!(
            apply_phonetic_dictionary(heard, &dictionary),
            expected,
            "{heard:?}"
        );
    }
}

#[test]
fn split_entry_words_that_are_function_words_stay() {
    // "in box" and "log in" spell `Inbox` and `Login`, but "in" is a function
    // word: the speaker almost always meant the two words.
    let dictionary = terms(&["Inbox", "Login"]);
    for text in ["check your in box", "please log in now"] {
        assert_eq!(apply_phonetic_dictionary(text, &dictionary), text, "{text}");
    }
}

#[test]
fn everyday_runs_stay_even_when_they_spell_the_entry() {
    for (text, entry) in [
        ("the way land goes", "Wayland"),
        ("walk east one block", "Easton"),
        ("we went out then", "Ethan"),
        ("paint all over it", "Oliver"),
    ] {
        assert_eq!(
            apply_phonetic_dictionary(text, &terms(&[entry])),
            text,
            "{text}"
        );
    }
}

#[test]
fn a_hearing_never_gains_a_syllable_and_acronyms_stay() {
    for (text, entry) in [
        ("set the isk value", "Isaac"),
        ("open the luks volume", "Lucas"),
        ("open the LUKS volume", "Lucas"),
        ("the CVE list", "Kevin"),
    ] {
        assert_eq!(
            apply_phonetic_dictionary(text, &terms(&[entry])),
            text,
            "{text}"
        );
    }
}

#[test]
fn a_hearing_never_leaves_a_stray_word_behind() {
    // "toe key oh" once became "Tokio oh". It is now refused outright: all
    // three are English words, and no exact sound shape covers them.
    let text = "use toe key oh for async";
    assert_eq!(apply_phonetic_dictionary(text, &terms(&["Tokio"])), text);
}

#[test]
fn a_common_word_is_never_swapped_into_a_short_name() {
    // None of these is in the lexicon, and each is one sound off a short
    // name; a single heard word must match a short entry exactly.
    let dictionary = names_and_tech();
    for text in [
        "let's sit on the sofa for a bit",
        "I feel lousy today",
        "the compression is lossy",
        "what an oddity",
        "the meson decays fast",
        "belay that order",
        "a hoary old joke",
        "she played with elan",
        "a cup of kava",
    ] {
        assert_eq!(apply_phonetic_dictionary(text, &dictionary), text, "{text}");
    }
    // At a sentence start the capital says nothing: still a common word.
    assert_eq!(
        apply_phonetic_dictionary("Sofa is comfy.", &dictionary),
        "Sofa is comfy."
    );
    // A name the model wrote mid-sentence keeps the ordinary bound; whether
    // "Anya" should become `Anna` is a product decision left as it was.
    assert_eq!(
        apply_phonetic_dictionary("I talked to Anya yesterday", &terms(&["Anna"])),
        "I talked to Anna yesterday"
    );
}

#[test]
fn a_capitalized_later_word_or_a_leading_no_ends_the_hearing() {
    let dictionary = names_and_tech();
    for text in [
        "a new Vim release",
        "there is no Vim here",
        "there is no vim here",
    ] {
        assert_eq!(apply_phonetic_dictionary(text, &dictionary), text, "{text}");
    }
    assert_eq!(
        apply_phonetic_dictionary("I use new vim daily", &dictionary),
        "I use Neovim daily"
    );
    // A sentence-start capital on the first word still counts.
    assert_eq!(
        apply_phonetic_dictionary("New vim is out.", &dictionary),
        "Neovim is out."
    );
}

#[test]
fn a_longer_entry_still_takes_a_one_word_hearing_one_sound_off() {
    let dictionary = names_and_tech();
    for (heard, expected) in [
        ("switch to hyperland now", "switch to Hyprland now"),
        ("the postgress database", "the Postgres database"),
        ("deploy on kubernetis", "deploy on Kubernetes"),
    ] {
        assert_eq!(
            apply_phonetic_dictionary(heard, &dictionary),
            expected,
            "{heard}"
        );
    }
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
fn the_lexicon_is_a_plain_lowercase_word_list() {
    let lexicon = english_lexicon();
    assert!(lexicon.len() >= 29_000, "{}", lexicon.len());
    assert!(
        lexicon
            .keys()
            .all(|word| { !word.is_empty() && word.bytes().all(|byte| byte.is_ascii_lowercase()) })
    );
    for word in ["sorry", "iron", "tunnel", "merry", "jeans", "walls"] {
        assert!(lexicon.contains_key(word), "{word}");
    }
}

/// A person-heavy Dictionary next to ordinary speech that sounds like it.
#[test]
fn ordinary_sentences_are_never_swapped_against_a_names_dictionary() {
    let dictionary = terms(&[
        "Sarah",
        "Aaron",
        "Daniel",
        "Maria",
        "Olivia",
        "Fedora",
        "Ellen",
        "Docker",
        "Ubuntu",
        "Postgres",
        "James",
        "Jones",
        "Wales",
        "Voisu",
        "Hyprland",
        "Kubernetes",
        "Deepgram",
    ]);
    for sentence in [
        "I am sorry about the delay.",
        "Please iron the shirt tonight.",
        "We drove through the tunnel.",
        "His denial was not convincing.",
        "They want to marry in June.",
        "Have a merry time at the party.",
        "My nephew loves Mario games.",
        "Pass me the olive oil.",
        "Fill the bird feeder again.",
        "Allen called this morning.",
        "The film had an alien in it.",
        "What should we make in the oven?",
        "Did you dock her boat yesterday?",
        "When we land, call me.",
        "Can you bunt too in this league?",
        "They post grace notes on the board.",
        "The jams are in the fridge.",
        "I bought new jeans today.",
        "The walls need fresh paint.",
    ] {
        assert_eq!(
            apply_phonetic_dictionary(sentence, &dictionary),
            sentence,
            "{sentence}"
        );
    }
}

#[test]
fn a_silent_es_or_ed_does_not_count_as_a_syllable() {
    for name in ["James", "Jones", "Wales", "Charles", "Miles", "Hayes"] {
        assert_eq!(syllables(name), 1, "{name}");
    }
    assert_eq!(syllables("Moses"), 2);
    assert_eq!(syllables("Postgres"), 2);
    assert_eq!(syllables("Kubernetes"), 4);
    let encoder = DoubleMetaphone::new(None);
    let eligible = |name: &str| Entry::parse(name, &encoder).is_some_and(|entry| entry.eligible);
    for name in ["James", "Jones", "Wales", "Charles", "Miles", "Hayes"] {
        assert!(!eligible(name), "{name}");
    }
    for name in ["Moses", "Postgres", "Kubernetes"] {
        assert!(eligible(name), "{name}");
    }
}

#[test]
fn multibyte_whitespace_never_panics() {
    // The technical-token check once stepped one byte past a multibyte
    // whitespace character and panicked mid-character.
    for space in ['\u{00A0}', '\u{2009}', '\u{3000}'] {
        let text = format!("say voice{space}you now{space}");
        let _ = apply_phonetic_dictionary(&text, &realistic());
        let text = format!("{space}voice you");
        assert_eq!(
            apply_phonetic_dictionary(&text, &realistic()),
            format!("{space}Voisu")
        );
    }
}

#[test]
fn a_spaced_dash_or_a_line_break_ends_a_run() {
    for text in [
        "I heard a voice - you know",
        "my voice\nyou see",
        "my voice\r\nyou see",
    ] {
        assert_eq!(
            apply_phonetic_dictionary(text, &realistic()),
            text,
            "{text:?}"
        );
    }
    assert_eq!(
        apply_phonetic_dictionary("try voice-you now", &realistic()),
        "try Voisu now"
    );
}

#[test]
fn entries_with_the_same_letters_are_one_entry() {
    for dictionary in [
        ["Claude Code", "claude-code"],
        ["claude-code", "Claude Code"],
    ] {
        let first = dictionary[0];
        assert_eq!(
            apply_phonetic_dictionary("ask clawed code to fix it", &terms(&dictionary)),
            format!("ask {first} to fix it")
        );
    }
}

#[test]
fn entry_casing_is_kept_and_capitals_only_start_sentences() {
    let dictionary = terms(&["iPhone"]);
    assert_eq!(
        apply_phonetic_dictionary("Eye phone is here.", &dictionary),
        "iPhone is here."
    );
    let dictionary = terms(&["kubecontrol"]);
    assert_eq!(
        apply_phonetic_dictionary("Run Cube control now.", &dictionary),
        "Run kubecontrol now."
    );
    assert_eq!(
        apply_phonetic_dictionary("Done. Cube control is up.", &dictionary),
        "Done. Kubecontrol is up."
    );
}

#[test]
fn a_full_dictionary_over_a_long_transcript_is_fast() {
    // 10 x 20 distinct two-part names. Several share Double Metaphone codes
    // with the transcript (Voisu, Voisko, Hyprland, Deepgram), so
    // the shape comparison really runs.
    let heads = ["Vo", "Hy", "Dee", "Ku", "Za", "Mo", "Te", "Ri", "Pa", "Lo"];
    let tails = [
        "isu", "prland", "pgram", "bern", "lin", "dar", "mex", "pol", "vane", "isko", "rith",
        "mund", "tova", "nexa", "quil", "brio", "fent", "gado", "huma", "jolt",
    ];
    let dictionary: Vec<String> = heads
        .iter()
        .flat_map(|head| tails.iter().map(move |tail| format!("{head}{tail}")))
        .collect();
    let distinct: HashSet<String> = dictionary.iter().map(|term| term.to_lowercase()).collect();
    assert_eq!(distinct.len(), USER_VOCABULARY_TERM_LIMIT);
    assert_eq!(dictionary.len(), USER_VOCABULARY_TERM_LIMIT);
    let sentence = "so I said voice you runs on hyper land and the deep gram model is fine ";
    let text = sentence.repeat(2000 / sentence.split_whitespace().count() + 1);
    let words = text.split_whitespace().count();
    assert!(words >= 2000);
    let started = std::time::Instant::now();
    let corrected = apply_phonetic_dictionary(&text, &dictionary);
    let elapsed = started.elapsed();
    assert!(corrected.contains("Voisu"), "{corrected}");
    assert!(corrected.contains("Deepgram"), "{corrected}");
    eprintln!(
        "phonetic pass: {} entries x {words} words in {elapsed:?}",
        dictionary.len()
    );
    assert!(elapsed < std::time::Duration::from_secs(1), "{elapsed:?}");
}

#[test]
fn the_unfamiliar_pass_needs_a_heard_word_outside_the_lexicon() {
    let entries = terms(&["Voisu", "Deepgram", "Codex", "TypeScript"]);
    for (heard, corrected) in [
        ("ask voysu now", "ask Voisu now"),
        ("ask Vosu now", "ask Voisu now"),
        ("open kodex", "open Codex"),
    ] {
        assert_eq!(
            apply_unfamiliar_phonetic_dictionary(heard, &entries, &[]),
            corrected
        );
    }
    for english in [
        "the voice you hear is mine",
        "the deep gram of salt",
        "the code ex boyfriend",
        "I love the type script",
    ] {
        assert_eq!(
            apply_unfamiliar_phonetic_dictionary(english, &entries, &[]),
            english
        );
    }
}

#[test]
fn the_unfamiliar_pass_never_touches_a_protected_term() {
    // "Voysu" is the user's own spelling, already written by an earlier pass.
    assert_eq!(
        apply_unfamiliar_phonetic_dictionary(
            "ask Voysu now",
            &terms(&["Voisu"]),
            &terms(&["Voysu"])
        ),
        "ask Voysu now"
    );
    assert_eq!(
        apply_unfamiliar_phonetic_dictionary("ask Voysu now", &terms(&["Voisu"]), &[]),
        "ask Voisu now"
    );
}
