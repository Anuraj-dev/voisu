//! Dev-only evidence helper: lists every phonetic Dictionary swap the Local
//! tail would make, for false-swap counting over a speech corpus.
//!
//! Input is JSON Lines, from the file named by the first argument or stdin:
//!
//! ```text
//! {"id": "rec-1", "text": "I use voice you daily", "dictionary": ["Voisu"]}
//! ```
//!
//! `id` is optional. Like the Local tail, exact-match correction runs first
//! and the phonetic pass runs on its result. Output is one tab-separated line
//! per swap (`id`, heard words, entry), then a `# lines … swaps …` summary.
//!
//! ```text
//! cargo run -p voisu-core --example phonetic_swaps -- corpus.jsonl
//! ```
//!
//! Reference mode counts false swaps over correct text: every non-empty line
//! of the text file is a sentence that needs no correction, and the
//! Dictionary file holds one entry per line. Every swap is false; the last
//! line is `# lines … false-swaps …`.
//!
//! ```text
//! cargo run -p voisu-core --example phonetic_swaps -- --text book.txt --dictionary names.txt
//! ```

use std::io::{self, BufRead, BufReader, Read};

use serde::Deserialize;

#[derive(Deserialize)]
struct Line {
    #[serde(default)]
    id: Option<String>,
    text: String,
    dictionary: Vec<String>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "--text") {
        let [_, text, flag, dictionary] = args.as_slice() else {
            return Err("usage: --text <file> --dictionary <file>".into());
        };
        if flag != "--dictionary" {
            return Err("usage: --text <file> --dictionary <file>".into());
        }
        return reference_mode(text, dictionary);
    }
    let input: Box<dyn Read> = match args.first() {
        Some(path) => Box::new(std::fs::File::open(path)?),
        None => Box::new(io::stdin()),
    };
    let mut lines = 0usize;
    let mut swaps = 0usize;
    for (number, line) in BufReader::new(input).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let record: Line =
            serde_json::from_str(&line).map_err(|error| format!("line {}: {error}", number + 1))?;
        lines += 1;
        let id = record.id.unwrap_or_else(|| format!("line-{}", number + 1));
        let exact = voisu_core::apply_user_vocabulary(&record.text, &record.dictionary, &[]);
        for swap in voisu_core::phonetic_dictionary_swaps(&exact, &record.dictionary) {
            swaps += 1;
            println!("{id}\t{}\t{}", swap.heard, swap.replacement);
        }
    }
    println!("# lines {lines} swaps {swaps}");
    Ok(())
}

fn reference_mode(text: &str, dictionary: &str) -> Result<(), Box<dyn std::error::Error>> {
    let dictionary: Vec<String> = std::fs::read_to_string(dictionary)?
        .lines()
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect();
    let mut lines = 0usize;
    let mut false_swaps = 0usize;
    for (number, line) in std::fs::read_to_string(text)?.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        lines += 1;
        let exact = voisu_core::apply_user_vocabulary(line, &dictionary, &[]);
        for swap in voisu_core::phonetic_dictionary_swaps(&exact, &dictionary) {
            false_swaps += 1;
            println!("line-{}\t{}\t{}", number + 1, swap.heard, swap.replacement);
        }
    }
    println!("# lines {lines} false-swaps {false_swaps}");
    Ok(())
}
