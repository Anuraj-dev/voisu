//! Prompt-echo filter, ported from `_is_prompt_echo` in
//! `tools/local-asr/voisu_asr/service.py`.

use std::collections::HashMap;

use regex::{Regex, RegexBuilder};

/// True when `text` only recites the prompt glossary.
///
/// A quiet window can make Whisper emit its own prompt: a run of three or
/// more consecutive prompt terms, in prompt order, joined by commas or spaces,
/// optionally ending in a period, and optionally led by the daemon's
/// `Glossary:` sentence opener. Shorter or reordered lists stay, because
/// people do dictate a name or two ("OpenAI, Anthropic.", "GPT LLM").
#[must_use]
pub fn is_prompt_echo(text: &str, prompt: &str) -> bool {
    let glossary = Regex::new(r"(?s)\A\s*Glossary:\s*(.*?)\.?\s*\z").expect("static pattern");
    let listed = glossary
        .captures(prompt)
        .and_then(|captures| captures.get(1))
        .map_or(prompt, |listed| listed.as_str());

    let mut position: HashMap<String, usize> = HashMap::new();
    let mut terms: Vec<String> = Vec::new();
    for item in listed
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
    {
        let key = item.to_lowercase();
        if !position.contains_key(&key) {
            position.insert(key.clone(), terms.len());
            terms.push(key);
        }
    }
    if terms.len() < 3 {
        return false;
    }
    // Longest first, so "Claude Code" wins over "Claude"; the sort is stable,
    // keeping prompt order among equal lengths like the Python reference.
    terms.sort_by_key(|term| std::cmp::Reverse(term.chars().count()));
    let term = terms
        .iter()
        .map(|term| regex::escape(term))
        .collect::<Vec<_>>()
        .join("|");

    let Ok(recital) = RegexBuilder::new(&format!(
        r"\A\s*(?:glossary:?\s*)?((?:{term})(?:[\s,]+(?:{term}))+)[\s,]*\.?\s*\z"
    ))
    .case_insensitive(true)
    .build() else {
        return false;
    };
    let Some(run) = recital.captures(text).and_then(|captures| captures.get(1)) else {
        return false;
    };
    let Ok(found) = RegexBuilder::new(&term).case_insensitive(true).build() else {
        return false;
    };
    let mut order = Vec::new();
    for item in found.find_iter(run.as_str()) {
        match position.get(&item.as_str().to_lowercase()) {
            Some(&index) => order.push(index),
            None => return false,
        }
    }
    order.len() >= 3 && order.windows(2).all(|pair| pair[1] == pair[0] + 1)
}
