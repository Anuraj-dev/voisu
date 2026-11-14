//! Developer-only helper for `tools/local-asr/llm_bench.py`.
//!
//! The benchmark must send the request production sends, so the prompt, schema
//! and sampling parameters come from this crate and are never copied into
//! Python. Run as
//! `cargo run -q -p voisu-cloud --example dump_llm_request -- --task intent-reconstruction`.
//!
//! The process reads one JSON object per line on stdin and answers with one
//! JSON line on stdout (flushed per line), so one process serves a whole run.
//!
//! `{"op":"request","id":..,"deepgram":..,"groq":..}` drives the real
//! `TranscriptDecisionPipeline::prepare`, which sanitises the sources and
//! decides whether production would call the model at all. Answer:
//! `{"id","ok":true,"eligible","eligibility","selection","fallback_text","body"}`
//! where `body` is the production request body (`model` is the production
//! default; the caller replaces it) and is `null` when production would not
//! call the model, in which case `fallback_text` is what it would deliver.
//!
//! `{"op":"accept","id":..,"deepgram":..,"groq":..,"response_body":..}` feeds
//! the provider response body (`null` models a failed call) through the real
//! response parser and `TranscriptDecisionPipeline::reconstruct` validation.
//! Answer: `{"id","ok":true,"text","selection","outcome","validation_reason"}`
//! with `text` being what production would deliver (the reconstruction, or the
//! safe source fallback when it is rejected or failed).
//!
//! Not reproduced: dictionary terms, user vocabulary and word confidences are
//! empty (production feeds them from the user's dictionary and the providers).

use std::io::{BufRead, Write};
use std::sync::Arc;

use serde_json::{Value, json};
use voisu_cloud::{
    INTENT_RECONSTRUCTION_DEADLINE, groq_intent_reconstruction_request_body,
    parse_intent_reconstruction_response_body,
};
use voisu_core::{
    BoundaryError, BoundaryFuture, BoundaryKind, CancelRegistry, IntentReconstructionRequest,
    MergeResult, PreparedTranscriptDecision, Provider, ReconciliationKind, ReconciliationModel,
    SourceTranscript, TranscriptDecision, TranscriptDecisionPipeline,
};

/// Hands the pipeline a response that was already fetched (or a failure).
struct CannedModel {
    result: Option<Result<MergeResult, BoundaryError>>,
}

impl ReconciliationModel for CannedModel {
    fn request(
        &mut self,
        _kind: ReconciliationKind,
        _sources: Vec<SourceTranscript>,
        _candidate: Option<MergeResult>,
        _cancel: Arc<CancelRegistry>,
    ) -> BoundaryFuture<'_, MergeResult> {
        Box::pin(async {
            Err(BoundaryError::new(
                BoundaryKind::Validation,
                "legacy reconciliation is not benchmarked",
            ))
        })
    }

    fn reconstruct_intent(
        &mut self,
        _request: IntentReconstructionRequest,
        _cancel: Arc<CancelRegistry>,
    ) -> BoundaryFuture<'_, MergeResult> {
        let result = self.result.take().unwrap_or_else(|| {
            Err(BoundaryError::new(
                BoundaryKind::Validation,
                "no canned response",
            ))
        });
        Box::pin(async move { result })
    }
}

fn sources(line: &Value) -> Vec<SourceTranscript> {
    let text = |key: &str| line.get(key).and_then(Value::as_str).unwrap_or_default();
    vec![
        SourceTranscript {
            provider: Provider::Deepgram,
            text: text("deepgram").to_owned(),
        },
        SourceTranscript {
            provider: Provider::Groq,
            text: text("groq").to_owned(),
        },
    ]
}

fn pipeline(
    result: Option<Result<MergeResult, BoundaryError>>,
) -> TranscriptDecisionPipeline<CannedModel> {
    TranscriptDecisionPipeline::with_intent_reconstruction(
        CannedModel { result },
        INTENT_RECONSTRUCTION_DEADLINE,
        Vec::new(),
    )
}

fn decision_json(decision: &TranscriptDecision) -> Value {
    let evidence = decision.intent_reconstruction.as_ref();
    json!({
        "text": decision.transcript.0,
        "selection": serde_json::to_value(decision.selection).unwrap_or(Value::Null),
        "outcome": evidence
            .and_then(|e| serde_json::to_value(e.outcome).ok())
            .unwrap_or(Value::Null),
        "validation_reason": decision.validation_reason,
    })
}

async fn handle(line: &Value) -> Value {
    let id = line.get("id").cloned().unwrap_or(Value::Null);
    let op = line.get("op").and_then(Value::as_str).unwrap_or_default();
    let prepared = pipeline(None).prepare(sources(line)).await;
    let mut answer = match (op, prepared) {
        (_, Err(error)) => json!({"ok": false, "error": error.diagnostic()}),
        ("request", Ok(PreparedTranscriptDecision::Reconstruct(attempt))) => json!({
            "ok": true,
            "eligible": true,
            "eligibility": serde_json::to_value(attempt.eligibility).unwrap_or(Value::Null),
            "body": groq_intent_reconstruction_request_body(&attempt.request),
        }),
        ("request", Ok(PreparedTranscriptDecision::Ready(decision))) => {
            let mut answer = decision_json(&decision);
            let object = answer.as_object_mut().expect("decision JSON is an object");
            object.insert("ok".to_owned(), json!(true));
            object.insert("eligible".to_owned(), json!(false));
            object.insert("body".to_owned(), Value::Null);
            answer
        }
        ("accept", Ok(PreparedTranscriptDecision::Reconstruct(attempt))) => {
            let result = match line.get("response_body").and_then(Value::as_str) {
                Some(body) => parse_intent_reconstruction_response_body(body.as_bytes()),
                None => Err(BoundaryError::new(
                    BoundaryKind::Validation,
                    "Intent Reconstruction request failed",
                )),
            };
            match pipeline(Some(result)).reconstruct(attempt).await {
                Ok(decision) => {
                    let mut answer = decision_json(&decision);
                    answer
                        .as_object_mut()
                        .expect("decision JSON is an object")
                        .insert("ok".to_owned(), json!(true));
                    answer
                }
                Err(error) => json!({"ok": false, "error": error.diagnostic()}),
            }
        }
        ("accept", Ok(PreparedTranscriptDecision::Ready(decision))) => {
            let mut answer = decision_json(&decision);
            let object = answer.as_object_mut().expect("decision JSON is an object");
            object.insert("ok".to_owned(), json!(true));
            object.insert("outcome".to_owned(), json!("skipped"));
            answer
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
    if task.as_deref() != Some("intent-reconstruction") {
        eprintln!("usage: dump_llm_request --task intent-reconstruction");
        std::process::exit(2);
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("tokio runtime");
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    for line in std::io::stdin().lock().lines() {
        let line = line.expect("stdin line");
        if line.trim().is_empty() {
            continue;
        }
        let answer = match serde_json::from_str::<Value>(&line) {
            Ok(value) => runtime.block_on(handle(&value)),
            Err(_) => json!({"ok": false, "error": "invalid JSON line"}),
        };
        writeln!(stdout, "{answer}").expect("stdout write");
        stdout.flush().expect("stdout flush");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(line: Value) -> Value {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime")
            .block_on(handle(&line))
    }

    const DEEPGRAM: &str = "open the config file and check the paths";
    const GROQ: &str = "open the con fig file and check the path";

    #[test]
    fn dumped_body_is_the_production_body() {
        let answer = run(json!({"op": "request", "id": "a", "deepgram": DEEPGRAM, "groq": GROQ}));
        assert_eq!(answer["ok"], true);
        assert_eq!(answer["eligible"], true, "{answer}");
        let expected = groq_intent_reconstruction_request_body(&IntentReconstructionRequest {
            sources: sources(&json!({"deepgram": DEEPGRAM, "groq": GROQ})),
            dictionary_terms: Vec::new(),
        });
        assert_eq!(answer["body"], expected);
        assert_eq!(answer["id"], "a");
    }

    #[test]
    fn identical_sources_are_not_eligible_and_fall_back() {
        let answer =
            run(json!({"op": "request", "id": "b", "deepgram": DEEPGRAM, "groq": DEEPGRAM}));
        assert_eq!(answer["eligible"], false);
        assert_eq!(answer["body"], Value::Null);
        assert_eq!(answer["text"], DEEPGRAM);
    }

    fn accept(content: &str) -> Value {
        let body = json!({"choices": [{"message": {"content": content}}]}).to_string();
        run(json!({
            "op": "accept", "id": "c", "deepgram": DEEPGRAM, "groq": GROQ,
            "response_body": body,
        }))
    }

    #[test]
    fn accepted_reconstruction_is_delivered() {
        let answer = accept(r#"{"wording":"open the config file and check the path"}"#);
        assert_eq!(answer["outcome"], "accepted", "{answer}");
        assert_eq!(answer["text"], "open the config file and check the path");
    }

    #[test]
    fn invalid_shape_and_failed_call_fall_back_to_a_source() {
        let bad = accept(r#"{"text":"wrong key"}"#);
        assert_eq!(bad["outcome"], "failed", "{bad}");
        assert!(bad["text"] == DEEPGRAM || bad["text"] == GROQ, "{bad}");
        let none = run(json!({
            "op": "accept", "id": "d", "deepgram": DEEPGRAM, "groq": GROQ,
            "response_body": Value::Null,
        }));
        assert_eq!(none["outcome"], "failed", "{none}");
    }

    #[test]
    fn production_length_guard_rejects_a_truncated_candidate() {
        let deepgram = "please open the config file and then check all of the paths in it";
        let groq = "please open the con fig file and then check all of the path in it";
        let body = json!({"choices": [{"message": {"content": r#"{"wording":"please open"}"#}}]});
        let answer = run(json!({
            "op": "accept", "id": "e", "deepgram": deepgram, "groq": groq,
            "response_body": body.to_string(),
        }));
        assert_eq!(answer["outcome"], "rejected", "{answer}");
        assert!(
            answer["text"] == deepgram || answer["text"] == groq,
            "{answer}"
        );
    }
}
