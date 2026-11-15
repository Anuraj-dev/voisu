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
    let input: Box<dyn Read> = match std::env::args().nth(1) {
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
