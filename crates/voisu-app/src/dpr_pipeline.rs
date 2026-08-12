//! Flagged Developer Prompt Rendering orchestration.
//!
//! The module owns one Final Transcript decision from a snapshotted policy and
//! selected Source Transcript through the existing compose and Delivery gates.
//! It has no API for replacing text after Delivery.

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use voisu_core::{
    compose_structured_candidate, organize_local_baseline, route_intent,
    text_sha256_fingerprint, CloudOutcome, CloudRequest, ComposeInput, ComposeSource,
    CompositionDecision, Credential, DeliveryAdapter, DeliveryFlags, DeliveryOutcome,
    IntentObservation, LocalBaselineOptions, ProcessHint, ProviderState, RenderingPolicy,
    RoutingDecision, SourceSelection, SourceTranscript, SttProvider, SurfaceHint, TimingHint,
    Transcript, TranscriptSelection, Provider,
    MAX_COMPOSE_FIELD_UTF8_BYTES,
};

use crate::dpr_cloud::{
    DprCloudAttempt, DprCloudClient, DprCloudErrorClass, DprCloudRequest,
    MAX_DPR_PROTECTED_TOKENS,
};

/// Maximum elapsed time from utterance end to initiating Delivery.
pub const DPR_DELIVERY_DEADLINE: Duration = Duration::from_millis(1_500);

pub type DprCloudFuture<'a> =
    Pin<Box<dyn Future<Output = DprCloudAttempt> + Send + 'a>>;

/// One-attempt cloud seam. Production uses [`DprCloudClient`]; tests inject a
/// counting boundary without networking or wall-clock sleeps.
pub trait DprCloudBoundary: Send + Sync {
    fn attempt<'a>(
        &'a self,
        credential: &'a Credential,
        request: DprCloudRequest<'a>,
        remaining: Duration,
    ) -> DprCloudFuture<'a>;
}

impl DprCloudBoundary for DprCloudClient {
    fn attempt<'a>(
        &'a self,
        credential: &'a Credential,
        request: DprCloudRequest<'a>,
        remaining: Duration,
    ) -> DprCloudFuture<'a> {
        Box::pin(async move { self.attempt_groq(credential, &request, remaining).await })
    }
}

/// Monotonic elapsed time from the Recording's utterance-end snapshot.
pub trait DprPipelineClock: Send + Sync {
    fn elapsed(&self) -> Duration;
}

pub struct SystemDprPipelineClock {
    utterance_end: Instant,
}

impl SystemDprPipelineClock {
    #[must_use]
    pub const fn from_utterance_end(utterance_end: Instant) -> Self {
        Self { utterance_end }
    }
}

impl DprPipelineClock for SystemDprPipelineClock {
    fn elapsed(&self) -> Duration {
        self.utterance_end.elapsed()
    }
}

pub enum DprCloudCapability<'a> {
    Ready {
        boundary: &'a dyn DprCloudBoundary,
        credential: &'a Credential,
    },
    Unavailable,
}

pub struct DprTransformInput<'a> {
    pub selected_source: &'a str,
    pub sources: &'a [ComposeSource],
    pub source_selection: &'a SourceSelection,
    pub provider_state: ProviderState,
    pub policy: RenderingPolicy,
    pub english_eligible: bool,
    pub surface_hint: Option<SurfaceHint>,
    pub process_hint: Option<ProcessHint>,
    pub timing: Option<TimingHint>,
    pub protected_tokens: &'a [&'a str],
    pub cloud: DprCloudCapability<'a>,
    pub clock: &'a dyn DprPipelineClock,
}

pub struct DprTransformCompletion {
    pub rendered: String,
    pub delivery: Result<DeliveryOutcome, voisu_core::BoundaryError>,
    pub delivery_flags: DeliveryFlags,
    pub routing: RoutingDecision,
    pub compose_decision: CompositionDecision,
    pub cloud_attempted: bool,
    pub cloud_error: Option<DprCloudErrorClass>,
}

/// Owned adapter from the daemon's Source Transcripts into the generic
/// provider-a/provider-b compose contract. Selection is deliberately local:
/// DPR must not spend a legacy free-form reconciliation call before its one
/// permitted structured cloud attempt.
pub struct DprSourceContext {
    pub selected_source: String,
    pub sources: Vec<ComposeSource>,
    pub source_selection: SourceSelection,
    pub provider_state: ProviderState,
    pub transcript_selection: TranscriptSelection,
}

#[must_use]
pub fn dpr_source_context(sources: &[SourceTranscript]) -> Option<DprSourceContext> {
    let available: Vec<&SourceTranscript> = sources
        .iter()
        .filter(|source| !source.text.is_empty())
        .collect();
    let selected = available
        .iter()
        .find(|source| source.provider == Provider::Groq)
        .copied()
        .or_else(|| available.first().copied())?;
    let selected_provider = selected.provider;
    let exact_agreement = available.len() > 1
        && available
            .iter()
            .all(|source| source.text == available[0].text);
    let punctuation_only_agreement = available.len() > 1
        && !exact_agreement
        && available
            .iter()
            .all(|source| normalized_source_words(&source.text) == normalized_source_words(&available[0].text));
    let provider_state = if available.len() <= 1 {
        ProviderState::SingleProvider
    } else if exact_agreement {
        ProviderState::ExactAgreement
    } else if punctuation_only_agreement {
        ProviderState::PunctuationOnlyAgreement
    } else {
        ProviderState::SemanticDisagreement
    };
    let reason = if available.len() <= 1 {
        "only_available"
    } else if exact_agreement {
        "exact_agreement"
    } else if punctuation_only_agreement {
        "punctuation_local_render"
    } else {
        "configured_primary_rank"
    };

    let mut compose_sources = Vec::with_capacity(available.len().min(2));
    compose_sources.push(ComposeSource {
        provider: SttProvider::ProviderA,
        available: true,
        text: selected.text.clone(),
        primary: true,
    });
    if let Some(other) = available
        .iter()
        .find(|source| source.provider != selected_provider)
    {
        compose_sources.push(ComposeSource {
            provider: SttProvider::ProviderB,
            available: true,
            text: other.text.clone(),
            primary: false,
        });
    }

    Some(DprSourceContext {
        selected_source: selected.text.clone(),
        sources: compose_sources,
        source_selection: SourceSelection {
            selected_provider: SttProvider::ProviderA,
            reason: reason.to_owned(),
        },
        provider_state,
        transcript_selection: match selected_provider {
            Provider::Groq if punctuation_only_agreement => TranscriptSelection::NearIdenticalGroq,
            Provider::Groq => TranscriptSelection::SourceGroq,
            Provider::Deepgram => TranscriptSelection::SourceDeepgram,
        },
    })
}

fn normalized_source_words(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric() && character != '\'')
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Extract closed, host-verifiable protected atoms from the selected source.
/// The model never decides this list. Ambiguous natural-language entities stay
/// untouched by this conservative recognizer; exact dictionary terms and
/// mechanically recognizable technical/semantic atoms fail closed.
#[must_use]
pub fn dpr_protected_tokens(selected_source: &str, dictionary_terms: &[String]) -> Vec<String> {
    let mut protected = Vec::new();
    for raw in selected_source.split_whitespace() {
        let token = raw.trim_matches(|character: char| {
            matches!(character, ',' | ';' | '!' | '?' | '"' | '\'' | '`')
        });
        let lower = token.to_ascii_lowercase();
        let negation = matches!(
            lower.as_str(),
            "no"
                | "not"
                | "never"
                | "cannot"
                | "can't"
                | "cant"
                | "don't"
                | "dont"
                | "won't"
                | "wont"
                | "isn't"
                | "isnt"
                | "aren't"
                | "arent"
                | "ain't"
                | "aint"
        );
        let technical = token.bytes().any(|byte| byte.is_ascii_digit())
            || token.contains("://")
            || token.contains('/')
            || token.contains('\\')
            || token.starts_with('-')
            || token.contains('_')
            || token.contains("::")
            || token.contains(['(', ')', '[', ']', '{', '}', '=', '@']);
        if negation || technical {
            push_protected(&mut protected, selected_source, token);
        }
    }

    let lower = selected_source.to_ascii_lowercase();
    let mut search_from = 0;
    while let Some(open_relative) = lower[search_from..].find("quote ") {
        let interior_start = search_from + open_relative + "quote ".len();
        let Some(close_relative) = lower[interior_start..].find(" unquote") else {
            break;
        };
        let interior_end = interior_start + close_relative;
        push_protected(
            &mut protected,
            selected_source,
            &selected_source[interior_start..interior_end],
        );
        search_from = interior_end + " unquote".len();
    }

    // Dictionary entries are important spelling/name evidence, but cannot
    // crowd closed technical or semantic atoms out of the bounded request.
    for term in dictionary_terms {
        push_protected(&mut protected, selected_source, term);
    }

    protected
}

fn push_protected(protected: &mut Vec<String>, source: &str, token: &str) {
    if protected.len() >= MAX_DPR_PROTECTED_TOKENS
        || token.is_empty()
        || token.len() > MAX_COMPOSE_FIELD_UTF8_BYTES
        || !source.contains(token)
        || protected.iter().any(|existing| existing == token)
    {
        return;
    }
    protected.push(token.to_owned());
}

/// Build the safe Final Transcript and initiate Delivery exactly once.
pub async fn dpr_transform_and_deliver(
    input: DprTransformInput<'_>,
    delivery: &mut dyn DeliveryAdapter,
) -> DprTransformCompletion {
    let routing = route_intent(&IntentObservation {
        policy: input.policy,
        primary_text: input.selected_source.to_owned(),
        provider_state: input.provider_state,
        surface_hint: input.surface_hint,
        process_hint: input.process_hint,
        timing: input.timing,
    });
    let baseline = organize_local_baseline(
        input.selected_source,
        &LocalBaselineOptions {
            policy: input.policy,
            route: routing.route,
            timing: None,
        },
    );
    let base_fingerprint = text_sha256_fingerprint(input.selected_source);
    let mut cloud_attempted = false;
    let mut cloud_error = None;
    let mut candidate = None;
    let cloud_outcome = if !input.english_eligible
        || routing.cloud_request == CloudRequest::NotAllowed
    {
        CloudOutcome::Skipped
    } else {
        let cloud_budget_elapsed = input.clock.elapsed();
        if cloud_budget_elapsed >= DPR_DELIVERY_DEADLINE {
            cloud_error = Some(DprCloudErrorClass::DeadlineExceeded);
            CloudOutcome::DeadlineExceeded
        } else {
            match input.cloud {
                DprCloudCapability::Unavailable => {
                    cloud_error = Some(DprCloudErrorClass::CredentialUnavailable);
                    CloudOutcome::ProviderFailure
                }
                DprCloudCapability::Ready {
                    boundary,
                    credential,
                } => {
                    cloud_attempted = true;
                    let remaining = DPR_DELIVERY_DEADLINE - cloud_budget_elapsed;
                    let attempt = boundary
                        .attempt(
                            credential,
                            DprCloudRequest {
                                sources: input.sources,
                                source_selection: input.source_selection,
                                base_fingerprint: &base_fingerprint,
                                policy: input.policy,
                                protected_tokens: input.protected_tokens,
                            },
                            remaining,
                        )
                        .await;
                    let (attempt_candidate, attempt_error) = attempt.into_parts();
                    if input.clock.elapsed() >= DPR_DELIVERY_DEADLINE {
                        cloud_error = Some(DprCloudErrorClass::DeadlineExceeded);
                        CloudOutcome::DeadlineExceeded
                    } else if let Some(accepted_candidate) = attempt_candidate {
                        candidate = Some(accepted_candidate);
                        CloudOutcome::Succeeded
                    } else {
                        let error = attempt_error.unwrap_or(DprCloudErrorClass::ProviderEnvelope);
                        cloud_error = Some(error);
                        cloud_outcome_for_error(error)
                    }
                }
            }
        }
    };
    let mut composed = compose_structured_candidate(&ComposeInput {
        local_baseline: &baseline,
        base_fingerprint: &base_fingerprint,
        sources: input.sources,
        source_selection: input.source_selection,
        protected_tokens: input.protected_tokens,
        policy: input.policy,
        cloud_outcome,
        candidate: candidate.as_ref(),
    });
    if cloud_outcome == CloudOutcome::Succeeded
        && input.clock.elapsed() >= DPR_DELIVERY_DEADLINE
    {
        cloud_error = Some(DprCloudErrorClass::DeadlineExceeded);
        composed = compose_structured_candidate(&ComposeInput {
            local_baseline: &baseline,
            base_fingerprint: &base_fingerprint,
            sources: input.sources,
            source_selection: input.source_selection,
            protected_tokens: input.protected_tokens,
            policy: input.policy,
            cloud_outcome: CloudOutcome::DeadlineExceeded,
            candidate: None,
        });
    }
    let rendered = composed.rendered().to_owned();
    let delivery_flags = composed.delivery();
    let compose_decision = composed.decision();
    let delivery = delivery.deliver(Transcript(rendered.clone())).await;
    DprTransformCompletion {
        rendered,
        delivery,
        delivery_flags,
        routing,
        compose_decision,
        cloud_attempted,
        cloud_error,
    }
}

fn cloud_outcome_for_error(error: DprCloudErrorClass) -> CloudOutcome {
    match error {
        DprCloudErrorClass::DeadlineExceeded => CloudOutcome::DeadlineExceeded,
        DprCloudErrorClass::RequestInvalid
        | DprCloudErrorClass::ResponseTooLarge
        | DprCloudErrorClass::ProviderEnvelope
        | DprCloudErrorClass::CandidateSchema => CloudOutcome::SchemaFailure,
        DprCloudErrorClass::CredentialUnavailable
        | DprCloudErrorClass::HttpClient
        | DprCloudErrorClass::Http4xx
        | DprCloudErrorClass::RateLimited
        | DprCloudErrorClass::Http5xx
        | DprCloudErrorClass::Transport => CloudOutcome::ProviderFailure,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use voisu_core::{
        parse_structured_candidate_json, text_sha256_fingerprint, BoundaryFuture, SttProvider,
        StructuredCandidate, Transcript,
    };

    use super::*;

    struct FixedClock(Duration);

    impl DprPipelineClock for FixedClock {
        fn elapsed(&self) -> Duration {
            self.0
        }
    }

    struct RecordingDelivery {
        calls: Arc<AtomicUsize>,
        rendered: Arc<Mutex<Vec<String>>>,
        initiated_ms: Option<(ControlledClock, Arc<AtomicU64>)>,
    }

    impl DeliveryAdapter for RecordingDelivery {
        fn deliver(&mut self, transcript: Transcript) -> BoundaryFuture<'_, DeliveryOutcome> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some((clock, initiated_ms)) = &self.initiated_ms {
                initiated_ms.store(
                    u64::try_from(clock.elapsed().as_millis()).unwrap_or(u64::MAX),
                    Ordering::SeqCst,
                );
            }
            self.rendered
                .lock()
                .expect("delivery lock")
                .push(transcript.0);
            Box::pin(async { Ok(DeliveryOutcome::compositor_submitted()) })
        }
    }

    struct ForbiddenCloud;

    impl DprCloudBoundary for ForbiddenCloud {
        fn attempt<'a>(
            &'a self,
            _credential: &'a Credential,
            _request: DprCloudRequest<'a>,
            _remaining: Duration,
        ) -> DprCloudFuture<'a> {
            panic!("Natural policy must never start cloud")
        }
    }

    #[derive(Clone)]
    struct ControlledClock(Arc<AtomicU64>);

    impl ControlledClock {
        fn new(elapsed_ms: u64) -> Self {
            Self(Arc::new(AtomicU64::new(elapsed_ms)))
        }
    }

    impl DprPipelineClock for ControlledClock {
        fn elapsed(&self) -> Duration {
            Duration::from_millis(self.0.load(Ordering::SeqCst))
        }
    }

    #[derive(Clone)]
    enum CannedCloudOutcome {
        Success(StructuredCandidate),
        Failure(DprCloudErrorClass),
    }

    struct CountingCloud {
        calls: Arc<AtomicUsize>,
        remaining_ms: Arc<AtomicU64>,
        clock: ControlledClock,
        completes_at_ms: u64,
        outcome: CannedCloudOutcome,
    }

    struct SequenceClock {
        calls: AtomicUsize,
        elapsed_ms: Vec<u64>,
    }

    impl DprPipelineClock for SequenceClock {
        fn elapsed(&self) -> Duration {
            let index = self.calls.fetch_add(1, Ordering::SeqCst);
            Duration::from_millis(
                *self
                    .elapsed_ms
                    .get(index)
                    .unwrap_or_else(|| self.elapsed_ms.last().expect("clock sequence")),
            )
        }
    }

    struct ImmediateCandidateCloud {
        calls: Arc<AtomicUsize>,
        candidate: StructuredCandidate,
    }

    impl DprCloudBoundary for ImmediateCandidateCloud {
        fn attempt<'a>(
            &'a self,
            _credential: &'a Credential,
            _request: DprCloudRequest<'a>,
            _remaining: Duration,
        ) -> DprCloudFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let candidate = self.candidate.clone();
            Box::pin(async move { DprCloudAttempt::success(candidate) })
        }
    }

    impl DprCloudBoundary for CountingCloud {
        fn attempt<'a>(
            &'a self,
            _credential: &'a Credential,
            _request: DprCloudRequest<'a>,
            remaining: Duration,
        ) -> DprCloudFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.remaining_ms.store(
                u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX),
                Ordering::SeqCst,
            );
            let current_ms = self.clock.0.load(Ordering::SeqCst);
            let deadline_ms = current_ms.saturating_add(
                u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX),
            );
            let result = if self.completes_at_ms > deadline_ms {
                self.clock.0.store(deadline_ms, Ordering::SeqCst);
                DprCloudAttempt::failure(DprCloudErrorClass::DeadlineExceeded)
            } else {
                self.clock.0.store(self.completes_at_ms, Ordering::SeqCst);
                match &self.outcome {
                    CannedCloudOutcome::Success(candidate) => {
                        DprCloudAttempt::success(candidate.clone())
                    }
                    CannedCloudOutcome::Failure(error) => DprCloudAttempt::failure(*error),
                }
            };
            Box::pin(async move { result })
        }
    }

    fn accepted_candidate(source: &str, output: &str, reason: &str) -> StructuredCandidate {
        let raw = serde_json::json!({
            "schema_version": "1",
            "base_fingerprint": text_sha256_fingerprint(source),
            "reconciliation": {"selected_provider": "provider_a", "reason": reason},
            "removals": [],
            "conversions": [],
            "layout": {"decision": "natural", "certainty": "clear"},
            "labels": [],
            "derivation": [{
                "kind": "keep",
                "source_provider": "provider_a",
                "source_text": source,
                "output_text": output,
                "conversion_id": null,
                "label": null
            }]
        });
        parse_structured_candidate_json(raw.to_string().as_bytes()).expect("candidate")
    }

    #[test]
    fn daemon_source_adapter_selects_a_real_source_without_model_reconciliation() {
        let sources = vec![
            SourceTranscript {
                provider: Provider::Deepgram,
                text: "hello from voice you".to_owned(),
            },
            SourceTranscript {
                provider: Provider::Groq,
                text: "hello from voisu".to_owned(),
            },
        ];
        let context = dpr_source_context(&sources).expect("source context");
        assert_eq!(context.selected_source, "hello from voisu");
        assert_eq!(context.sources[0].provider, SttProvider::ProviderA);
        assert_eq!(context.sources[0].text, "hello from voisu");
        assert!(context.sources[0].primary);
        assert_eq!(context.sources[1].provider, SttProvider::ProviderB);
        assert_eq!(context.provider_state, ProviderState::SemanticDisagreement);
        assert_eq!(context.source_selection.reason, "configured_primary_rank");
        assert_eq!(context.transcript_selection, TranscriptSelection::SourceGroq);
        assert!(dpr_source_context(&[]).is_none());
    }

    #[test]
    fn protected_tokens_are_host_derived_for_dictionary_and_closed_sensitive_families() {
        let source = "Do not run cargo test --workspace on 2026-08-16; open https://example.test/a and crates/voisu_core/src/lib.rs; exact quote connection refused unquote";
        let protected = dpr_protected_tokens(source, &["connection refused".to_owned()]);

        for expected in [
            "not",
            "--workspace",
            "2026-08-16",
            "https://example.test/a",
            "crates/voisu_core/src/lib.rs",
            "connection refused",
        ] {
            assert!(
                protected.iter().any(|token| token == expected),
                "missing {expected}: {protected:?}"
            );
        }
        assert!(!protected.iter().any(|token| token == "cargo"));
        assert!(protected.len() <= MAX_DPR_PROTECTED_TOKENS);
    }

    #[tokio::test]
    async fn natural_simple_delivers_local_baseline_once_without_cloud() {
        let calls = Arc::new(AtomicUsize::new(0));
        let rendered = Arc::new(Mutex::new(Vec::new()));
        let mut delivery = RecordingDelivery {
            calls: Arc::clone(&calls),
            rendered: Arc::clone(&rendered),
            initiated_ms: None,
        };
        let source = "hello from voisu";
        let sources = [ComposeSource {
            provider: SttProvider::ProviderA,
            available: true,
            text: source.to_owned(),
            primary: true,
        }];
        let selection = SourceSelection {
            selected_provider: SttProvider::ProviderA,
            reason: "only_available".to_owned(),
        };
        let credential = Credential::new("controlled-secret".to_owned()).expect("credential");
        let cloud = ForbiddenCloud;
        let clock = FixedClock(Duration::ZERO);

        let completion = dpr_transform_and_deliver(
            DprTransformInput {
                selected_source: source,
                sources: &sources,
                source_selection: &selection,
                provider_state: ProviderState::SingleProvider,
                policy: RenderingPolicy::Natural,
                english_eligible: true,
                surface_hint: None,
                process_hint: None,
                timing: None,
                protected_tokens: &[],
                cloud: DprCloudCapability::Ready {
                    boundary: &cloud,
                    credential: &credential,
                },
                clock: &clock,
            },
            &mut delivery,
        )
        .await;

        assert!(!completion.cloud_attempted);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(rendered.lock().expect("delivery lock").as_slice(), ["Hello from voisu."]);
        assert_eq!(completion.rendered, "Hello from voisu.");
        assert_eq!(completion.delivery_flags.state, "unsent");
        assert!(!completion.delivery_flags.auto_send);
        assert!(!completion.delivery_flags.live_type);
        assert!(!completion.delivery_flags.replace_delivered);
        assert!(completion.delivery.is_ok());
        assert_eq!(completion.routing.cloud_request, CloudRequest::NotAllowed);
        assert_eq!(
            completion.compose_decision,
            CompositionDecision::FallbackBaseline
        );
    }

    #[tokio::test]
    async fn adaptive_dispute_accepts_fast_structured_candidate_with_one_cloud_call() {
        let delivery_calls = Arc::new(AtomicUsize::new(0));
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let mut delivery = RecordingDelivery {
            calls: Arc::clone(&delivery_calls),
            rendered: Arc::clone(&delivered),
            initiated_ms: None,
        };
        let source = "hello from voisu";
        let sources = [
            ComposeSource {
                provider: SttProvider::ProviderA,
                available: true,
                text: source.to_owned(),
                primary: true,
            },
            ComposeSource {
                provider: SttProvider::ProviderB,
                available: true,
                text: "hello from voice you".to_owned(),
                primary: false,
            },
        ];
        let selection = SourceSelection {
            selected_provider: SttProvider::ProviderA,
            reason: "configured_primary_rank".to_owned(),
        };
        let clock = ControlledClock::new(100);
        let cloud_calls = Arc::new(AtomicUsize::new(0));
        let remaining_ms = Arc::new(AtomicU64::new(0));
        let cloud = CountingCloud {
            calls: Arc::clone(&cloud_calls),
            remaining_ms: Arc::clone(&remaining_ms),
            clock: clock.clone(),
            completes_at_ms: 700,
            outcome: CannedCloudOutcome::Success(accepted_candidate(
                source,
                "Hello from voisu!",
                "configured_primary_rank",
            )),
        };
        let credential = Credential::new("controlled-secret".to_owned()).expect("credential");

        let completion = dpr_transform_and_deliver(
            DprTransformInput {
                selected_source: source,
                sources: &sources,
                source_selection: &selection,
                provider_state: ProviderState::SemanticDisagreement,
                policy: RenderingPolicy::Adaptive,
                english_eligible: true,
                surface_hint: None,
                process_hint: None,
                timing: None,
                protected_tokens: &[],
                cloud: DprCloudCapability::Ready {
                    boundary: &cloud,
                    credential: &credential,
                },
                clock: &clock,
            },
            &mut delivery,
        )
        .await;

        assert!(completion.cloud_attempted);
        assert_eq!(cloud_calls.load(Ordering::SeqCst), 1);
        assert_eq!(remaining_ms.load(Ordering::SeqCst), 1_400);
        assert_eq!(delivery_calls.load(Ordering::SeqCst), 1);
        assert_eq!(delivered.lock().expect("delivery lock").as_slice(), ["Hello from voisu!"]);
        assert_eq!(completion.rendered, "Hello from voisu!");
        assert_eq!(completion.compose_decision, CompositionDecision::Accept);
        assert_eq!(completion.delivery_flags, DeliveryFlags::dpr_default());
        assert_eq!(completion.cloud_error, None);
    }

    #[tokio::test]
    async fn slow_cloud_is_cancelled_and_baseline_enters_delivery_at_1500ms() {
        let delivery_calls = Arc::new(AtomicUsize::new(0));
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let delivery_at_ms = Arc::new(AtomicU64::new(u64::MAX));
        let clock = ControlledClock::new(100);
        let mut delivery = RecordingDelivery {
            calls: Arc::clone(&delivery_calls),
            rendered: Arc::clone(&delivered),
            initiated_ms: Some((clock.clone(), Arc::clone(&delivery_at_ms))),
        };
        let source = "hello from voisu";
        let sources = [
            ComposeSource {
                provider: SttProvider::ProviderA,
                available: true,
                text: source.to_owned(),
                primary: true,
            },
            ComposeSource {
                provider: SttProvider::ProviderB,
                available: true,
                text: "hello from voice you".to_owned(),
                primary: false,
            },
        ];
        let selection = SourceSelection {
            selected_provider: SttProvider::ProviderA,
            reason: "configured_primary_rank".to_owned(),
        };
        let cloud_calls = Arc::new(AtomicUsize::new(0));
        let remaining_ms = Arc::new(AtomicU64::new(0));
        let cloud = CountingCloud {
            calls: Arc::clone(&cloud_calls),
            remaining_ms: Arc::clone(&remaining_ms),
            clock: clock.clone(),
            completes_at_ms: 5_000,
            outcome: CannedCloudOutcome::Success(accepted_candidate(
                source,
                "Hello from voisu!",
                "configured_primary_rank",
            )),
        };
        let credential = Credential::new("controlled-secret".to_owned()).expect("credential");

        let completion = dpr_transform_and_deliver(
            DprTransformInput {
                selected_source: source,
                sources: &sources,
                source_selection: &selection,
                provider_state: ProviderState::SemanticDisagreement,
                policy: RenderingPolicy::Adaptive,
                english_eligible: true,
                surface_hint: None,
                process_hint: None,
                timing: None,
                protected_tokens: &[],
                cloud: DprCloudCapability::Ready {
                    boundary: &cloud,
                    credential: &credential,
                },
                clock: &clock,
            },
            &mut delivery,
        )
        .await;

        assert_eq!(cloud_calls.load(Ordering::SeqCst), 1);
        assert_eq!(remaining_ms.load(Ordering::SeqCst), 1_400);
        assert_eq!(delivery_calls.load(Ordering::SeqCst), 1);
        assert_eq!(delivery_at_ms.load(Ordering::SeqCst), 1_500);
        assert_eq!(completion.rendered, "Hello from voisu.");
        assert_eq!(completion.compose_decision, CompositionDecision::FallbackBaseline);
        assert_eq!(
            completion.cloud_error,
            Some(DprCloudErrorClass::DeadlineExceeded)
        );
        assert_eq!(completion.delivery_flags, DeliveryFlags::dpr_default());
    }

    #[tokio::test]
    async fn compose_rejection_delivers_baseline_without_a_second_cloud_call() {
        let delivery_calls = Arc::new(AtomicUsize::new(0));
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let mut delivery = RecordingDelivery {
            calls: Arc::clone(&delivery_calls),
            rendered: Arc::clone(&delivered),
            initiated_ms: None,
        };
        let source = "hello from voisu";
        let sources = [
            ComposeSource {
                provider: SttProvider::ProviderA,
                available: true,
                text: source.to_owned(),
                primary: true,
            },
            ComposeSource {
                provider: SttProvider::ProviderB,
                available: true,
                text: "hello from voice you".to_owned(),
                primary: false,
            },
        ];
        let selection = SourceSelection {
            selected_provider: SttProvider::ProviderA,
            reason: "configured_primary_rank".to_owned(),
        };
        let mut stale = accepted_candidate(
            source,
            "Hello from voisu!",
            "configured_primary_rank",
        );
        stale.base_fingerprint = format!("sha256:{}", "0".repeat(64));
        let clock = ControlledClock::new(0);
        let cloud_calls = Arc::new(AtomicUsize::new(0));
        let cloud = CountingCloud {
            calls: Arc::clone(&cloud_calls),
            remaining_ms: Arc::new(AtomicU64::new(0)),
            clock: clock.clone(),
            completes_at_ms: 300,
            outcome: CannedCloudOutcome::Success(stale),
        };
        let credential = Credential::new("controlled-secret".to_owned()).expect("credential");

        let completion = dpr_transform_and_deliver(
            DprTransformInput {
                selected_source: source,
                sources: &sources,
                source_selection: &selection,
                provider_state: ProviderState::SemanticDisagreement,
                policy: RenderingPolicy::Adaptive,
                english_eligible: true,
                surface_hint: None,
                process_hint: None,
                timing: None,
                protected_tokens: &[],
                cloud: DprCloudCapability::Ready {
                    boundary: &cloud,
                    credential: &credential,
                },
                clock: &clock,
            },
            &mut delivery,
        )
        .await;

        assert_eq!(cloud_calls.load(Ordering::SeqCst), 1);
        assert_eq!(delivery_calls.load(Ordering::SeqCst), 1);
        assert_eq!(completion.rendered, "Hello from voisu.");
        assert_eq!(completion.compose_decision, CompositionDecision::FallbackBaseline);
        assert_eq!(completion.cloud_error, None);
        assert_eq!(completion.delivery_flags, DeliveryFlags::dpr_default());
    }

    #[tokio::test]
    async fn candidate_that_crosses_deadline_during_compose_cannot_upgrade_delivery() {
        let source = "hello from voisu";
        let sources = [
            ComposeSource {
                provider: SttProvider::ProviderA,
                available: true,
                text: source.to_owned(),
                primary: true,
            },
            ComposeSource {
                provider: SttProvider::ProviderB,
                available: true,
                text: "hello from voice you".to_owned(),
                primary: false,
            },
        ];
        let selection = SourceSelection {
            selected_provider: SttProvider::ProviderA,
            reason: "configured_primary_rank".to_owned(),
        };
        let clock = SequenceClock {
            calls: AtomicUsize::new(0),
            elapsed_ms: vec![100, 700, 1_500],
        };
        let cloud_calls = Arc::new(AtomicUsize::new(0));
        let cloud = ImmediateCandidateCloud {
            calls: Arc::clone(&cloud_calls),
            candidate: accepted_candidate(
                source,
                "Hello from voisu!",
                "configured_primary_rank",
            ),
        };
        let credential = Credential::new("controlled-secret".to_owned()).expect("credential");
        let delivery_calls = Arc::new(AtomicUsize::new(0));
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let mut delivery = RecordingDelivery {
            calls: Arc::clone(&delivery_calls),
            rendered: Arc::clone(&delivered),
            initiated_ms: None,
        };

        let completion = dpr_transform_and_deliver(
            DprTransformInput {
                selected_source: source,
                sources: &sources,
                source_selection: &selection,
                provider_state: ProviderState::SemanticDisagreement,
                policy: RenderingPolicy::Adaptive,
                english_eligible: true,
                surface_hint: None,
                process_hint: None,
                timing: None,
                protected_tokens: &[],
                cloud: DprCloudCapability::Ready {
                    boundary: &cloud,
                    credential: &credential,
                },
                clock: &clock,
            },
            &mut delivery,
        )
        .await;

        assert_eq!(cloud_calls.load(Ordering::SeqCst), 1);
        assert_eq!(delivery_calls.load(Ordering::SeqCst), 1);
        assert_eq!(delivered.lock().expect("delivery lock").as_slice(), ["Hello from voisu."]);
        assert_eq!(completion.rendered, "Hello from voisu.");
        assert_eq!(completion.compose_decision, CompositionDecision::FallbackBaseline);
        assert_eq!(
            completion.cloud_error,
            Some(DprCloudErrorClass::DeadlineExceeded)
        );
        assert_eq!(completion.delivery_flags, DeliveryFlags::dpr_default());
    }

    #[tokio::test]
    async fn provider_failure_delivers_baseline_after_exactly_one_total_attempt() {
        let delivery_calls = Arc::new(AtomicUsize::new(0));
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let mut delivery = RecordingDelivery {
            calls: Arc::clone(&delivery_calls),
            rendered: delivered,
            initiated_ms: None,
        };
        let source = "hello from voisu";
        let sources = [
            ComposeSource {
                provider: SttProvider::ProviderA,
                available: true,
                text: source.to_owned(),
                primary: true,
            },
            ComposeSource {
                provider: SttProvider::ProviderB,
                available: true,
                text: "hello from voice you".to_owned(),
                primary: false,
            },
        ];
        let selection = SourceSelection {
            selected_provider: SttProvider::ProviderA,
            reason: "configured_primary_rank".to_owned(),
        };
        let clock = ControlledClock::new(0);
        let cloud_calls = Arc::new(AtomicUsize::new(0));
        let cloud = CountingCloud {
            calls: Arc::clone(&cloud_calls),
            remaining_ms: Arc::new(AtomicU64::new(0)),
            clock: clock.clone(),
            completes_at_ms: 200,
            outcome: CannedCloudOutcome::Failure(DprCloudErrorClass::Http5xx),
        };
        let credential = Credential::new("controlled-secret".to_owned()).expect("credential");

        let completion = dpr_transform_and_deliver(
            DprTransformInput {
                selected_source: source,
                sources: &sources,
                source_selection: &selection,
                provider_state: ProviderState::SemanticDisagreement,
                policy: RenderingPolicy::Adaptive,
                english_eligible: true,
                surface_hint: None,
                process_hint: None,
                timing: None,
                protected_tokens: &[],
                cloud: DprCloudCapability::Ready {
                    boundary: &cloud,
                    credential: &credential,
                },
                clock: &clock,
            },
            &mut delivery,
        )
        .await;

        assert_eq!(cloud_calls.load(Ordering::SeqCst), 1);
        assert_eq!(delivery_calls.load(Ordering::SeqCst), 1);
        assert_eq!(completion.rendered, "Hello from voisu.");
        assert_eq!(completion.cloud_error, Some(DprCloudErrorClass::Http5xx));
        assert_eq!(completion.delivery_flags, DeliveryFlags::dpr_default());
    }

    #[tokio::test]
    async fn every_cloud_forbidden_route_delivers_with_zero_attempts() {
        let credential = Credential::new("controlled-secret".to_owned()).expect("credential");
        let cloud = ForbiddenCloud;
        for (source, policy, provider_state, surface_hint) in [
            (
                "the providers disagree",
                RenderingPolicy::Natural,
                ProviderState::SemanticDisagreement,
                None,
            ),
            (
                "a simple local sentence",
                RenderingPolicy::Adaptive,
                ProviderState::SingleProvider,
                None,
            ),
            (
                "1. first\n2. second",
                RenderingPolicy::Adaptive,
                ProviderState::SingleProvider,
                None,
            ),
            (
                "cargo test",
                RenderingPolicy::Adaptive,
                ProviderState::SingleProvider,
                Some(SurfaceHint::Terminal),
            ),
        ] {
            let sources = [ComposeSource {
                provider: SttProvider::ProviderA,
                available: true,
                text: source.to_owned(),
                primary: true,
            }];
            let selection = SourceSelection {
                selected_provider: SttProvider::ProviderA,
                reason: "only_available".to_owned(),
            };
            let delivery_calls = Arc::new(AtomicUsize::new(0));
            let mut delivery = RecordingDelivery {
                calls: Arc::clone(&delivery_calls),
                rendered: Arc::new(Mutex::new(Vec::new())),
                initiated_ms: None,
            };
            let clock = FixedClock(Duration::ZERO);
            let completion = dpr_transform_and_deliver(
                DprTransformInput {
                    selected_source: source,
                    sources: &sources,
                    source_selection: &selection,
                    provider_state,
                    policy,
                    english_eligible: true,
                    surface_hint,
                    process_hint: None,
                    timing: None,
                    protected_tokens: &[],
                    cloud: DprCloudCapability::Ready {
                        boundary: &cloud,
                        credential: &credential,
                    },
                    clock: &clock,
                },
                &mut delivery,
            )
            .await;

            assert_eq!(completion.routing.cloud_request, CloudRequest::NotAllowed);
            assert!(!completion.cloud_attempted);
            assert_eq!(delivery_calls.load(Ordering::SeqCst), 1);
            assert_eq!(completion.delivery_flags, DeliveryFlags::dpr_default());
        }
    }

    #[tokio::test]
    async fn non_english_path_cannot_open_cloud_even_when_route_allows_it() {
        let source = "bonjour from voisu";
        let sources = [ComposeSource {
            provider: SttProvider::ProviderA,
            available: true,
            text: source.to_owned(),
            primary: true,
        }];
        let selection = SourceSelection {
            selected_provider: SttProvider::ProviderA,
            reason: "only_available".to_owned(),
        };
        let credential = Credential::new("controlled-secret".to_owned()).expect("credential");
        let cloud = ForbiddenCloud;
        let delivery_calls = Arc::new(AtomicUsize::new(0));
        let mut delivery = RecordingDelivery {
            calls: Arc::clone(&delivery_calls),
            rendered: Arc::new(Mutex::new(Vec::new())),
            initiated_ms: None,
        };
        let clock = FixedClock(Duration::ZERO);

        let completion = dpr_transform_and_deliver(
            DprTransformInput {
                selected_source: source,
                sources: &sources,
                source_selection: &selection,
                provider_state: ProviderState::SemanticDisagreement,
                policy: RenderingPolicy::Adaptive,
                english_eligible: false,
                surface_hint: None,
                process_hint: None,
                timing: None,
                protected_tokens: &[],
                cloud: DprCloudCapability::Ready {
                    boundary: &cloud,
                    credential: &credential,
                },
                clock: &clock,
            },
            &mut delivery,
        )
        .await;

        assert_eq!(completion.routing.cloud_request, CloudRequest::Allowed);
        assert!(!completion.cloud_attempted);
        assert_eq!(delivery_calls.load(Ordering::SeqCst), 1);
        assert_eq!(completion.delivery_flags, DeliveryFlags::dpr_default());
    }

    #[tokio::test]
    async fn structured_required_attempt_without_ready_credentials_delivers_baseline() {
        let source = "goal build voisu context rust requirements fast";
        let sources = [ComposeSource {
            provider: SttProvider::ProviderA,
            available: true,
            text: source.to_owned(),
            primary: true,
        }];
        let selection = SourceSelection {
            selected_provider: SttProvider::ProviderA,
            reason: "only_available".to_owned(),
        };
        let delivery_calls = Arc::new(AtomicUsize::new(0));
        let mut delivery = RecordingDelivery {
            calls: Arc::clone(&delivery_calls),
            rendered: Arc::new(Mutex::new(Vec::new())),
            initiated_ms: None,
        };
        let clock = FixedClock(Duration::ZERO);

        let completion = dpr_transform_and_deliver(
            DprTransformInput {
                selected_source: source,
                sources: &sources,
                source_selection: &selection,
                provider_state: ProviderState::SingleProvider,
                policy: RenderingPolicy::Structured,
                english_eligible: true,
                surface_hint: None,
                process_hint: None,
                timing: None,
                protected_tokens: &[],
                cloud: DprCloudCapability::Unavailable,
                clock: &clock,
            },
            &mut delivery,
        )
        .await;

        assert_eq!(completion.routing.cloud_request, CloudRequest::Required);
        assert!(!completion.cloud_attempted);
        assert_eq!(delivery_calls.load(Ordering::SeqCst), 1);
        assert_eq!(completion.compose_decision, CompositionDecision::FallbackBaseline);
        assert_eq!(
            completion.cloud_error,
            Some(DprCloudErrorClass::CredentialUnavailable)
        );
        assert_eq!(completion.delivery_flags, DeliveryFlags::dpr_default());
    }
}
