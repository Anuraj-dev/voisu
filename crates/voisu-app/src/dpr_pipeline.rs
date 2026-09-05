//! Flagged Developer Prompt Rendering orchestration.
//!
//! The module owns one Final Transcript decision from a snapshotted policy and
//! selected Source Transcript through the existing compose and Delivery gates.
//! It has no API for replacing text after Delivery.

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use voisu_core::{
    CloudOutcome, CloudRequest, ComposeInput, ComposeSource, CompositionDecision, Credential,
    DeliveryAdapter, DeliveryFlags, DeliveryOutcome, DprDiagnostic, FormatEditSafety,
    IntentObservation, LocalBaselineOptions, MAX_COMPOSE_FIELD_UTF8_BYTES, ProcessHint, Provider,
    ProviderState, RenderingPolicy, RoutingDecision, SourceSelection, SourceTranscript,
    SttProvider, SurfaceHint, TimingHint, Transcript, TranscriptSelection, apply_format_edits_with,
    compose_structured_candidate, leftover_admits_format_cloud, organize_local_baseline,
    route_intent, sanitize_source_transcripts, text_sha256_fingerprint,
};

use crate::dpr_cloud::{
    DprCloudAttempt, DprCloudClient, DprCloudErrorClass, DprCloudRequest, MAX_DPR_PROTECTED_TOKENS,
};

/// Maximum elapsed time from utterance end to initiating Delivery.
pub const DPR_DELIVERY_DEADLINE: Duration = Duration::from_millis(1_500);
/// Failure ceiling from ValidationCompleted to initiating formatting Delivery.
pub const DPR_FORMAT_GATE: Duration = Duration::from_millis(5_000);
/// Maximum time granted to the formatting Provider request.
pub const DPR_FORMAT_PROVIDER_BUDGET: Duration = Duration::from_millis(4_750);
/// Time reserved for host parsing, validation, composition, and Delivery initiation.
pub const DPR_FORMAT_HOST_RESERVE: Duration = Duration::from_millis(250);

pub type DprCloudFuture<'a> = Pin<Box<dyn Future<Output = DprCloudAttempt> + Send + 'a>>;

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

/// Monotonic elapsed time from the deadline origin selected by the daemon.
pub trait DprPipelineClock: Send + Sync {
    fn elapsed(&self) -> Duration;
}

pub struct SystemDprPipelineClock {
    started_at: Instant,
}

/// Instant that starts the DPR deadline clock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DprPipelineClockOrigin {
    UtteranceEnd,
    ValidationCompleted,
}

/// Formatting (`qwen_format_enabled`) is measured from ValidationCompleted.
/// Derivation stays on utterance_end.
#[must_use]
pub const fn dpr_pipeline_clock_origin(qwen_format_enabled: bool) -> DprPipelineClockOrigin {
    if qwen_format_enabled {
        DprPipelineClockOrigin::ValidationCompleted
    } else {
        DprPipelineClockOrigin::UtteranceEnd
    }
}

impl SystemDprPipelineClock {
    #[must_use]
    pub const fn from_utterance_end(utterance_end: Instant) -> Self {
        Self {
            started_at: utterance_end,
        }
    }

    #[must_use]
    pub const fn from_validation_completed(validation_completed: Instant) -> Self {
        Self {
            started_at: validation_completed,
        }
    }
}

impl DprPipelineClock for SystemDprPipelineClock {
    fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
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
    /// Request the small-edit formatting contract instead of #139 derivation.
    /// Production leaves this false until the Qwen formatter flag is on.
    pub small_edit_contract: bool,
}

pub struct DprTransformCompletion {
    pub rendered: String,
    pub delivery: Result<DeliveryOutcome, voisu_core::BoundaryError>,
    pub delivery_flags: DeliveryFlags,
    pub routing: RoutingDecision,
    pub compose_decision: CompositionDecision,
    pub cloud_attempted: bool,
    pub cloud_error: Option<DprCloudErrorClass>,
    pub diagnostic: DprDiagnostic,
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
pub fn dpr_source_context(
    sources: &[SourceTranscript],
    dictionary_terms: &[String],
) -> Option<DprSourceContext> {
    // Quality-classify before selection so pure-outro Groq never wins over empty
    // Deepgram and a trailing anchored outro never enters compose/Delivery.
    let sanitized = sanitize_source_transcripts(sources.iter().cloned());
    let available: Vec<&SourceTranscript> = sanitized
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
        && available.iter().all(|source| {
            normalized_source_words(&source.text) == normalized_source_words(&available[0].text)
        });
    let other = available
        .iter()
        .find(|source| source.provider != selected_provider)
        .copied();
    let safe_complementary =
        if available.len() == 2 && !exact_agreement && !punctuation_only_agreement {
            other.and_then(|other| merge_insertion_only_sources(&selected.text, &other.text))
        } else {
            None
        };
    let protected_disagreement = safe_complementary.is_none()
        && other.is_some_and(|other| {
            protected_atoms_disagree(&selected.text, &other.text, dictionary_terms)
        });
    let provider_state = if available.len() <= 1 {
        ProviderState::SingleProvider
    } else if exact_agreement {
        ProviderState::ExactAgreement
    } else if punctuation_only_agreement {
        ProviderState::PunctuationOnlyAgreement
    } else if safe_complementary.is_some() {
        ProviderState::SafeComplementary
    } else if protected_disagreement {
        ProviderState::ProtectedTokenDisagreement
    } else {
        ProviderState::SemanticDisagreement
    };
    let reason = if available.len() <= 1 {
        "only_available"
    } else if exact_agreement {
        "exact_agreement"
    } else if punctuation_only_agreement {
        "punctuation_local_render"
    } else if safe_complementary.is_some() {
        "safe_complementary_merge"
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
    if let Some(other) = other {
        compose_sources.push(ComposeSource {
            provider: SttProvider::ProviderB,
            available: true,
            text: other.text.clone(),
            primary: false,
        });
    }

    Some(DprSourceContext {
        selected_source: safe_complementary.unwrap_or_else(|| selected.text.clone()),
        sources: compose_sources,
        source_selection: SourceSelection {
            selected_provider: SttProvider::ProviderA,
            reason: reason.to_owned(),
        },
        provider_state,
        transcript_selection: if provider_state == ProviderState::SafeComplementary {
            TranscriptSelection::Complementary
        } else {
            match selected_provider {
                Provider::Groq if punctuation_only_agreement => {
                    TranscriptSelection::NearIdenticalGroq
                }
                Provider::Groq => TranscriptSelection::SourceGroq,
                Provider::Deepgram => TranscriptSelection::SourceDeepgram,
            }
        },
    })
}

fn protected_atoms_disagree(left: &str, right: &str, dictionary_terms: &[String]) -> bool {
    let left_protected: Vec<String> = dpr_protected_tokens(left, dictionary_terms)
        .into_iter()
        .filter(|token| !is_closed_negation(token))
        .collect();
    let right_protected: Vec<String> = dpr_protected_tokens(right, dictionary_terms)
        .into_iter()
        .filter(|token| !is_closed_negation(token))
        .collect();
    left_protected.iter().any(|token| !right.contains(token))
        || right_protected.iter().any(|token| !left.contains(token))
}

fn is_closed_negation(token: &str) -> bool {
    matches!(
        token.to_ascii_lowercase().as_str(),
        "no" | "not"
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
    )
}

/// Conservatively merges two whitespace-token streams only when their shared
/// anchors remain ordered and at most one provider contributes tokens in each
/// gap. If both providers offer different text for the same gap, the evidence
/// is disputed and no merge is produced. The token cap keeps the bounded
/// look-ahead local even for an adversarially large Source Transcript.
fn merge_insertion_only_sources(left: &str, right: &str) -> Option<String> {
    const MAX_SAFE_COMPLEMENT_TOKENS: usize = 128;
    let left_tokens: Vec<&str> = left.split_whitespace().collect();
    let right_tokens: Vec<&str> = right.split_whitespace().collect();
    if left_tokens.is_empty()
        || right_tokens.is_empty()
        || left_tokens.len() > MAX_SAFE_COMPLEMENT_TOKENS
        || right_tokens.len() > MAX_SAFE_COMPLEMENT_TOKENS
    {
        return None;
    }

    let mut merged = Vec::with_capacity(left_tokens.len().saturating_add(right_tokens.len()));
    let mut left_at = 0usize;
    let mut right_at = 0usize;
    let mut shared_anchors = 0usize;
    let mut left_contributed = false;
    let mut right_contributed = false;

    while left_at < left_tokens.len() || right_at < right_tokens.len() {
        let mut next_anchor = None;
        let mut nearest_distance = usize::MAX;
        for (left_index, left_token) in left_tokens.iter().enumerate().skip(left_at) {
            for (right_index, right_token) in right_tokens.iter().enumerate().skip(right_at) {
                if token_anchor_eq(left_token, right_token) {
                    let distance =
                        left_index.saturating_sub(left_at) + right_index.saturating_sub(right_at);
                    if distance < nearest_distance {
                        nearest_distance = distance;
                        next_anchor = Some((left_index, right_index));
                    }
                }
            }
        }

        let Some((left_anchor, right_anchor)) = next_anchor else {
            let left_gap = &left_tokens[left_at..];
            let right_gap = &right_tokens[right_at..];
            if !left_gap.is_empty() && !right_gap.is_empty() {
                return None;
            }
            if !left_gap
                .iter()
                .chain(right_gap)
                .all(|token| safe_complement_token(token))
            {
                return None;
            }
            left_contributed |= !left_gap.is_empty();
            right_contributed |= !right_gap.is_empty();
            merged.extend_from_slice(left_gap);
            merged.extend_from_slice(right_gap);
            break;
        };

        let left_gap = &left_tokens[left_at..left_anchor];
        let right_gap = &right_tokens[right_at..right_anchor];
        if !left_gap.is_empty() && !right_gap.is_empty() {
            return None;
        }
        if !left_gap
            .iter()
            .chain(right_gap)
            .all(|token| safe_complement_token(token))
        {
            return None;
        }
        left_contributed |= !left_gap.is_empty();
        right_contributed |= !right_gap.is_empty();
        merged.extend_from_slice(left_gap);
        merged.extend_from_slice(right_gap);
        merged.push(left_tokens[left_anchor]);
        shared_anchors += 1;
        left_at = left_anchor + 1;
        right_at = right_anchor + 1;
    }

    (shared_anchors >= 2 && left_contributed && right_contributed).then(|| merged.join(" "))
}

fn safe_complement_token(token: &str) -> bool {
    token.bytes().any(|byte| byte.is_ascii_digit())
        || token.contains("//")
        || token.contains('/')
        || token.contains('\\')
        || token.starts_with('-')
        || token.contains('_')
        || token.contains("::")
        || token.contains(['(', ')', '[', ']', '{', '}', '=', '@'])
}

fn token_anchor_eq(left: &str, right: &str) -> bool {
    let normalize = |token: &str| {
        token
            .trim_matches(|character: char| !character.is_alphanumeric())
            .to_ascii_lowercase()
    };
    let left = normalize(left);
    !left.is_empty() && left == normalize(right)
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
            "no" | "not"
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
            || token.contains(['(', ')', '[', ']', '{', '}', '=', '@'])
            || is_dot_technical_token(token);
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

    // Organized `"…"` spans (including interiors from `quote,` … `unquote`).
    let mut quoted_from = 0;
    while let Some(open_relative) = selected_source[quoted_from..].find('"') {
        let interior_start = quoted_from + open_relative + 1;
        let Some(close_relative) = selected_source[interior_start..].find('"') else {
            break;
        };
        push_protected(
            &mut protected,
            selected_source,
            &selected_source[interior_start..interior_start + close_relative],
        );
        quoted_from = interior_start + close_relative + 1;
    }

    // Dictionary entries are important spelling/name evidence, but cannot
    // crowd closed technical or semantic atoms out of the bounded request.
    for term in dictionary_terms {
        push_protected(&mut protected, selected_source, term);
    }

    protected
}

/// `cargo.toml` / `.env` are technical. A sentence-final `hello.` is not.
fn is_dot_technical_token(token: &str) -> bool {
    let bytes = token.as_bytes();
    if bytes.first() == Some(&b'.')
        && bytes
            .get(1)
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
    {
        return true;
    }
    bytes.windows(3).any(|window| {
        window[1] == b'.' && window[0].is_ascii_alphanumeric() && window[2].is_ascii_alphanumeric()
    })
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
    let delivery_deadline = if input.small_edit_contract {
        DPR_FORMAT_GATE
    } else {
        DPR_DELIVERY_DEADLINE
    };
    let routing = route_intent(&IntentObservation {
        policy: input.policy,
        primary_text: input.selected_source.to_owned(),
        provider_state: input.provider_state,
        surface_hint: input.surface_hint,
        process_hint: input.process_hint,
        timing: input.timing,
    });
    let route_selected_at = input.clock.elapsed();
    let mut diagnostic = DprDiagnostic::production(&routing, route_selected_at);
    let baseline = organize_local_baseline(
        input.selected_source,
        &LocalBaselineOptions {
            policy: input.policy,
            route: routing.route,
            timing: None,
        },
    );
    let organized_protected = dpr_protected_tokens(baseline.rendered(), &[]);
    let mut protected_tokens: Vec<&str> = input.protected_tokens.to_vec();
    for token in &organized_protected {
        if protected_tokens.len() >= MAX_DPR_PROTECTED_TOKENS {
            break;
        }
        if !protected_tokens.contains(&token.as_str()) {
            protected_tokens.push(token.as_str());
        }
    }
    let base_fingerprint = text_sha256_fingerprint(input.selected_source);
    let organized_fingerprint = text_sha256_fingerprint(baseline.rendered());
    let mut cloud_attempted = false;
    let mut cloud_error = None;
    let mut candidate = None;
    let mut format_rendered = None;
    let skip_format_cloud =
        input.small_edit_contract && !leftover_admits_format_cloud(baseline.rendered());
    let cloud_outcome = if !input.english_eligible
        || routing.cloud_request == CloudRequest::NotAllowed
        || skip_format_cloud
    {
        diagnostic.cloud_skipped(route_selected_at);
        CloudOutcome::Skipped
    } else {
        let cloud_budget_elapsed = input.clock.elapsed();
        let provider_budget = delivery_deadline
            .saturating_sub(cloud_budget_elapsed)
            .saturating_sub(if input.small_edit_contract {
                DPR_FORMAT_HOST_RESERVE
            } else {
                Duration::ZERO
            })
            .min(if input.small_edit_contract {
                DPR_FORMAT_PROVIDER_BUDGET
            } else {
                DPR_DELIVERY_DEADLINE
            });
        if cloud_budget_elapsed >= delivery_deadline || provider_budget.is_zero() {
            cloud_error = Some(DprCloudErrorClass::DeadlineExceeded);
            diagnostic.cloud_skipped(cloud_budget_elapsed);
            CloudOutcome::DeadlineExceeded
        } else {
            match input.cloud {
                DprCloudCapability::Unavailable => {
                    cloud_error = Some(DprCloudErrorClass::CredentialUnavailable);
                    diagnostic.cloud_skipped(cloud_budget_elapsed);
                    CloudOutcome::ProviderFailure
                }
                DprCloudCapability::Ready {
                    boundary,
                    credential,
                } => {
                    cloud_attempted = true;
                    diagnostic.cloud_request_started(cloud_budget_elapsed);
                    let attempt = boundary
                        .attempt(
                            credential,
                            DprCloudRequest {
                                sources: input.sources,
                                source_selection: input.source_selection,
                                selected_source: input.selected_source,
                                base_fingerprint: &base_fingerprint,
                                policy: input.policy,
                                protected_tokens: &protected_tokens,
                                small_edit_contract: input.small_edit_contract,
                            },
                            provider_budget,
                        )
                        .await;
                    let (attempt_candidate, attempt_format_edits, attempt_error) =
                        attempt.into_parts();
                    let attempt_completed_at = input.clock.elapsed();
                    if attempt_candidate.is_some()
                        || attempt_format_edits.is_some()
                        || attempt_error.is_some_and(dpr_error_has_response)
                    {
                        diagnostic.cloud_response_received(attempt_completed_at);
                    }
                    if attempt_completed_at >= delivery_deadline {
                        cloud_error = Some(DprCloudErrorClass::DeadlineExceeded);
                        CloudOutcome::DeadlineExceeded
                    } else if input.small_edit_contract {
                        match attempt_format_edits {
                            Some(edits) => {
                                let edit_base = if edits.base_fingerprint == organized_fingerprint {
                                    baseline.rendered()
                                } else {
                                    input.selected_source
                                };
                                let applied = apply_format_edits_with(
                                    edit_base,
                                    &edits,
                                    &FormatEditSafety {
                                        protected_tokens: &protected_tokens,
                                        policy: input.policy,
                                    },
                                );
                                if applied.accepted && !edits.edits.is_empty() {
                                    if edits.base_fingerprint == organized_fingerprint {
                                        format_rendered = Some(applied.rendered);
                                        CloudOutcome::Succeeded
                                    } else {
                                        // Cloud edited the spoken source. Re-run
                                        // spoken-mark conversion so leftover cues
                                        // still convert, then reject if that would
                                        // smash a converted command/URL.
                                        let reapplied = organize_local_baseline(
                                            &applied.rendered,
                                            &LocalBaselineOptions {
                                                policy: input.policy,
                                                route: voisu_core::RenderingRoute::LiteralIdentity,
                                                timing: None,
                                            },
                                        );
                                        if organized_protected.iter().any(|token| {
                                            let core = token.trim_end_matches(['.', '!', '?', ',']);
                                            !core.is_empty() && !reapplied.rendered().contains(core)
                                        }) {
                                            cloud_error = Some(DprCloudErrorClass::CandidateSchema);
                                            CloudOutcome::SchemaFailure
                                        } else {
                                            format_rendered = Some(reapplied.rendered().to_owned());
                                            CloudOutcome::Succeeded
                                        }
                                    }
                                } else if applied.accepted {
                                    CloudOutcome::Skipped
                                } else {
                                    cloud_error = Some(DprCloudErrorClass::CandidateSchema);
                                    CloudOutcome::SchemaFailure
                                }
                            }
                            None => {
                                let error =
                                    attempt_error.unwrap_or(DprCloudErrorClass::CandidateSchema);
                                cloud_error = Some(error);
                                cloud_outcome_for_error(error)
                            }
                        }
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
    let compose_finished_at = input.clock.elapsed();
    let (rendered, delivery_flags, compose_decision, fallback_trigger, error_codes, span_summary) =
        if let Some(host_rendered) = format_rendered {
            if compose_finished_at >= delivery_deadline {
                cloud_error = Some(DprCloudErrorClass::DeadlineExceeded);
                let composed = compose_structured_candidate(&ComposeInput {
                    local_baseline: &baseline,
                    base_fingerprint: &base_fingerprint,
                    sources: input.sources,
                    source_selection: input.source_selection,
                    protected_tokens: &protected_tokens,
                    policy: input.policy,
                    cloud_outcome: CloudOutcome::DeadlineExceeded,
                    candidate: None,
                });
                (
                    composed.rendered().to_owned(),
                    composed.delivery(),
                    composed.decision(),
                    composed.fallback_trigger(),
                    composed.error_codes().to_vec(),
                    composed.span_summary().cloned(),
                )
            } else {
                (
                    host_rendered,
                    DeliveryFlags::dpr_default(),
                    CompositionDecision::Accept,
                    None,
                    Vec::new(),
                    None,
                )
            }
        } else {
            let mut composed = compose_structured_candidate(&ComposeInput {
                local_baseline: &baseline,
                base_fingerprint: &base_fingerprint,
                sources: input.sources,
                source_selection: input.source_selection,
                protected_tokens: &protected_tokens,
                policy: input.policy,
                cloud_outcome,
                candidate: candidate.as_ref(),
            });
            if cloud_outcome == CloudOutcome::Succeeded && compose_finished_at >= delivery_deadline
            {
                cloud_error = Some(DprCloudErrorClass::DeadlineExceeded);
                composed = compose_structured_candidate(&ComposeInput {
                    local_baseline: &baseline,
                    base_fingerprint: &base_fingerprint,
                    sources: input.sources,
                    source_selection: input.source_selection,
                    protected_tokens: &protected_tokens,
                    policy: input.policy,
                    cloud_outcome: CloudOutcome::DeadlineExceeded,
                    candidate: None,
                });
            }
            (
                composed.rendered().to_owned(),
                composed.delivery(),
                composed.decision(),
                composed.fallback_trigger(),
                composed.error_codes().to_vec(),
                composed.span_summary().cloned(),
            )
        };
    diagnostic.composition_completed(
        compose_decision,
        fallback_trigger,
        &error_codes,
        compose_finished_at,
    );
    // B5 additive per-span evidence; absent for candidate-level rejects and
    // soft salvage, so shipped (flag-off) records are byte-identical.
    if let Some(summary) = span_summary.as_ref() {
        diagnostic.record_span_adjudication(summary);
    }
    diagnostic.delivery_emitted(input.clock.elapsed(), delivery_flags);
    let delivery = delivery.deliver(Transcript(rendered.clone())).await;
    DprTransformCompletion {
        rendered,
        delivery,
        delivery_flags,
        routing,
        compose_decision,
        cloud_attempted,
        cloud_error,
        diagnostic,
    }
}

fn dpr_error_has_response(error: DprCloudErrorClass) -> bool {
    matches!(
        error,
        DprCloudErrorClass::ResponseTooLarge
            | DprCloudErrorClass::ProviderEnvelope
            | DprCloudErrorClass::CandidateSchema
    )
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
        BoundaryFuture, DprDiagnosticEventName, DprFeedbackKind, FormatEditCandidate,
        StructuredCandidate, SttProvider, Transcript, parse_format_edit_candidate_json,
        parse_structured_candidate_json, text_sha256_fingerprint,
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

    #[derive(Clone)]
    enum CannedFormatCloudOutcome {
        Success(FormatEditCandidate),
        Failure(DprCloudErrorClass),
    }

    struct CountingCloud {
        calls: Arc<AtomicUsize>,
        remaining_ms: Arc<AtomicU64>,
        clock: ControlledClock,
        completes_at_ms: u64,
        outcome: CannedCloudOutcome,
    }

    struct CountingFormatCloud {
        calls: Arc<AtomicUsize>,
        remaining_ms: Arc<AtomicU64>,
        clock: ControlledClock,
        completes_at_ms: u64,
        honor_budget: bool,
        outcome: CannedFormatCloudOutcome,
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
            let deadline_ms =
                current_ms.saturating_add(u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX));
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

    impl DprCloudBoundary for CountingFormatCloud {
        fn attempt<'a>(
            &'a self,
            _credential: &'a Credential,
            request: DprCloudRequest<'a>,
            remaining: Duration,
        ) -> DprCloudFuture<'a> {
            assert!(request.small_edit_contract);
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.remaining_ms.store(
                u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX),
                Ordering::SeqCst,
            );
            let current_ms = self.clock.0.load(Ordering::SeqCst);
            let deadline_ms =
                current_ms.saturating_add(u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX));
            let result = if self.honor_budget && self.completes_at_ms > deadline_ms {
                self.clock.0.store(deadline_ms, Ordering::SeqCst);
                DprCloudAttempt::failure(DprCloudErrorClass::DeadlineExceeded)
            } else {
                self.clock.0.store(self.completes_at_ms, Ordering::SeqCst);
                match &self.outcome {
                    CannedFormatCloudOutcome::Success(candidate) => {
                        DprCloudAttempt::format_edits(candidate.clone())
                    }
                    CannedFormatCloudOutcome::Failure(error) => DprCloudAttempt::failure(*error),
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

    fn diagnostic_event_names(completion: &DprTransformCompletion) -> Vec<DprDiagnosticEventName> {
        completion
            .diagnostic
            .events()
            .iter()
            .map(|event| event.name())
            .collect()
    }

    #[test]
    fn formatting_clock_origin_is_validation_completed() {
        assert_eq!(
            dpr_pipeline_clock_origin(true),
            DprPipelineClockOrigin::ValidationCompleted
        );
    }

    #[test]
    fn derivation_clock_origin_is_utterance_end() {
        assert_eq!(
            dpr_pipeline_clock_origin(false),
            DprPipelineClockOrigin::UtteranceEnd
        );
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
        let context = dpr_source_context(&sources, &[]).expect("source context");
        assert_eq!(context.selected_source, "hello from voisu");
        assert_eq!(context.sources[0].provider, SttProvider::ProviderA);
        assert_eq!(context.sources[0].text, "hello from voisu");
        assert!(context.sources[0].primary);
        assert_eq!(context.sources[1].provider, SttProvider::ProviderB);
        assert_eq!(context.provider_state, ProviderState::SemanticDisagreement);
        assert_eq!(context.source_selection.reason, "configured_primary_rank");
        assert_eq!(
            context.transcript_selection,
            TranscriptSelection::SourceGroq
        );
        assert!(dpr_source_context(&[], &[]).is_none());
    }

    #[test]
    fn pure_outro_sources_yield_no_dpr_context() {
        let sources = vec![
            SourceTranscript {
                provider: Provider::Deepgram,
                text: String::new(),
            },
            SourceTranscript {
                provider: Provider::Groq,
                text: "Thank you for watching!".to_owned(),
            },
        ];
        assert!(
            dpr_source_context(&sources, &[]).is_none(),
            "silence + pure outro must not select a Source Transcript"
        );
    }

    #[test]
    fn pure_outro_with_trivial_prefix_yields_no_dpr_context() {
        for head_outro in [
            "Please like and subscribe",
            "OK thanks for watching",
            "Yeah thank you for watching",
        ] {
            let sources = vec![
                SourceTranscript {
                    provider: Provider::Deepgram,
                    text: String::new(),
                },
                SourceTranscript {
                    provider: Provider::Groq,
                    text: head_outro.to_owned(),
                },
            ];
            assert!(
                dpr_source_context(&sources, &[]).is_none(),
                "trivial-prefix outro must not select: {head_outro:?}"
            );
        }
    }

    #[test]
    fn genuine_speech_wins_over_outro_only_sibling() {
        let sources = vec![
            SourceTranscript {
                provider: Provider::Deepgram,
                text: "Schedule the review for Wednesday morning.".to_owned(),
            },
            SourceTranscript {
                provider: Provider::Groq,
                text: "Thank you for watching!".to_owned(),
            },
        ];
        let context = dpr_source_context(&sources, &[]).expect("genuine context");
        assert_eq!(
            context.selected_source,
            "Schedule the review for Wednesday morning."
        );
        assert_eq!(
            context.transcript_selection,
            TranscriptSelection::SourceDeepgram
        );
        assert_eq!(context.provider_state, ProviderState::SingleProvider);
    }

    #[test]
    fn anchored_final_outro_is_stripped_before_dpr_selection() {
        let sources = vec![
            SourceTranscript {
                provider: Provider::Groq,
                text: "Schedule the review for Wednesday morning. Thank you for watching."
                    .to_owned(),
            },
            SourceTranscript {
                provider: Provider::Deepgram,
                text: "Schedule the review for Wednesday morning. Thanks for watching!".to_owned(),
            },
        ];
        let context = dpr_source_context(&sources, &[]).expect("stripped context");
        assert_eq!(
            context.selected_source,
            "Schedule the review for Wednesday morning."
        );
        assert!(!context.selected_source.to_lowercase().contains("watching"));
    }

    #[test]
    fn mid_sentence_outro_phrase_is_preserved_for_dpr_selection() {
        let spoken = "The demo went well and the recording was transcribed by Whisper.";
        let sources = vec![
            SourceTranscript {
                provider: Provider::Groq,
                text: spoken.to_owned(),
            },
            SourceTranscript {
                provider: Provider::Deepgram,
                text: spoken.to_owned(),
            },
        ];
        let context = dpr_source_context(&sources, &[]).expect("mid-sentence context");
        assert_eq!(context.selected_source, spoken);
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

    #[test]
    fn protected_tokens_include_organized_quote_interior_and_dot_files() {
        let quoted = dpr_protected_tokens("\"connection refused\"", &[]);
        assert!(
            quoted.iter().any(|token| token == "connection refused"),
            "quoted interior missing: {quoted:?}"
        );

        let dots = dpr_protected_tokens("open cargo.toml and .env today", &[]);
        assert!(
            dots.iter().any(|token| token == "cargo.toml"),
            "cargo.toml missing: {dots:?}"
        );
        assert!(
            dots.iter().any(|token| token == ".env"),
            ".env missing: {dots:?}"
        );
        assert!(!dots.iter().any(|token| token == "today"));
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
                small_edit_contract: false,
            },
            &mut delivery,
        )
        .await;

        assert!(!completion.cloud_attempted);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            rendered.lock().expect("delivery lock").as_slice(),
            ["Hello from voisu."]
        );
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
        assert_eq!(
            completion.diagnostic.feedback_kind(),
            DprFeedbackKind::Silent
        );
        assert_eq!(
            diagnostic_event_names(&completion),
            vec![
                DprDiagnosticEventName::RouteSelected,
                DprDiagnosticEventName::CloudSkipped,
                DprDiagnosticEventName::FallbackBaselineSelected,
                DprDiagnosticEventName::DeliveryEmitted,
            ]
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
                small_edit_contract: false,
            },
            &mut delivery,
        )
        .await;

        assert!(completion.cloud_attempted);
        assert_eq!(cloud_calls.load(Ordering::SeqCst), 1);
        assert_eq!(remaining_ms.load(Ordering::SeqCst), 1_400);
        assert_eq!(delivery_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            delivered.lock().expect("delivery lock").as_slice(),
            ["Hello from voisu!"]
        );
        assert_eq!(completion.rendered, "Hello from voisu!");
        assert_eq!(completion.compose_decision, CompositionDecision::Accept);
        assert_eq!(completion.delivery_flags, DeliveryFlags::dpr_default());
        assert_eq!(completion.cloud_error, None);
        assert_eq!(
            completion.diagnostic.feedback_kind(),
            DprFeedbackKind::Silent
        );
        assert_eq!(
            diagnostic_event_names(&completion),
            vec![
                DprDiagnosticEventName::RouteSelected,
                DprDiagnosticEventName::CloudRequestStarted,
                DprDiagnosticEventName::CloudResponseReceived,
                DprDiagnosticEventName::CompositionAccepted,
                DprDiagnosticEventName::DeliveryEmitted,
            ]
        );
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
                small_edit_contract: false,
            },
            &mut delivery,
        )
        .await;

        assert_eq!(cloud_calls.load(Ordering::SeqCst), 1);
        assert_eq!(remaining_ms.load(Ordering::SeqCst), 1_400);
        assert_eq!(delivery_calls.load(Ordering::SeqCst), 1);
        assert_eq!(delivery_at_ms.load(Ordering::SeqCst), 1_500);
        assert_eq!(completion.rendered, "Hello from voisu.");
        assert_eq!(
            completion.compose_decision,
            CompositionDecision::FallbackBaseline
        );
        assert_eq!(
            completion.cloud_error,
            Some(DprCloudErrorClass::DeadlineExceeded)
        );
        assert_eq!(completion.delivery_flags, DeliveryFlags::dpr_default());
        assert_eq!(
            completion.diagnostic.feedback_kind(),
            DprFeedbackKind::MinimalStatus
        );
        assert_eq!(
            diagnostic_event_names(&completion),
            vec![
                DprDiagnosticEventName::RouteSelected,
                DprDiagnosticEventName::CloudRequestStarted,
                DprDiagnosticEventName::CloudDeadlineExceeded,
                DprDiagnosticEventName::FallbackBaselineSelected,
                DprDiagnosticEventName::DeliveryEmitted,
            ]
        );
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
        let mut stale = accepted_candidate(source, "Hello from voisu!", "configured_primary_rank");
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
                small_edit_contract: false,
            },
            &mut delivery,
        )
        .await;

        assert_eq!(cloud_calls.load(Ordering::SeqCst), 1);
        assert_eq!(delivery_calls.load(Ordering::SeqCst), 1);
        assert_eq!(completion.rendered, "Hello from voisu.");
        assert_eq!(
            completion.compose_decision,
            CompositionDecision::FallbackBaseline
        );
        assert_eq!(completion.cloud_error, None);
        assert_eq!(completion.delivery_flags, DeliveryFlags::dpr_default());
        assert_eq!(
            completion.diagnostic.feedback_kind(),
            DprFeedbackKind::MinimalStatus
        );
        assert_eq!(
            diagnostic_event_names(&completion),
            vec![
                DprDiagnosticEventName::RouteSelected,
                DprDiagnosticEventName::CloudRequestStarted,
                DprDiagnosticEventName::CloudResponseReceived,
                DprDiagnosticEventName::SourceDerivationFailed,
                DprDiagnosticEventName::FallbackBaselineSelected,
                DprDiagnosticEventName::DeliveryEmitted,
            ]
        );
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
            elapsed_ms: vec![0, 100, 700, 1_500],
        };
        let cloud_calls = Arc::new(AtomicUsize::new(0));
        let cloud = ImmediateCandidateCloud {
            calls: Arc::clone(&cloud_calls),
            candidate: accepted_candidate(source, "Hello from voisu!", "configured_primary_rank"),
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
                small_edit_contract: false,
            },
            &mut delivery,
        )
        .await;

        assert_eq!(cloud_calls.load(Ordering::SeqCst), 1);
        assert_eq!(delivery_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            delivered.lock().expect("delivery lock").as_slice(),
            ["Hello from voisu."]
        );
        assert_eq!(completion.rendered, "Hello from voisu.");
        assert_eq!(
            completion.compose_decision,
            CompositionDecision::FallbackBaseline
        );
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
                small_edit_contract: false,
            },
            &mut delivery,
        )
        .await;

        assert_eq!(cloud_calls.load(Ordering::SeqCst), 1);
        assert_eq!(delivery_calls.load(Ordering::SeqCst), 1);
        assert_eq!(completion.rendered, "Hello from voisu.");
        assert_eq!(completion.cloud_error, Some(DprCloudErrorClass::Http5xx));
        assert_eq!(completion.delivery_flags, DeliveryFlags::dpr_default());
        assert_eq!(
            completion.diagnostic.feedback_kind(),
            DprFeedbackKind::MinimalStatus
        );
        assert_eq!(
            diagnostic_event_names(&completion),
            vec![
                DprDiagnosticEventName::RouteSelected,
                DprDiagnosticEventName::CloudRequestStarted,
                DprDiagnosticEventName::ProviderFailed,
                DprDiagnosticEventName::FallbackBaselineSelected,
                DprDiagnosticEventName::DeliveryEmitted,
            ]
        );
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
                    small_edit_contract: false,
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
                small_edit_contract: false,
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
                small_edit_contract: false,
            },
            &mut delivery,
        )
        .await;

        assert_eq!(completion.routing.cloud_request, CloudRequest::Required);
        assert!(!completion.cloud_attempted);
        assert_eq!(delivery_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            completion.compose_decision,
            CompositionDecision::FallbackBaseline
        );
        assert_eq!(
            completion.cloud_error,
            Some(DprCloudErrorClass::CredentialUnavailable)
        );
        assert_eq!(completion.delivery_flags, DeliveryFlags::dpr_default());
    }

    struct FormatEditCloud {
        calls: Arc<AtomicUsize>,
        saw_small_edit_contract: Arc<Mutex<Option<bool>>>,
        remaining_ms: Arc<AtomicU64>,
        candidate: FormatEditCandidate,
    }

    impl DprCloudBoundary for FormatEditCloud {
        fn attempt<'a>(
            &'a self,
            _credential: &'a Credential,
            request: DprCloudRequest<'a>,
            remaining: Duration,
        ) -> DprCloudFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.remaining_ms.store(
                u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX),
                Ordering::SeqCst,
            );
            *self.saw_small_edit_contract.lock().expect("flag lock") =
                Some(request.small_edit_contract);
            let candidate = self.candidate.clone();
            Box::pin(async move { DprCloudAttempt::format_edits(candidate) })
        }
    }

    fn format_edit_candidate(
        source: &str,
        start: usize,
        end: usize,
        before: &str,
        after: &str,
        kind: &str,
    ) -> FormatEditCandidate {
        let raw = serde_json::json!({
            "version": "1",
            "base_fingerprint": text_sha256_fingerprint(source),
            "edits": [{
                "start_utf8": start,
                "end_utf8": end,
                "before": before,
                "after": after,
                "kind": kind,
            }]
        });
        parse_format_edit_candidate_json(raw.to_string().as_bytes()).expect("format edits")
    }

    async fn run_timed_format_attempt(
        cloud: &dyn DprCloudBoundary,
        clock: &dyn DprPipelineClock,
        delivery: &mut dyn DeliveryAdapter,
    ) -> DprTransformCompletion {
        let source = "goal ship the rust parser";
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

        dpr_transform_and_deliver(
            DprTransformInput {
                selected_source: source,
                sources: &sources,
                source_selection: &selection,
                provider_state: ProviderState::SemanticDisagreement,
                policy: RenderingPolicy::Structured,
                english_eligible: true,
                surface_hint: None,
                process_hint: None,
                timing: None,
                protected_tokens: &[],
                cloud: DprCloudCapability::Ready {
                    boundary: cloud,
                    credential: &credential,
                },
                clock,
                small_edit_contract: true,
            },
            delivery,
        )
        .await
    }

    #[tokio::test]
    async fn small_edit_contract_delivers_host_applied_text_not_model_prose() {
        let source = "goal ship the rust parser";
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
        let calls = Arc::new(AtomicUsize::new(0));
        let saw_flag = Arc::new(Mutex::new(None));
        let cloud = FormatEditCloud {
            calls: Arc::clone(&calls),
            saw_small_edit_contract: Arc::clone(&saw_flag),
            remaining_ms: Arc::new(AtomicU64::new(0)),
            candidate: format_edit_candidate(source, 0, 4, "goal", "Goal:\n", "structure"),
        };
        let delivery_calls = Arc::new(AtomicUsize::new(0));
        let rendered = Arc::new(Mutex::new(Vec::new()));
        let mut delivery = RecordingDelivery {
            calls: Arc::clone(&delivery_calls),
            rendered: Arc::clone(&rendered),
            initiated_ms: None,
        };
        let clock = FixedClock(Duration::ZERO);

        let completion = dpr_transform_and_deliver(
            DprTransformInput {
                selected_source: source,
                sources: &sources,
                source_selection: &selection,
                provider_state: ProviderState::SemanticDisagreement,
                policy: RenderingPolicy::Structured,
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
                small_edit_contract: true,
            },
            &mut delivery,
        )
        .await;

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(*saw_flag.lock().expect("flag"), Some(true));
        assert_eq!(completion.rendered, "Goal:\n ship the rust parser");
        assert_eq!(
            rendered.lock().expect("delivery").as_slice(),
            ["Goal:\n ship the rust parser"]
        );
        assert_eq!(completion.compose_decision, CompositionDecision::Accept);
        assert_eq!(completion.delivery_flags, DeliveryFlags::dpr_default());
        assert!(completion.cloud_error.is_none());
        assert_ne!(completion.rendered, "invented polished model prose");
    }

    #[tokio::test]
    async fn formatting_provider_attempt_is_capped_at_4750ms() {
        let source = "goal ship the rust parser";
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
        let remaining_ms = Arc::new(AtomicU64::new(0));
        let cloud = FormatEditCloud {
            calls: Arc::new(AtomicUsize::new(0)),
            saw_small_edit_contract: Arc::new(Mutex::new(None)),
            remaining_ms: Arc::clone(&remaining_ms),
            candidate: format_edit_candidate(source, 0, 4, "goal", "Goal:\n", "structure"),
        };
        let delivery_calls = Arc::new(AtomicUsize::new(0));
        let mut delivery = RecordingDelivery {
            calls: Arc::clone(&delivery_calls),
            rendered: Arc::new(Mutex::new(Vec::new())),
            initiated_ms: None,
        };
        let clock = FixedClock(Duration::ZERO);

        let _completion = dpr_transform_and_deliver(
            DprTransformInput {
                selected_source: source,
                sources: &sources,
                source_selection: &selection,
                provider_state: ProviderState::SemanticDisagreement,
                policy: RenderingPolicy::Structured,
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
                small_edit_contract: true,
            },
            &mut delivery,
        )
        .await;

        assert_eq!(remaining_ms.load(Ordering::SeqCst), 4_750);
    }

    #[tokio::test]
    async fn formatting_host_reserve_prevents_zero_budget_provider_attempt() {
        let source = "goal ship the rust parser";
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
        let cloud_calls = Arc::new(AtomicUsize::new(0));
        let cloud = FormatEditCloud {
            calls: Arc::clone(&cloud_calls),
            saw_small_edit_contract: Arc::new(Mutex::new(None)),
            remaining_ms: Arc::new(AtomicU64::new(u64::MAX)),
            candidate: format_edit_candidate(source, 0, 4, "goal", "Goal:\n", "structure"),
        };
        let delivery_calls = Arc::new(AtomicUsize::new(0));
        let rendered = Arc::new(Mutex::new(Vec::new()));
        let mut delivery = RecordingDelivery {
            calls: Arc::clone(&delivery_calls),
            rendered: Arc::clone(&rendered),
            initiated_ms: None,
        };
        let clock = FixedClock(DPR_FORMAT_GATE - DPR_FORMAT_HOST_RESERVE);
        let baseline = organize_local_baseline(
            source,
            &LocalBaselineOptions {
                policy: RenderingPolicy::Structured,
                route: voisu_core::RenderingRoute::LocalWithOptionalCloud,
                timing: None,
            },
        );

        let completion = dpr_transform_and_deliver(
            DprTransformInput {
                selected_source: source,
                sources: &sources,
                source_selection: &selection,
                provider_state: ProviderState::SemanticDisagreement,
                policy: RenderingPolicy::Structured,
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
                small_edit_contract: true,
            },
            &mut delivery,
        )
        .await;

        assert_eq!(cloud_calls.load(Ordering::SeqCst), 0);
        assert_eq!(delivery_calls.load(Ordering::SeqCst), 1);
        assert_eq!(completion.rendered, baseline.rendered());
        assert_eq!(
            completion.compose_decision,
            CompositionDecision::FallbackBaseline
        );
        assert_eq!(
            completion.cloud_error,
            Some(DprCloudErrorClass::DeadlineExceeded)
        );
    }

    #[tokio::test]
    async fn formatting_provider_timeout_delivers_baseline_once() {
        let source = "goal ship the rust parser";
        let clock = ControlledClock::new(0);
        let cloud_calls = Arc::new(AtomicUsize::new(0));
        let remaining_ms = Arc::new(AtomicU64::new(0));
        let cloud = CountingFormatCloud {
            calls: Arc::clone(&cloud_calls),
            remaining_ms: Arc::clone(&remaining_ms),
            clock: clock.clone(),
            completes_at_ms: 4_751,
            honor_budget: true,
            outcome: CannedFormatCloudOutcome::Success(format_edit_candidate(
                source,
                0,
                4,
                "goal",
                "Goal:\n",
                "structure",
            )),
        };
        let delivery_calls = Arc::new(AtomicUsize::new(0));
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let mut delivery = RecordingDelivery {
            calls: Arc::clone(&delivery_calls),
            rendered: Arc::clone(&delivered),
            initiated_ms: None,
        };

        let completion = run_timed_format_attempt(&cloud, &clock, &mut delivery).await;

        assert_eq!(remaining_ms.load(Ordering::SeqCst), 4_750);
        assert_eq!(cloud_calls.load(Ordering::SeqCst), 1);
        assert_eq!(delivery_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            delivered.lock().expect("delivery").as_slice(),
            ["Goal ship the rust parser."]
        );
        assert_eq!(completion.rendered, "Goal ship the rust parser.");
        assert_eq!(
            completion.cloud_error,
            Some(DprCloudErrorClass::DeadlineExceeded)
        );
    }

    #[tokio::test]
    async fn formatting_response_arriving_after_five_second_gate_is_discarded() {
        let source = "goal ship the rust parser";
        let clock = ControlledClock::new(0);
        let cloud = CountingFormatCloud {
            calls: Arc::new(AtomicUsize::new(0)),
            remaining_ms: Arc::new(AtomicU64::new(0)),
            clock: clock.clone(),
            completes_at_ms: 5_001,
            honor_budget: false,
            outcome: CannedFormatCloudOutcome::Success(format_edit_candidate(
                source,
                0,
                4,
                "goal",
                "Goal:\n",
                "structure",
            )),
        };
        let delivery_calls = Arc::new(AtomicUsize::new(0));
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let mut delivery = RecordingDelivery {
            calls: Arc::clone(&delivery_calls),
            rendered: Arc::clone(&delivered),
            initiated_ms: None,
        };

        let completion = run_timed_format_attempt(&cloud, &clock, &mut delivery).await;

        assert_eq!(delivery_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            delivered.lock().expect("delivery").as_slice(),
            ["Goal ship the rust parser."]
        );
        assert_eq!(completion.rendered, "Goal ship the rust parser.");
        assert_eq!(
            completion.compose_decision,
            CompositionDecision::FallbackBaseline
        );
        assert_eq!(
            completion.cloud_error,
            Some(DprCloudErrorClass::DeadlineExceeded)
        );
    }

    #[tokio::test]
    async fn formatting_success_after_legacy_deadline_before_five_second_gate_is_accepted() {
        let source = "goal pls ship the rust parser";
        let clock = ControlledClock::new(0);
        let cloud_calls = Arc::new(AtomicUsize::new(0));
        let cloud = CountingFormatCloud {
            calls: Arc::clone(&cloud_calls),
            remaining_ms: Arc::new(AtomicU64::new(0)),
            clock: clock.clone(),
            completes_at_ms: 2_000,
            honor_budget: true,
            outcome: CannedFormatCloudOutcome::Success(format_edit_candidate(
                source,
                5,
                8,
                "pls",
                "Please",
                "bounded_wording",
            )),
        };
        let delivery_calls = Arc::new(AtomicUsize::new(0));
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let mut delivery = RecordingDelivery {
            calls: Arc::clone(&delivery_calls),
            rendered: Arc::clone(&delivered),
            initiated_ms: None,
        };
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
        let baseline = organize_local_baseline(
            source,
            &LocalBaselineOptions {
                policy: RenderingPolicy::Structured,
                route: voisu_core::RenderingRoute::LocalWithOptionalCloud,
                timing: None,
            },
        );

        let completion = dpr_transform_and_deliver(
            DprTransformInput {
                selected_source: source,
                sources: &sources,
                source_selection: &selection,
                provider_state: ProviderState::SemanticDisagreement,
                policy: RenderingPolicy::Structured,
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
                small_edit_contract: true,
            },
            &mut delivery,
        )
        .await;

        assert_eq!(cloud_calls.load(Ordering::SeqCst), 1);
        assert_eq!(delivery_calls.load(Ordering::SeqCst), 1);
        assert_eq!(completion.rendered, "goal Please ship the rust parser");
        assert_eq!(
            delivered.lock().expect("delivery").as_slice(),
            ["goal Please ship the rust parser"]
        );
        assert_ne!(completion.rendered, baseline.rendered());
        assert_eq!(completion.compose_decision, CompositionDecision::Accept);
        assert!(completion.cloud_error.is_none());
    }

    #[tokio::test]
    async fn formatting_rate_limit_delivers_baseline_once() {
        let clock = ControlledClock::new(0);
        let cloud = CountingFormatCloud {
            calls: Arc::new(AtomicUsize::new(0)),
            remaining_ms: Arc::new(AtomicU64::new(0)),
            clock: clock.clone(),
            completes_at_ms: 300,
            honor_budget: true,
            outcome: CannedFormatCloudOutcome::Failure(DprCloudErrorClass::RateLimited),
        };
        let delivery_calls = Arc::new(AtomicUsize::new(0));
        let mut delivery = RecordingDelivery {
            calls: Arc::clone(&delivery_calls),
            rendered: Arc::new(Mutex::new(Vec::new())),
            initiated_ms: None,
        };

        let completion = run_timed_format_attempt(&cloud, &clock, &mut delivery).await;

        assert_eq!(delivery_calls.load(Ordering::SeqCst), 1);
        assert_eq!(completion.rendered, "Goal ship the rust parser.");
        assert_eq!(
            completion.cloud_error,
            Some(DprCloudErrorClass::RateLimited)
        );
    }

    #[tokio::test]
    async fn invalid_small_edits_fall_back_to_local_baseline() {
        let source = "goal ship the rust parser";
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
        let mut stale = format_edit_candidate(source, 0, 4, "goal", "Goal:\n", "structure");
        stale.base_fingerprint = format!("sha256:{}", "0".repeat(64));
        let cloud = FormatEditCloud {
            calls: Arc::new(AtomicUsize::new(0)),
            saw_small_edit_contract: Arc::new(Mutex::new(None)),
            remaining_ms: Arc::new(AtomicU64::new(0)),
            candidate: stale,
        };
        let delivery_calls = Arc::new(AtomicUsize::new(0));
        let mut delivery = RecordingDelivery {
            calls: Arc::clone(&delivery_calls),
            rendered: Arc::new(Mutex::new(Vec::new())),
            initiated_ms: None,
        };
        let clock = FixedClock(Duration::ZERO);
        let baseline = organize_local_baseline(
            source,
            &LocalBaselineOptions {
                policy: RenderingPolicy::Structured,
                route: voisu_core::RenderingRoute::LocalWithOptionalCloud,
                timing: None,
            },
        );

        let completion = dpr_transform_and_deliver(
            DprTransformInput {
                selected_source: source,
                sources: &sources,
                source_selection: &selection,
                provider_state: ProviderState::SemanticDisagreement,
                policy: RenderingPolicy::Structured,
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
                small_edit_contract: true,
            },
            &mut delivery,
        )
        .await;

        assert_eq!(completion.rendered, baseline.rendered());
        assert_eq!(delivery_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            completion.compose_decision,
            CompositionDecision::FallbackBaseline
        );
        assert_eq!(
            completion.cloud_error,
            Some(DprCloudErrorClass::CandidateSchema)
        );
        assert_eq!(completion.delivery_flags, DeliveryFlags::dpr_default());
    }

    #[tokio::test]
    async fn derivation_path_ignores_format_edit_payloads() {
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
        let cloud = FormatEditCloud {
            calls: Arc::new(AtomicUsize::new(0)),
            saw_small_edit_contract: Arc::new(Mutex::new(None)),
            remaining_ms: Arc::new(AtomicU64::new(0)),
            candidate: format_edit_candidate(source, 0, 5, "hello", "Hello", "casing"),
        };
        let delivery_calls = Arc::new(AtomicUsize::new(0));
        let mut delivery = RecordingDelivery {
            calls: Arc::clone(&delivery_calls),
            rendered: Arc::new(Mutex::new(Vec::new())),
            initiated_ms: None,
        };
        let clock = FixedClock(Duration::ZERO);
        let baseline = organize_local_baseline(source, &LocalBaselineOptions::default());

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
                small_edit_contract: false,
            },
            &mut delivery,
        )
        .await;

        assert_eq!(completion.rendered, baseline.rendered());
        assert_eq!(delivery_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            completion.compose_decision,
            CompositionDecision::FallbackBaseline
        );
        assert_eq!(
            *cloud.saw_small_edit_contract.lock().expect("flag"),
            Some(false)
        );
    }

    #[tokio::test]
    async fn formatting_accepts_bounded_wording_absent_from_the_source() {
        let source = "goal pls ship the rust parser";
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
        let cloud = FormatEditCloud {
            calls: Arc::new(AtomicUsize::new(0)),
            saw_small_edit_contract: Arc::new(Mutex::new(None)),
            remaining_ms: Arc::new(AtomicU64::new(0)),
            candidate: format_edit_candidate(source, 5, 8, "pls", "Please", "bounded_wording"),
        };
        let mut delivery = RecordingDelivery {
            calls: Arc::new(AtomicUsize::new(0)),
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
                small_edit_contract: true,
            },
            &mut delivery,
        )
        .await;

        assert_eq!(completion.rendered, "goal Please ship the rust parser");
        assert_eq!(completion.compose_decision, CompositionDecision::Accept);
        assert!(completion.cloud_error.is_none());
    }

    #[tokio::test]
    async fn formatting_safety_rejects_protected_heading_and_artifact_to_baseline() {
        let source = "goal do not deploy https://example.test/a";
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
        let url_start = source.find("https://example.test/a").unwrap();
        let cloud = FormatEditCloud {
            calls: Arc::new(AtomicUsize::new(0)),
            saw_small_edit_contract: Arc::new(Mutex::new(None)),
            remaining_ms: Arc::new(AtomicU64::new(0)),
            candidate: format_edit_candidate(
                source,
                url_start,
                url_start + "https://example.test/a".len(),
                "https://example.test/a",
                "https://evil.test/a",
                "bounded_wording",
            ),
        };
        let delivery_calls = Arc::new(AtomicUsize::new(0));
        let mut delivery = RecordingDelivery {
            calls: Arc::clone(&delivery_calls),
            rendered: Arc::new(Mutex::new(Vec::new())),
            initiated_ms: None,
        };
        let clock = FixedClock(Duration::ZERO);
        let baseline = organize_local_baseline(
            source,
            &LocalBaselineOptions {
                policy: RenderingPolicy::Adaptive,
                route: voisu_core::RenderingRoute::LocalWithOptionalCloud,
                timing: None,
            },
        );

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
                protected_tokens: &["not", "https://example.test/a"],
                cloud: DprCloudCapability::Ready {
                    boundary: &cloud,
                    credential: &credential,
                },
                clock: &clock,
                small_edit_contract: true,
            },
            &mut delivery,
        )
        .await;

        assert_eq!(completion.rendered, baseline.rendered());
        assert_eq!(delivery_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            completion.compose_decision,
            CompositionDecision::FallbackBaseline
        );
        assert_eq!(
            completion.cloud_error,
            Some(DprCloudErrorClass::CandidateSchema)
        );
    }

    async fn deliver_local_spoken_marks(source: &str) -> DprTransformCompletion {
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
        let mut delivery = RecordingDelivery {
            calls: Arc::new(AtomicUsize::new(0)),
            rendered: Arc::new(Mutex::new(Vec::new())),
            initiated_ms: None,
        };
        let clock = FixedClock(Duration::ZERO);
        dpr_transform_and_deliver(
            DprTransformInput {
                selected_source: source,
                sources: &sources,
                source_selection: &selection,
                provider_state: ProviderState::SingleProvider,
                policy: RenderingPolicy::Adaptive,
                english_eligible: true,
                surface_hint: None,
                process_hint: None,
                timing: None,
                protected_tokens: &[],
                cloud: DprCloudCapability::Unavailable,
                clock: &clock,
                small_edit_contract: false,
            },
            &mut delivery,
        )
        .await
    }

    #[tokio::test]
    async fn spoken_dash_dash_delivers_without_cloud() {
        let completion = deliver_local_spoken_marks("cargo test dash dash workspace").await;
        assert_eq!(completion.rendered, "cargo test --workspace");
        assert!(!completion.cloud_attempted);
        assert_eq!(completion.routing.cloud_request, CloudRequest::NotAllowed);
        assert_eq!(
            completion.compose_decision,
            CompositionDecision::FallbackBaseline
        );
    }

    #[tokio::test]
    async fn spoken_mark_oracles_deliver_without_cloud() {
        for (source, expect) in [
            ("cargo test dash dash workspace", "cargo test --workspace"),
            (
                "create slash voisu core slash s r c slash lib dot rs",
                "create/voisu core/s r c/lib.rs",
            ),
            (
                "https colon slash slash example dot test slash a",
                "https://example.test/a",
            ),
            ("quote, leave this, unquote", "\"leave this\""),
        ] {
            let completion = deliver_local_spoken_marks(source).await;
            assert_eq!(completion.rendered, expect, "source={source:?}");
            assert!(!completion.cloud_attempted, "source={source:?}");
            assert_eq!(
                completion.compose_decision,
                CompositionDecision::FallbackBaseline,
                "source={source:?}"
            );
        }
    }

    #[tokio::test]
    async fn spoken_goal_delivers_ordinary_words_without_heading() {
        let completion =
            deliver_local_spoken_marks("Goal is to deploy the application right now").await;
        assert_eq!(
            completion.rendered,
            "Goal is to deploy the application right now."
        );
        assert!(!completion.rendered.contains("Goal:"));
        assert!(!completion.cloud_attempted);
        assert_eq!(
            completion.compose_decision,
            CompositionDecision::FallbackBaseline
        );
    }

    #[tokio::test]
    async fn spoken_first_second_third_delivers_numbered_lines_without_cloud() {
        let completion = deliver_local_spoken_marks(
            "first do the deployment second figure out the env variable third report to me",
        )
        .await;
        assert_eq!(
            completion.rendered,
            "1. Do the deployment\n2. Figure out the env variable\n3. Report to me"
        );
        assert!(!completion.cloud_attempted);
        assert_eq!(
            completion.compose_decision,
            CompositionDecision::FallbackBaseline
        );
    }

    struct ReadyFormatPath {
        completion: DprTransformCompletion,
        cloud_calls: usize,
        saw_small_edit: Option<bool>,
        delivery_calls: usize,
        delivered: Vec<String>,
    }

    async fn deliver_ready_format_path(
        source: &str,
        provider_state: ProviderState,
        policy: RenderingPolicy,
        small_edit_contract: bool,
    ) -> ReadyFormatPath {
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
        let cloud_calls = Arc::new(AtomicUsize::new(0));
        let saw_small_edit = Arc::new(Mutex::new(None));
        let cloud = FormatEditCloud {
            calls: Arc::clone(&cloud_calls),
            saw_small_edit_contract: Arc::clone(&saw_small_edit),
            remaining_ms: Arc::new(AtomicU64::new(0)),
            candidate: format_edit_candidate(
                source,
                0,
                source.len().min(4),
                &source[..source.len().min(4)],
                "X",
                "casing",
            ),
        };
        let delivery_calls = Arc::new(AtomicUsize::new(0));
        let rendered = Arc::new(Mutex::new(Vec::new()));
        let mut delivery = RecordingDelivery {
            calls: Arc::clone(&delivery_calls),
            rendered: Arc::clone(&rendered),
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
                surface_hint: None,
                process_hint: None,
                timing: None,
                protected_tokens: &[],
                cloud: DprCloudCapability::Ready {
                    boundary: &cloud,
                    credential: &credential,
                },
                clock: &clock,
                small_edit_contract,
            },
            &mut delivery,
        )
        .await;
        ReadyFormatPath {
            completion,
            cloud_calls: cloud_calls.load(Ordering::SeqCst),
            saw_small_edit: *saw_small_edit.lock().expect("flag"),
            delivery_calls: delivery_calls.load(Ordering::SeqCst),
            delivered: rendered.lock().expect("delivery").clone(),
        }
    }

    fn assert_formatting_cloud_skipped(run: &ReadyFormatPath, expected: &str) {
        assert_eq!(run.completion.rendered, expected);
        assert_eq!(run.delivered.as_slice(), [expected]);
        assert_eq!(run.cloud_calls, 0);
        assert!(!run.completion.cloud_attempted);
        assert!(run.completion.cloud_error.is_none());
        assert_eq!(run.delivery_calls, 1);
        assert_eq!(
            run.completion.compose_decision,
            CompositionDecision::FallbackBaseline
        );
        assert!(
            diagnostic_event_names(&run.completion).contains(&DprDiagnosticEventName::CloudSkipped)
        );
        assert!(
            !diagnostic_event_names(&run.completion)
                .contains(&DprDiagnosticEventName::CloudRequestStarted)
        );
    }

    #[tokio::test]
    async fn dash_dash_dispute_does_not_start_formatting_cloud() {
        let run = deliver_ready_format_path(
            "cargo test dash dash workspace",
            ProviderState::SemanticDisagreement,
            RenderingPolicy::Adaptive,
            true,
        )
        .await;
        assert_formatting_cloud_skipped(&run, "cargo test --workspace");
    }

    #[tokio::test]
    async fn first_second_third_dispute_does_not_start_formatting_cloud() {
        let run = deliver_ready_format_path(
            "first do the deployment second figure out the env variable third report to me",
            ProviderState::SemanticDisagreement,
            RenderingPolicy::Adaptive,
            true,
        )
        .await;
        assert_formatting_cloud_skipped(
            &run,
            "1. Do the deployment\n2. Figure out the env variable\n3. Report to me",
        );
    }

    #[tokio::test]
    async fn ordinary_disputed_chat_does_not_start_formatting_cloud() {
        let run = deliver_ready_format_path(
            "hey how are you doing today",
            ProviderState::SemanticDisagreement,
            RenderingPolicy::Adaptive,
            true,
        )
        .await;
        assert_formatting_cloud_skipped(&run, "Hey, how are you doing today.");
    }

    #[tokio::test]
    async fn leftover_goal_flag_on_may_start_formatting_cloud() {
        let run = deliver_ready_format_path(
            "Goal is to deploy the application right now",
            ProviderState::SemanticDisagreement,
            RenderingPolicy::Adaptive,
            true,
        )
        .await;
        assert_eq!(run.cloud_calls, 1);
        assert!(run.completion.cloud_attempted);
        assert_eq!(run.saw_small_edit, Some(true));
        assert_eq!(run.delivery_calls, 1);
    }

    #[tokio::test]
    async fn leftover_goal_flag_off_does_not_take_formatting_contract() {
        let run = deliver_ready_format_path(
            "goal ship the rust parser",
            ProviderState::SemanticDisagreement,
            RenderingPolicy::Adaptive,
            false,
        )
        .await;
        assert_eq!(run.cloud_calls, 1);
        assert!(run.completion.cloud_attempted);
        assert_eq!(run.saw_small_edit, Some(false));
        assert_eq!(run.delivery_calls, 1);
        assert_eq!(run.completion.rendered, "Goal ship the rust parser.");
    }

    #[tokio::test]
    async fn format_edit_cannot_smash_converted_url() {
        let source = "goal https colon slash slash example dot test slash a";
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
        let converted = "Goal https://example.test/a.";
        let url_start = converted.find("https://example.test/a").unwrap();
        let cloud = FormatEditCloud {
            calls: Arc::new(AtomicUsize::new(0)),
            saw_small_edit_contract: Arc::new(Mutex::new(None)),
            remaining_ms: Arc::new(AtomicU64::new(0)),
            candidate: format_edit_candidate(
                converted,
                url_start,
                url_start + "https://example.test/a".len(),
                "https://example.test/a",
                "https://evil.test/a",
                "bounded_wording",
            ),
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
                small_edit_contract: true,
            },
            &mut delivery,
        )
        .await;

        assert_eq!(completion.rendered, "Goal https://example.test/a.");
        assert!(!completion.rendered.contains("evil"));
        assert_eq!(delivery_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            completion.compose_decision,
            CompositionDecision::FallbackBaseline
        );
        assert_eq!(
            completion.cloud_error,
            Some(DprCloudErrorClass::CandidateSchema)
        );
    }

    #[tokio::test]
    async fn format_edit_cannot_smash_converted_dash_dash() {
        let source = "goal cargo test dash dash workspace";
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
        let converted = "goal cargo test --workspace";
        let flag_start = converted.find("--workspace").unwrap();
        let cloud = FormatEditCloud {
            calls: Arc::new(AtomicUsize::new(0)),
            saw_small_edit_contract: Arc::new(Mutex::new(None)),
            remaining_ms: Arc::new(AtomicU64::new(0)),
            candidate: format_edit_candidate(
                converted,
                flag_start,
                flag_start + "--workspace".len(),
                "--workspace",
                "--evil",
                "bounded_wording",
            ),
        };
        let mut delivery = RecordingDelivery {
            calls: Arc::new(AtomicUsize::new(0)),
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
                small_edit_contract: true,
            },
            &mut delivery,
        )
        .await;

        assert_eq!(completion.rendered, "goal cargo test --workspace");
        assert!(!completion.rendered.contains("evil"));
        assert_eq!(
            completion.compose_decision,
            CompositionDecision::FallbackBaseline
        );
        assert_eq!(
            completion.cloud_error,
            Some(DprCloudErrorClass::CandidateSchema)
        );
    }

    #[tokio::test]
    async fn accepted_spoken_source_edit_still_converts_remaining_marks() {
        let source = "goal look at example dot com";
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
        let look_start = source.find("look").unwrap();
        let cloud = FormatEditCloud {
            calls: Arc::new(AtomicUsize::new(0)),
            saw_small_edit_contract: Arc::new(Mutex::new(None)),
            remaining_ms: Arc::new(AtomicU64::new(u64::MAX)),
            candidate: format_edit_candidate(
                source,
                look_start,
                look_start + 4,
                "look",
                "Look",
                "casing",
            ),
        };
        let mut delivery = RecordingDelivery {
            calls: Arc::new(AtomicUsize::new(0)),
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
                small_edit_contract: true,
            },
            &mut delivery,
        )
        .await;

        assert!(
            completion.rendered.contains("example.com"),
            "accepted spoken-source edit must not leave spoken dot, got {:?}",
            completion.rendered
        );
        assert!(
            !completion.rendered.to_ascii_lowercase().contains("dot"),
            "spoken dot must convert after the accepted edit, got {:?}",
            completion.rendered
        );
        assert_eq!(completion.compose_decision, CompositionDecision::Accept);
    }

    #[tokio::test]
    async fn spoken_source_edit_cannot_rewrite_words_that_become_protected() {
        let source = "goal cargo test dash dash workspace";
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
        let workspace_start = source.find("workspace").unwrap();
        let cloud = FormatEditCloud {
            calls: Arc::new(AtomicUsize::new(0)),
            saw_small_edit_contract: Arc::new(Mutex::new(None)),
            remaining_ms: Arc::new(AtomicU64::new(u64::MAX)),
            candidate: format_edit_candidate(
                source,
                workspace_start,
                workspace_start + "workspace".len(),
                "workspace",
                "evil",
                "bounded_wording",
            ),
        };
        let mut delivery = RecordingDelivery {
            calls: Arc::new(AtomicUsize::new(0)),
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
                small_edit_contract: true,
            },
            &mut delivery,
        )
        .await;

        assert_eq!(completion.rendered, "goal cargo test --workspace");
        assert!(!completion.rendered.contains("evil"));
        assert_eq!(
            completion.compose_decision,
            CompositionDecision::FallbackBaseline
        );
        assert_eq!(
            completion.cloud_error,
            Some(DprCloudErrorClass::CandidateSchema)
        );
    }
}
