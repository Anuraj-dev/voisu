//! Architecture A Final Transform Gate (Smart Writing SW10 / spec §8).

use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::time::{Duration, Instant};

use futures_util::FutureExt;
use voisu_core::{
    apply_grammar_candidate_json, format_validated_for_grammar, format_validated_with,
    parse_formatting_commands, BoundaryError, DeliveryAdapter, DeliveryOutcome,
    EnglishEligibilityOutcome, FormatOptions, GrammarErrorCode, GrammarOutcome,
    GrammarSafetyOptions, Provider, SmartWritingDiagnostic, SmartWritingEditEvidence,
    SmartWritingMode, SmartWritingOutcome, SmartWritingReasonCode, Transcript,
    WritingMode as CoreWritingMode, FORMATTER_CONTRACT_ID, LOCAL_FORMATTER_WORK_DEADLINE,
    MAX_VALIDATED_TRANSCRIPT_UTF8_BYTES,
};

use crate::config::WritingMode;
use crate::grammar_http::MAX_GRAMMAR_RESPONSE_BYTES;
use crate::minimal_grammar::{
    MinimalGrammarAdapter, MinimalGrammarError, GRAMMAR_RESULT_CUTOFF_FROM_GATE_ENTRY,
    MINIMAL_GRAMMAR_MODEL,
};

pub const FINAL_TRANSFORM_GATE_DEADLINE: Duration = Duration::from_millis(1_000);
pub const DELIVERY_INITIATION_RESERVE: Duration = Duration::from_millis(100);
pub const CANDIDATE_PIPELINE_DEADLINE: Duration = Duration::from_millis(900);
pub const LOCAL_SAFETY_COMPOSE_WORK_DEADLINE: Duration = Duration::from_millis(100);

const _: () = assert!(
    CANDIDATE_PIPELINE_DEADLINE.as_millis() + DELIVERY_INITIATION_RESERVE.as_millis()
        == FINAL_TRANSFORM_GATE_DEADLINE.as_millis()
);
const _: () = assert!(
    GRAMMAR_RESULT_CUTOFF_FROM_GATE_ENTRY.as_millis()
        + LOCAL_SAFETY_COMPOSE_WORK_DEADLINE.as_millis()
        == CANDIDATE_PIPELINE_DEADLINE.as_millis()
);

/// Exact language declarations actually supplied to active transcription
/// providers for one Recording. Eligibility never inspects transcript words.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedRecordingLanguages {
    declarations: Vec<(Provider, String)>,
}

impl ResolvedRecordingLanguages {
    #[must_use]
    pub fn new(declarations: Vec<(Provider, String)>) -> Self {
        Self { declarations }
    }

    #[must_use]
    pub fn english_eligibility(&self) -> EnglishEligibilityOutcome {
        if self.declarations.is_empty()
            || self
                .declarations
                .iter()
                .any(|(_, language)| !is_explicit_english(language))
        {
            EnglishEligibilityOutcome::Ineligible
        } else {
            EnglishEligibilityOutcome::Eligible
        }
    }

    #[must_use]
    pub fn declarations(&self) -> &[(Provider, String)] {
        &self.declarations
    }
}

fn is_explicit_english(language: &str) -> bool {
    let normalized = language.trim().to_ascii_lowercase();
    normalized == "en"
        || normalized
            .strip_prefix("en-")
            .is_some_and(|region| {
                !region.is_empty()
                    && region
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            })
}

pub type GrammarBoundaryFuture<'a> = Pin<
    Box<dyn Future<Output = Result<Vec<u8>, MinimalGrammarError>> + Send + 'a>,
>;

/// Injection seam for hermetic gate tests; production is the SW6 adapter.
pub trait GrammarRequestBoundary: Send + Sync {
    fn request_candidate<'a>(
        &'a self,
        bearer_token: &'a str,
        validated_transcript: &'a str,
        baseline: &'a voisu_core::FormattingBaseline,
        gate_entry: Instant,
    ) -> GrammarBoundaryFuture<'a>;
}

impl GrammarRequestBoundary for MinimalGrammarAdapter {
    fn request_candidate<'a>(
        &'a self,
        bearer_token: &'a str,
        validated_transcript: &'a str,
        baseline: &'a voisu_core::FormattingBaseline,
        gate_entry: Instant,
    ) -> GrammarBoundaryFuture<'a> {
        Box::pin(self.request_candidate(
            bearer_token,
            validated_transcript,
            baseline,
            gate_entry,
        ))
    }
}

pub enum GrammarGateCapability<'a> {
    Ready {
        boundary: &'a dyn GrammarRequestBoundary,
        bearer_token: &'a str,
    },
    Unavailable,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CredentialGateEvidence {
    pub prep_latency_ms: Option<u64>,
    pub reap_watchdog_crossed: bool,
}

pub struct FinalTransformInput<'a> {
    pub validated_transcript: &'a str,
    pub writing_mode: WritingMode,
    pub languages: &'a ResolvedRecordingLanguages,
    pub grammar: GrammarGateCapability<'a>,
    pub dictionary_terms: &'a [&'a str],
    pub protected_names: &'a [&'a str],
    pub credential: CredentialGateEvidence,
}

pub struct FinalTransformCompletion {
    pub rendered: String,
    pub delivery: Result<DeliveryOutcome, BoundaryError>,
    pub diagnostic: SmartWritingDiagnostic,
}

/// Run candidate work after Validation and initiate exactly one Delivery.
/// Formatting is persisted before any untrusted grammar work, so every later
/// error, timeout, rejection, or panic preserves the baseline byte-for-byte.
pub async fn final_transform_and_deliver(
    input: FinalTransformInput<'_>,
    delivery: &mut dyn DeliveryAdapter,
) -> FinalTransformCompletion {
    let gate_entry = Instant::now();
    let candidate_deadline = gate_entry + CANDIDATE_PIPELINE_DEADLINE;
    let parsed_commands = parse_formatting_commands(input.validated_transcript);
    let english = input.languages.english_eligibility();
    let mode = match input.writing_mode {
        WritingMode::Smart => CoreWritingMode::Smart,
        WritingMode::Literal => CoreWritingMode::Literal,
    };

    let formatter_started = Instant::now();
    let formatted = catch_unwind(AssertUnwindSafe(|| match mode {
        CoreWritingMode::Smart => format_validated_for_grammar(
            input.validated_transcript,
            FormatOptions {
                dictionary: input.dictionary_terms,
                protected_names: input.protected_names,
                ..FormatOptions::default()
            },
        ),
        CoreWritingMode::Literal => format_validated_with(
            input.validated_transcript,
            CoreWritingMode::Literal,
            FormatOptions {
                dictionary: input.dictionary_terms,
                protected_names: input.protected_names,
                ..FormatOptions::default()
            },
        ),
    }));
    let formatter_latency = formatter_started.elapsed();

    let mut reasons = vec![match input.writing_mode {
        WritingMode::Smart => SmartWritingReasonCode::ModeSmart,
        WritingMode::Literal => SmartWritingReasonCode::ModeLiteral,
    }];
    let mut selected = input.validated_transcript.to_owned();
    let mut formatter_contract = FORMATTER_CONTRACT_ID.to_owned();
    let mut formatter_failed = false;

    let baseline = match formatted {
        Ok(baseline) => {
            formatter_contract = baseline.formatter_contract().to_owned();
            selected = baseline.rendered().to_owned();
            Some(baseline)
        }
        Err(_) => {
            formatter_failed = true;
            reasons.push(SmartWritingReasonCode::FormatterPanic);
            None
        }
    };
    if input.validated_transcript.len() > MAX_VALIDATED_TRANSCRIPT_UTF8_BYTES {
        formatter_failed = true;
        reasons.push(SmartWritingReasonCode::InputOversize);
    } else if !formatter_failed && formatter_latency >= LOCAL_FORMATTER_WORK_DEADLINE {
        // The core formatter returns an identity baseline whenever its
        // cooperative 50 ms budget is reached. Its typed baseline deliberately
        // keeps the fields private, so the elapsed bound is the observable
        // distinction available at this gate boundary.
        formatter_failed = true;
        push_unique_reason(&mut reasons, SmartWritingReasonCode::FormatterDeadline);
    }

    let mut request_began = false;
    let mut http_latency = None;
    let mut safety_latency = None;
    let mut grammar_accepted = false;
    let mut edit_evidence = Vec::new();

    if input.writing_mode == WritingMode::Smart && !formatter_failed {
        if english == EnglishEligibilityOutcome::Ineligible {
            reasons.push(SmartWritingReasonCode::EnglishIneligible);
        } else if parsed_commands.has_command_span() {
            // D_cmd-A separability: local Formatting survives, provider is skipped.
        } else if let (Some(baseline), GrammarGateCapability::Ready { boundary, bearer_token }) =
            (baseline.as_ref(), &input.grammar)
        {
            if Instant::now() >= candidate_deadline {
                push_unique_reason(&mut reasons, SmartWritingReasonCode::SafetyDeadline);
            } else {
                request_began = true;
                let http_started = Instant::now();
                let request = tokio::time::timeout_at(
                    tokio::time::Instant::from_std(candidate_deadline),
                    AssertUnwindSafe(boundary.request_candidate(
                        bearer_token,
                        input.validated_transcript,
                        baseline,
                        gate_entry,
                    ))
                    .catch_unwind(),
                )
                .await;
                http_latency = Some(duration_millis(http_started.elapsed()));

                match request {
                    Ok(Ok(Ok(candidate))) if Instant::now() < candidate_deadline => {
                        let safety_started = Instant::now();
                        let safety = catch_unwind(AssertUnwindSafe(|| {
                            apply_grammar_candidate_json(
                                input.validated_transcript,
                                baseline.base_version(),
                                baseline,
                                &candidate,
                                GrammarSafetyOptions {
                                    dictionary_terms: input.dictionary_terms,
                                    protected_names: input.protected_names,
                                },
                            )
                        }));
                        let safety_elapsed = safety_started.elapsed();
                        safety_latency = Some(duration_millis(safety_elapsed));
                        match safety {
                            Ok(result)
                                if safety_elapsed <= LOCAL_SAFETY_COMPOSE_WORK_DEADLINE
                                    && gate_entry.elapsed() < CANDIDATE_PIPELINE_DEADLINE =>
                            {
                                if matches!(
                                    result.outcome,
                                    GrammarOutcome::Both | GrammarOutcome::GrammarOnly
                                ) {
                                    selected = result.rendered.clone();
                                    grammar_accepted = true;
                                    push_unique_reason(
                                        &mut reasons,
                                        SmartWritingReasonCode::EditAccepted,
                                    );
                                }
                                let fallback_code = result
                                    .diagnostics
                                    .first()
                                    .map(|diagnostic| map_grammar_error(diagnostic.code))
                                    .unwrap_or(SmartWritingReasonCode::Malformed);
                                edit_evidence = edit_evidence_from_candidate(
                                    &candidate,
                                    &result,
                                    if grammar_accepted {
                                        SmartWritingReasonCode::EditAccepted
                                    } else {
                                        fallback_code
                                    },
                                );
                                for diagnostic in result.diagnostics {
                                    push_unique_reason(
                                        &mut reasons,
                                        map_grammar_error(diagnostic.code),
                                    );
                                }
                            }
                            Ok(_) => push_unique_reason(
                                &mut reasons,
                                SmartWritingReasonCode::SafetyDeadline,
                            ),
                            Err(_) => {
                                push_unique_reason(&mut reasons, SmartWritingReasonCode::SafetyPanic)
                            }
                        }
                    }
                    Ok(Ok(Ok(_))) | Err(_) => {
                        push_unique_reason(&mut reasons, SmartWritingReasonCode::SafetyDeadline)
                    }
                    Ok(Ok(Err(error))) => {
                        push_unique_reason(&mut reasons, map_adapter_error(&error))
                    }
                    Ok(Err(_)) => {
                        push_unique_reason(&mut reasons, SmartWritingReasonCode::HttpTransport)
                    }
                }
            }
        } else {
            reasons.push(SmartWritingReasonCode::CapabilityUnavailable);
        }
    }

    let outcome = match input.writing_mode {
        WritingMode::Literal if formatter_failed => SmartWritingOutcome::LiteralFallback,
        WritingMode::Literal if parsed_commands.has_command_span() => {
            SmartWritingOutcome::LiteralCommands
        }
        WritingMode::Literal => SmartWritingOutcome::Literal,
        WritingMode::Smart if formatter_failed => SmartWritingOutcome::IdentityFallback,
        WritingMode::Smart if grammar_accepted => SmartWritingOutcome::FormattingAndGrammar,
        WritingMode::Smart => SmartWritingOutcome::FormattingOnly,
    };
    let mut diagnostic = SmartWritingDiagnostic::new(
        match input.writing_mode {
            WritingMode::Smart => SmartWritingMode::Smart,
            WritingMode::Literal => SmartWritingMode::Literal,
        },
        english,
        formatter_contract,
        input.validated_transcript,
        &selected,
        outcome,
    );
    diagnostic.request_began = request_began;
    diagnostic.formatter_latency_ms = Some(duration_millis(formatter_latency));
    diagnostic.http_latency_ms = http_latency;
    diagnostic.safety_latency_ms = safety_latency;
    diagnostic.credential_prep_latency_ms = input.credential.prep_latency_ms;
    diagnostic.reap_watchdog_crossed = input.credential.reap_watchdog_crossed;
    if input.credential.reap_watchdog_crossed {
        push_unique_reason(&mut reasons, SmartWritingReasonCode::CleanupOverrun);
    }
    if request_began {
        diagnostic.set_model_id(MINIMAL_GRAMMAR_MODEL);
    }
    diagnostic.set_edits(edit_evidence);
    diagnostic.reason_codes = reasons;

    // Candidate is frozen here. Delivery is the sole side effect and is called
    // exactly once even when candidate work fell back.
    let delivery_future = delivery.deliver(Transcript(selected.clone()));
    // This metric ends when Delivery is initiated, not after clipboard/libei
    // I/O. The explicit reserve is for entering Delivery before the one-second
    // candidate deadline; the adapter owns its existing I/O bound afterwards.
    diagnostic.total_gate_latency_ms = Some(duration_millis(gate_entry.elapsed()));
    let delivery_outcome = delivery_future.await;
    FinalTransformCompletion {
        rendered: selected,
        delivery: delivery_outcome,
        diagnostic,
    }
}

fn map_adapter_error(error: &MinimalGrammarError) -> SmartWritingReasonCode {
    use crate::grammar_http::GrammarHttpError;
    match error {
        MinimalGrammarError::Transport(GrammarHttpError::Timeout)
        | MinimalGrammarError::ResultCutoff => SmartWritingReasonCode::HttpTimeout,
        MinimalGrammarError::Transport(GrammarHttpError::NonSuccessStatus { .. }) => {
            SmartWritingReasonCode::HttpStatus
        }
        MinimalGrammarError::Transport(GrammarHttpError::BodyTooLarge { .. }) => {
            SmartWritingReasonCode::ResponseOversize
        }
        MinimalGrammarError::Transport(_)
        | MinimalGrammarError::InvalidBaselineIdentity => SmartWritingReasonCode::HttpTransport,
        MinimalGrammarError::InvalidProviderEnvelope => SmartWritingReasonCode::Schema,
    }
}

fn map_grammar_error(error: GrammarErrorCode) -> SmartWritingReasonCode {
    match error {
        GrammarErrorCode::Oversize => SmartWritingReasonCode::ResponseOversize,
        GrammarErrorCode::Malformed
        | GrammarErrorCode::Unsorted
        | GrammarErrorCode::SpanOutOfBounds
        | GrammarErrorCode::SpanNotCharBoundary
        | GrammarErrorCode::NotTokenBoundary
        | GrammarErrorCode::AnchorMismatch
        | GrammarErrorCode::FormattingIdentity
        | GrammarErrorCode::FormattingDerivation => SmartWritingReasonCode::Malformed,
        GrammarErrorCode::StaleGrammar => SmartWritingReasonCode::Stale,
        GrammarErrorCode::ProtectedSpan => SmartWritingReasonCode::ProtectedSpan,
        GrammarErrorCode::UnknownRule => SmartWritingReasonCode::UnknownRule,
        GrammarErrorCode::RuleContext => SmartWritingReasonCode::RuleContext,
        GrammarErrorCode::Unmappable => SmartWritingReasonCode::Unmappable,
        GrammarErrorCode::Overlap => SmartWritingReasonCode::Overlap,
    }
}

fn edit_evidence_from_candidate(
    candidate: &[u8],
    result: &voisu_core::GrammarSafetyResult,
    fallback_code: SmartWritingReasonCode,
) -> Vec<SmartWritingEditEvidence> {
    if candidate.len() > MAX_GRAMMAR_RESPONSE_BYTES {
        return Vec::new();
    }
    let Ok(root) = serde_json::from_slice::<serde_json::Value>(candidate) else {
        return Vec::new();
    };
    let Some(edits) = root.get("edits").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };

    edits
        .iter()
        .filter_map(|edit| {
            let object = edit.as_object()?;
            let edit_id = object.get("id")?.as_str()?;
            let rule_id = object.get("rule_id")?.as_str()?;
            let start_utf8 = object.get("start_utf8")?.as_u64()?;
            let end_utf8 = object.get("end_utf8")?.as_u64()?;
            let before = object.get("before")?.as_str()?;
            let after = object.get("after")?.as_str()?;
            let code = result
                .diagnostics
                .iter()
                .find(|diagnostic| diagnostic.edit_id == edit_id)
                .map(|diagnostic| map_grammar_error(diagnostic.code))
                .unwrap_or(fallback_code);
            Some(SmartWritingEditEvidence::new(
                edit_id, rule_id, start_utf8, end_utf8, before, after, code,
            ))
        })
        .collect()
}

fn push_unique_reason(
    reasons: &mut Vec<SmartWritingReasonCode>,
    reason: SmartWritingReasonCode,
) {
    if !reasons.contains(&reason) {
        reasons.push(reason);
    }
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio::time::{sleep, timeout};
    use voisu_core::{BoundaryFuture, BoundaryKind};

    struct RecordingDelivery {
        calls: Arc<AtomicUsize>,
        delivered: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl DeliveryAdapter for RecordingDelivery {
        fn deliver(&mut self, transcript: Transcript) -> BoundaryFuture<'_, DeliveryOutcome> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.delivered
                .lock()
                .expect("delivery lock")
                .push(transcript.0);
            Box::pin(async { Ok(DeliveryOutcome::clipboard_fallback("controlled")) })
        }
    }

    struct CannedGrammar(Result<Vec<u8>, MinimalGrammarError>);

    impl GrammarRequestBoundary for CannedGrammar {
        fn request_candidate<'a>(
            &'a self,
            _bearer_token: &'a str,
            _validated_transcript: &'a str,
            _baseline: &'a voisu_core::FormattingBaseline,
            _gate_entry: Instant,
        ) -> GrammarBoundaryFuture<'a> {
            Box::pin(async { self.0.clone() })
        }
    }

    fn delivery() -> (RecordingDelivery, Arc<AtomicUsize>, Arc<std::sync::Mutex<Vec<String>>>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let delivered = Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            RecordingDelivery {
                calls: Arc::clone(&calls),
                delivered: Arc::clone(&delivered),
            },
            calls,
            delivered,
        )
    }

    fn english() -> ResolvedRecordingLanguages {
        ResolvedRecordingLanguages::new(vec![
            (Provider::Groq, "en".to_owned()),
            (Provider::Deepgram, "en-US".to_owned()),
        ])
    }

    #[test]
    fn english_resolution_is_declaration_only_and_fail_closed() {
        assert_eq!(english().english_eligibility(), EnglishEligibilityOutcome::Eligible);
        for declarations in [
            Vec::new(),
            vec![(Provider::Groq, String::new())],
            vec![(Provider::Groq, "auto".to_owned())],
            vec![(Provider::Groq, "fr".to_owned())],
            vec![(Provider::Groq, "en-".to_owned())],
        ] {
            assert_eq!(
                ResolvedRecordingLanguages::new(declarations).english_eligibility(),
                EnglishEligibilityOutcome::Ineligible
            );
        }
    }

    #[test]
    fn gate_constants_match_the_release_manifest() {
        let companion: serde_json::Value = serde_json::from_str(include_str!(
            "../../../docs/research/smart-writing-spec-constants-2026-08-09.json"
        ))
        .expect("constants companion");
        assert_eq!(
            companion["timing"]["FINAL_TRANSFORM_GATE_DEADLINE"],
            duration_millis(FINAL_TRANSFORM_GATE_DEADLINE)
        );
        assert_eq!(
            companion["timing"]["DELIVERY_INITIATION_RESERVE"],
            duration_millis(DELIVERY_INITIATION_RESERVE)
        );
        assert_eq!(
            companion["timing"]["CANDIDATE_PIPELINE_DEADLINE"],
            duration_millis(CANDIDATE_PIPELINE_DEADLINE)
        );
        assert_eq!(
            companion["timing"]["LOCAL_SAFETY_COMPOSE_WORK_DEADLINE"],
            duration_millis(LOCAL_SAFETY_COMPOSE_WORK_DEADLINE)
        );
    }

    #[tokio::test]
    async fn literal_commands_format_and_deliver_once_without_grammar() {
        let (mut delivery, calls, delivered) = delivery();
        let completion = final_transform_and_deliver(
            FinalTransformInput {
                validated_transcript: "hello command new line world",
                writing_mode: WritingMode::Literal,
                languages: &english(),
                grammar: GrammarGateCapability::Unavailable,
                dictionary_terms: &[],
                protected_names: &[],
                credential: CredentialGateEvidence::default(),
            },
            &mut delivery,
        )
        .await
        ;
        assert_eq!(completion.rendered, "hello\nworld");
        assert_eq!(completion.diagnostic.outcome, SmartWritingOutcome::LiteralCommands);
        assert!(!completion.diagnostic.request_began);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(delivered.lock().expect("delivered").as_slice(), ["hello\nworld"]);
    }

    #[tokio::test]
    async fn smart_candidate_composes_and_formatting_wins_before_delivery() {
        let text = "there is two issues";
        let baseline = format_validated_for_grammar(text, FormatOptions::default());
        let candidate = serde_json::json!({
            "base_version": baseline.base_version(),
            "base_fingerprint": baseline.base_fingerprint(),
            "edits": [{
                "id": "plural",
                "rule_id": "G_THERE_IS_PLURAL_QUANTITY",
                "start_utf8": 6,
                "end_utf8": 8,
                "before": "is",
                "after": "are"
            }]
        })
        .to_string()
        .into_bytes();
        let grammar = CannedGrammar(Ok(candidate));
        let (mut delivery, calls, delivered) = delivery();
        let completion = final_transform_and_deliver(
            FinalTransformInput {
                validated_transcript: text,
                writing_mode: WritingMode::Smart,
                languages: &english(),
                grammar: GrammarGateCapability::Ready {
                    boundary: &grammar,
                    bearer_token: "ready",
                },
                dictionary_terms: &[],
                protected_names: &[],
                credential: CredentialGateEvidence::default(),
            },
            &mut delivery,
        )
        .await
        ;
        assert_eq!(completion.rendered, "There are two issues.");
        assert_eq!(completion.diagnostic.outcome, SmartWritingOutcome::FormattingAndGrammar);
        assert!(completion.diagnostic.request_began);
        assert_eq!(completion.diagnostic.edits.len(), 1);
        assert_eq!(
            completion.diagnostic.edits[0].code,
            SmartWritingReasonCode::EditAccepted
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(delivered.lock().expect("delivered").as_slice(), ["There are two issues."]);
    }

    #[tokio::test]
    async fn grammar_error_preserves_exact_formatting_baseline_and_delivers_once() {
        let grammar = CannedGrammar(Err(MinimalGrammarError::InvalidProviderEnvelope));
        let (mut delivery, calls, delivered) = delivery();
        let completion = final_transform_and_deliver(
            FinalTransformInput {
                validated_transcript: "hello world",
                writing_mode: WritingMode::Smart,
                languages: &english(),
                grammar: GrammarGateCapability::Ready {
                    boundary: &grammar,
                    bearer_token: "ready",
                },
                dictionary_terms: &[],
                protected_names: &[],
                credential: CredentialGateEvidence::default(),
            },
            &mut delivery,
        )
        .await
        ;
        assert_eq!(completion.rendered, "Hello world.");
        assert_eq!(completion.diagnostic.outcome, SmartWritingOutcome::FormattingOnly);
        assert!(completion.diagnostic.reason_codes.contains(&SmartWritingReasonCode::Schema));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(delivered.lock().expect("delivered").as_slice(), ["Hello world."]);
    }

    #[tokio::test]
    async fn commands_and_non_english_skip_grammar_but_keep_formatting_delivery() {
        for (text, languages) in [
            ("hello command new line world", english()),
            (
                "hello world",
                ResolvedRecordingLanguages::new(vec![(Provider::Groq, "fr".to_owned())]),
            ),
        ] {
            let grammar = CannedGrammar(Err(MinimalGrammarError::Transport(
                crate::grammar_http::GrammarHttpError::Transport,
            )));
            let (mut delivery, calls, _) = delivery();
            let completion = final_transform_and_deliver(
                FinalTransformInput {
                    validated_transcript: text,
                    writing_mode: WritingMode::Smart,
                    languages: &languages,
                    grammar: GrammarGateCapability::Ready {
                        boundary: &grammar,
                        bearer_token: "ready",
                    },
                    dictionary_terms: &[],
                    protected_names: &[],
                    credential: CredentialGateEvidence::default(),
                },
                &mut delivery,
            )
            .await;
            assert!(!completion.diagnostic.request_began);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn dropping_wired_gate_cancels_real_http_before_delivery() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let (entered_tx, entered_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut request = Vec::new();
            let mut chunk = [0_u8; 2048];
            let mut expected = None;
            loop {
                let read = socket.read(&mut chunk).await.expect("read request");
                assert!(read > 0, "client closed before request completed");
                request.extend_from_slice(&chunk[..read]);
                if expected.is_none()
                    && let Some(header_end) = request.windows(4).position(|w| w == b"\r\n\r\n")
                {
                    let header_end = header_end + 4;
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(str::trim)
                                .and_then(|value| value.parse::<usize>().ok())
                        })
                        .expect("content length");
                    expected = Some(header_end + length);
                }
                if expected.is_some_and(|length| request.len() >= length) {
                    break;
                }
            }
            let _ = entered_tx.send(());
            let read = timeout(Duration::from_secs(1), socket.read(&mut chunk))
                .await
                .expect("client cancellation deadline")
                .expect("read cancellation");
            assert_eq!(read, 0, "gate drop must close the request socket");
            let _ = socket.shutdown().await;
        });

        let client = crate::grammar_http::GrammarHttpClient::with_endpoint(format!(
            "http://{address}/openai/v1/chat/completions"
        ))
        .expect("client");
        let grammar = MinimalGrammarAdapter::new(client);
        let languages = english();
        let (mut delivery, calls, _) = delivery();
        let mut gate = Box::pin(final_transform_and_deliver(
            FinalTransformInput {
                validated_transcript: "there is two issues",
                writing_mode: WritingMode::Smart,
                languages: &languages,
                grammar: GrammarGateCapability::Ready {
                    boundary: &grammar,
                    bearer_token: "ready",
                },
                dictionary_terms: &[],
                protected_names: &[],
                credential: CredentialGateEvidence::default(),
            },
            &mut delivery,
        ));
        tokio::select! {
            entered = entered_rx => entered.expect("request entered"),
            _ = &mut gate => panic!("gate completed while server was hanging"),
        }
        drop(gate);
        timeout(Duration::from_secs(2), server)
            .await
            .expect("server join deadline")
            .expect("server task");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn delivery_error_is_not_retried() {
        struct FailingDelivery(Arc<AtomicUsize>);
        impl DeliveryAdapter for FailingDelivery {
            fn deliver(&mut self, _transcript: Transcript) -> BoundaryFuture<'_, DeliveryOutcome> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Box::pin(async {
                    Err(BoundaryError::new(
                        BoundaryKind::Delivery,
                        "controlled delivery failure",
                    ))
                })
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let mut delivery = FailingDelivery(Arc::clone(&calls));
        assert!(final_transform_and_deliver(
            FinalTransformInput {
                validated_transcript: "hello",
                writing_mode: WritingMode::Smart,
                languages: &english(),
                grammar: GrammarGateCapability::Unavailable,
                dictionary_terms: &[],
                protected_names: &[],
                credential: CredentialGateEvidence::default(),
            },
            &mut delivery,
        )
        .await
        .delivery
        .is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn gate_latency_stops_when_delivery_is_initiated() {
        struct SlowDelivery;

        impl DeliveryAdapter for SlowDelivery {
            fn deliver(&mut self, _transcript: Transcript) -> BoundaryFuture<'_, DeliveryOutcome> {
                Box::pin(async {
                    sleep(Duration::from_millis(150)).await;
                    Ok(DeliveryOutcome::clipboard_fallback("slow delivery"))
                })
            }
        }

        let mut delivery = SlowDelivery;
        let completion = final_transform_and_deliver(
            FinalTransformInput {
                validated_transcript: "hello world",
                writing_mode: WritingMode::Smart,
                languages: &english(),
                grammar: GrammarGateCapability::Unavailable,
                dictionary_terms: &[],
                protected_names: &[],
                credential: CredentialGateEvidence::default(),
            },
            &mut delivery,
        )
        .await;

        let gate_latency = completion
            .diagnostic
            .total_gate_latency_ms
            .expect("gate latency");
        assert!(
            gate_latency < DELIVERY_INITIATION_RESERVE.as_millis() as u64,
            "Delivery I/O must not be included in gate latency: {gate_latency}ms"
        );
        assert!(completion.delivery.is_ok());
    }
}
