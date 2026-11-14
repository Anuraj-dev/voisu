//! Developer-only helper for `tools/local-asr/llm_bench.py --task grammar`.
//!
//! Emits the exact production Minimal Grammar request body and runs a provider
//! response through the production envelope check and safety gate, so the
//! benchmark never keeps its own copy of the prompt or schema. Run as
//! `cargo run -q -p voisu-app --example dump_llm_request -- --task grammar`.
//!
//! One JSON object per line on stdin, one JSON line per answer on stdout.
//!
//! `{"op":"request","id":..,"text":..}` builds the Formatting baseline for the
//! text and answers `{"id","ok":true,"eligible","body","text"}` where `body`
//! is the production request (`model` is the production default; the caller
//! replaces it) and `text` is the Formatting-only result.
//!
//! `{"op":"accept","id":..,"text":..,"response_body":..}` answers
//! `{"id","ok":true,"text","outcome","error_codes"}` with `text` being what
//! production delivers: the grammar-composed text when the edits are accepted,
//! otherwise the Formatting baseline. `outcome` is `accepted`, `unchanged`
//! (no edits), `rejected` (the safety gate refused them) or `failed` (no
//! response or an invalid provider envelope). `response_body: null` models a
//! failed call.
//!
//! Not reproduced: the English-eligibility and command-span gates, dictionary
//! terms and protected names (empty here), and the production transport.

use std::io::{BufRead, Write};

use serde_json::{Value, json};
use voisu_app::minimal_grammar::{build_request, extract_candidate};
use voisu_core::{
    FormatOptions, FormattingBaseline, GrammarOutcome, GrammarSafetyOptions,
    MAX_VALIDATED_TRANSCRIPT_UTF8_BYTES, apply_grammar_candidate_json,
    format_validated_for_grammar,
};

fn baseline(text: &str) -> FormattingBaseline {
    format_validated_for_grammar(text, FormatOptions::default())
}

fn handle(line: &Value) -> Value {
    let id = line.get("id").cloned().unwrap_or(Value::Null);
    let text = line.get("text").and_then(Value::as_str).unwrap_or_default();
    let baseline = baseline(text);
    let mut answer = match line.get("op").and_then(Value::as_str) {
        Some("request") => {
            let eligible = text.len() <= MAX_VALIDATED_TRANSCRIPT_UTF8_BYTES;
            json!({
                "ok": true,
                "eligible": eligible,
                "body": eligible.then(|| build_request(text, &baseline)),
                "text": baseline.rendered(),
            })
        }
        Some("accept") => {
            let candidate = line
                .get("response_body")
                .and_then(Value::as_str)
                .and_then(|body| extract_candidate(body.as_bytes()).ok());
            match candidate {
                None => json!({
                    "ok": true, "text": baseline.rendered(),
                    "outcome": "failed", "error_codes": [],
                }),
                Some(candidate) => {
                    let result = apply_grammar_candidate_json(
                        text,
                        baseline.base_version(),
                        &baseline,
                        &candidate,
                        GrammarSafetyOptions::default(),
                    );
                    let (delivered, outcome) = match result.outcome {
                        GrammarOutcome::Both | GrammarOutcome::GrammarOnly => {
                            (result.rendered.clone(), "accepted")
                        }
                        _ if result.diagnostics.is_empty() => {
                            (baseline.rendered().to_owned(), "unchanged")
                        }
                        _ => (baseline.rendered().to_owned(), "rejected"),
                    };
                    json!({
                        "ok": true, "text": delivered, "outcome": outcome,
                        "error_codes": result.error_codes(),
                    })
                }
            }
        }
        _ => json!({"ok": false, "error": "unknown op"}),
    };
    answer
        .as_object_mut()
        .expect("answer is an object")
        .insert("id".to_owned(), id);
    answer
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut task = None;
    while let Some(arg) = args.next() {
        if arg == "--task" {
            task = args.next();
        }
    }
    if task.as_deref() != Some("grammar") {
        eprintln!("usage: dump_llm_request --task grammar");
        std::process::exit(2);
    }
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    for line in std::io::stdin().lock().lines() {
        let line = line.expect("stdin line");
        if line.trim().is_empty() {
            continue;
        }
        let answer = match serde_json::from_str::<Value>(&line) {
            Ok(value) => handle(&value),
            Err(_) => json!({"ok": false, "error": "invalid JSON line"}),
        };
        writeln!(stdout, "{answer}").expect("stdout write");
        stdout.flush().expect("stdout flush");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "lets meet there is 3 issues and he didnt come";

    fn accept(response: Value) -> Value {
        handle(&json!({
            "op": "accept", "id": "x", "text": TEXT,
            "response_body": response.to_string(),
        }))
    }

    fn envelope(candidate: &Value) -> Value {
        json!({"choices": [{"message": {"content": candidate.to_string()}}]})
    }

    #[test]
    fn dumped_body_is_the_production_body() {
        let answer = handle(&json!({"op": "request", "id": "x", "text": TEXT}));
        assert_eq!(answer["eligible"], true);
        assert_eq!(answer["body"], build_request(TEXT, &baseline(TEXT)));
        assert_eq!(answer["body"]["model"], "openai/gpt-oss-20b");
    }

    #[test]
    fn failed_call_and_bad_envelope_deliver_the_formatting_baseline() {
        let formatted = baseline(TEXT).rendered().to_owned();
        let none = handle(&json!({"op": "accept", "id": "x", "text": TEXT, "response_body": null}));
        assert_eq!(
            (none["outcome"].as_str(), none["text"].as_str()),
            (Some("failed"), Some(formatted.as_str()))
        );
        let bad = accept(json!({"choices": []}));
        assert_eq!(bad["outcome"], "failed");
    }

    #[test]
    fn empty_edits_are_unchanged_and_a_stale_candidate_is_rejected() {
        let base = baseline(TEXT);
        let empty = accept(envelope(&json!({
            "base_version": base.base_version(),
            "base_fingerprint": base.base_fingerprint(),
            "edits": [],
        })));
        assert_eq!(empty["outcome"], "unchanged", "{empty}");
        assert_eq!(empty["text"], base.rendered());
        let stale = accept(envelope(&json!({
            "base_version": base.base_version(),
            "base_fingerprint": format!("sha256:{}", "0".repeat(64)),
            "edits": [],
        })));
        assert_eq!(stale["outcome"], "rejected", "{stale}");
    }
}
