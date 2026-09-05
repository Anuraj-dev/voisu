//! Deadline-bounded structured cloud client for Developer Prompt Rendering.
//!
//! This module owns DPR-T4's single Groq HTTP attempt and provider envelope.
//! The default job still parses the #139 [`StructuredCandidate`]. The flagged
//! formatting job requests the small-edit contract instead and never treats a
//! free-form model string as Delivery text.

use std::fmt;
use std::sync::Once;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use voisu_core::{
    CLOSED_CONVERSIONS, CLOSED_FORMAT_EDIT_KINDS, CLOSED_SOURCE_SELECTION_REASONS,
    CLOSED_STRUCTURED_LABELS, ComposeSource, Credential, FORMAT_EDIT_CONTRACT_VERSION,
    FormatEditCandidate, MAX_COMPOSE_CONVERSIONS, MAX_COMPOSE_DERIVATION_SPANS,
    MAX_COMPOSE_FIELD_UTF8_BYTES, MAX_COMPOSE_LABELS, MAX_COMPOSE_REMOVALS,
    MAX_FORMAT_EDIT_FIELD_UTF8_BYTES, MAX_FORMAT_EDITS, Provider, RenderingPolicy, SecretStore,
    SourceSelection, StructuredCandidate, parse_format_edit_candidate_json,
    parse_structured_candidate_json,
};

use crate::system::{endpoint_authority_is_allowed, parsed_host_is_loopback};

/// Preferred in-budget candidate from the approved #140 matrix.
pub const DPR_GROQ_MODEL: &str = "openai/gpt-oss-20b";
pub const DPR_GROQ_REASONING_EFFORT: &str = "low";
pub const DPR_FORMAT_GROQ_MODEL: &str = "qwen/qwen3.6-27b";
pub const DPR_FORMAT_GROQ_REASONING_EFFORT: &str = "none";
pub const DPR_FORMAT_GROQ_REASONING_FORMAT: &str = "hidden";
pub const DPR_GROQ_ENDPOINT: &str = "https://api.groq.com/openai/v1/chat/completions";

/// Retained only for explicit evaluation or a future measured in-budget path.
pub const DPR_GEMINI_FLASH_LITE_MODEL: &str = "gemini-3.5-flash-lite";
pub const DPR_GEMINI_FLASH_MODEL: &str = "gemini-3.6-flash";

/// Provider envelope cap. The nested candidate remains independently limited
/// to `MAX_COMPOSE_CANDIDATE_BYTES` by the sole compose parser.
pub const MAX_DPR_PROVIDER_ENVELOPE_BYTES: usize = 73_728;
pub const MAX_DPR_SOURCE_UTF8_BYTES: usize = 100_000;
pub const MAX_DPR_SOURCES: usize = 2;
pub const MAX_DPR_PROTECTED_TOKENS: usize = 128;

/// #140 found no sole production-ready cloud default. Local baseline remains
/// the Delivery authority whenever this optional attempt is absent or rejected.
pub const DPR_HAS_SOLE_PRODUCTION_READY_CLOUD_DEFAULT: bool = false;

const DPR_MAX_COMPLETION_TOKENS: u32 = 4_096;

const SYSTEM_INSTRUCTION: &str = "You organize English speech into text. Preserve wording and spoken grammar. Do not invent requirements, paraphrases, explanations, or technical assumptions. Allowed: punctuation/casing, clear filler or clear backtrack removals, closed symbol/format cue conversions, layout (natural / multi-paragraph / numbered / structured_sections), and closed labels only when Structured or clearly licensed. Never auto-send. Return structured JSON decisions only — never a free-form polished string as sole authority. Groq path: same organize-only contract. Do not run grammar correction.";

const RESPONSE_INSTRUCTION: &str = "Return the smallest valid candidate under the supplied JSON schema. Derivation must be source ordered and complete: concatenating output_text reconstructs the proposal, and every non-layout span proves its source_text against the named provider. Uncertain backtrack preserves words. Uncertain layout stays natural. Use only the supplied closed labels and conversions. One call only.";

const FORMAT_EDIT_SYSTEM_INSTRUCTION: &str = "You propose localized formatting edits for English speech. Preserve wording and spoken grammar. Do not invent requirements, paraphrases, explanations, or technical assumptions. Allowed kinds only: punctuation, casing, whitespace_layout, filler_removal, clear_backtrack_removal, quote_conversion, structure, bounded_wording. Never return a polished transcript or free-form Delivery string. Reconciliation is a different job — do not select sources or emit derivation spans. Return the small-edit JSON contract only.";

const FORMAT_EDIT_RESPONSE_INSTRUCTION: &str = "Return the smallest valid edit list under the supplied JSON contract. Each edit must name a UTF-8 range on the supplied base, the exact before text, the replacement after text, and a closed kind. Empty edits means leave the base unchanged. One call only. Never emit a rendered or output_text field.";

/// Closed diagnostic classification. Variants intentionally carry no provider
/// response body, request content, transcript, or credential.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DprCloudErrorClass {
    CredentialUnavailable,
    DeadlineExceeded,
    RequestInvalid,
    HttpClient,
    Http4xx,
    RateLimited,
    Http5xx,
    Transport,
    ResponseTooLarge,
    ProviderEnvelope,
    CandidateSchema,
}

impl DprCloudErrorClass {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CredentialUnavailable => "credential_unavailable",
            Self::DeadlineExceeded => "deadline_exceeded",
            Self::RequestInvalid => "request_invalid",
            Self::HttpClient => "http_client",
            Self::Http4xx => "http_4xx",
            Self::RateLimited => "rate_limited",
            Self::Http5xx => "http_5xx",
            Self::Transport => "transport",
            Self::ResponseTooLarge => "response_too_large",
            Self::ProviderEnvelope => "provider_envelope",
            Self::CandidateSchema => "candidate_schema",
        }
    }
}

impl fmt::Display for DprCloudErrorClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One T4 attempt result. A failed attempt always has both payloads `None`; T5
/// maps that absence to the local baseline. The small-edit payload is parsed
/// JSON only — the host still has to apply it before Delivery.
pub struct DprCloudAttempt {
    candidate: Option<StructuredCandidate>,
    format_edits: Option<FormatEditCandidate>,
    error: Option<DprCloudErrorClass>,
}

impl DprCloudAttempt {
    #[must_use]
    pub fn success(candidate: StructuredCandidate) -> Self {
        Self {
            candidate: Some(candidate),
            format_edits: None,
            error: None,
        }
    }

    #[must_use]
    pub fn format_edits(candidate: FormatEditCandidate) -> Self {
        Self {
            candidate: None,
            format_edits: Some(candidate),
            error: None,
        }
    }

    #[must_use]
    pub const fn failure(error: DprCloudErrorClass) -> Self {
        Self {
            candidate: None,
            format_edits: None,
            error: Some(error),
        }
    }

    #[must_use]
    pub const fn candidate(&self) -> Option<&StructuredCandidate> {
        self.candidate.as_ref()
    }

    #[must_use]
    pub const fn format_edits_candidate(&self) -> Option<&FormatEditCandidate> {
        self.format_edits.as_ref()
    }

    #[must_use]
    pub const fn error(&self) -> Option<DprCloudErrorClass> {
        self.error
    }

    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        Option<StructuredCandidate>,
        Option<FormatEditCandidate>,
        Option<DprCloudErrorClass>,
    ) {
        (self.candidate, self.format_edits, self.error)
    }
}

/// Exact host evidence sent to the structured model. This type deliberately
/// has no `Debug`: transcripts and protected tokens must not become loggable by
/// formatting an orchestration input.
pub struct DprCloudRequest<'a> {
    pub sources: &'a [ComposeSource],
    pub source_selection: &'a SourceSelection,
    pub selected_source: &'a str,
    pub base_fingerprint: &'a str,
    pub policy: RenderingPolicy,
    pub protected_tokens: &'a [&'a str],
    /// When true, request/parse the small-edit formatting contract. When false,
    /// keep the existing #139 derivation job. Off by default.
    pub small_edit_contract: bool,
}

/// Process-owned DPR HTTP capability. Clone is cheap (`reqwest::Client` is
/// internally shared); no per-attempt task, retry, backoff, or provider fallback
/// is created.
#[derive(Clone, Debug)]
pub struct DprCloudClient {
    client: reqwest::Client,
    endpoint: String,
    max_body_bytes: usize,
}

impl DprCloudClient {
    /// Groq is preferred when a cloud attempt is already scheduled, but this is
    /// not a sole production-ready model default (#140 residual R3).
    pub fn groq() -> Result<Self, DprCloudErrorClass> {
        Self::with_endpoint(DPR_GROQ_ENDPOINT)
    }

    /// Loopback injection is for hermetic HTTP contract tests.
    pub fn with_endpoint(endpoint: impl Into<String>) -> Result<Self, DprCloudErrorClass> {
        Self::with_config(endpoint, MAX_DPR_PROVIDER_ENVELOPE_BYTES)
    }

    fn with_config(
        endpoint: impl Into<String>,
        max_body_bytes: usize,
    ) -> Result<Self, DprCloudErrorClass> {
        let endpoint = endpoint.into();
        if !endpoint_is_allowed(&endpoint) || max_body_bytes == 0 {
            return Err(DprCloudErrorClass::HttpClient);
        }
        ensure_rustls_ring_provider();
        let client = reqwest::Client::builder()
            // Reqwest 0.13 otherwise retries selected low-level protocol NACKs.
            .retry(reqwest::retry::never())
            // A redirect is a second HTTP request and could move credentialed
            // traffic away from the fixed endpoint.
            .redirect(reqwest::redirect::Policy::none())
            .pool_max_idle_per_host(2)
            .build()
            .map_err(|_| DprCloudErrorClass::HttpClient)?;
        Ok(Self {
            client,
            endpoint,
            max_body_bytes,
        })
    }

    /// Make exactly one Groq HTTP request inside the caller-owned remaining
    /// Delivery budget. Dropping/timeout cancels the request future; there is no
    /// late-result callback and no API capable of replacing delivered text.
    pub async fn attempt_groq(
        &self,
        credential: &Credential,
        request: &DprCloudRequest<'_>,
        remaining: Duration,
    ) -> DprCloudAttempt {
        if remaining.is_zero() {
            return DprCloudAttempt::failure(DprCloudErrorClass::DeadlineExceeded);
        }

        let started = Instant::now();
        if !request_is_bounded(request) {
            return DprCloudAttempt::failure(DprCloudErrorClass::RequestInvalid);
        }

        let body = build_groq_request(request);
        let remaining = remaining.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return DprCloudAttempt::failure(DprCloudErrorClass::DeadlineExceeded);
        }
        let operation = async {
            let response = self
                .client
                .post(&self.endpoint)
                .header(
                    reqwest::header::AUTHORIZATION,
                    format!("Bearer {}", credential.expose_to_boundary()),
                )
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .header(reqwest::header::ACCEPT, "application/json")
                .json(&body)
                .send()
                .await
                .map_err(|_| DprCloudErrorClass::Transport)?;

            classify_status(response.status().as_u16())?;
            let envelope = read_bounded(response, self.max_body_bytes).await?;
            extract_payload(&envelope, request.small_edit_contract)
        };

        match tokio::time::timeout(remaining, operation).await {
            Err(_) => DprCloudAttempt::failure(DprCloudErrorClass::DeadlineExceeded),
            Ok(Err(error)) => DprCloudAttempt::failure(error),
            Ok(Ok(payload)) => match payload {
                ParsedPayload::Derivation(candidate) => DprCloudAttempt::success(candidate),
                ParsedPayload::FormatEdits(candidate) => DprCloudAttempt::format_edits(candidate),
            },
        }
    }
}

fn request_is_bounded(request: &DprCloudRequest<'_>) -> bool {
    if request.sources.is_empty()
        || request.sources.len() > MAX_DPR_SOURCES
        || request.selected_source.len() > MAX_DPR_SOURCE_UTF8_BYTES
        || request.base_fingerprint.len() != "sha256:".len() + 64
        || !request.base_fingerprint.starts_with("sha256:")
        || !request.base_fingerprint["sha256:".len()..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || !CLOSED_SOURCE_SELECTION_REASONS.contains(&request.source_selection.reason.as_str())
        || request.source_selection.reason.len() > MAX_COMPOSE_FIELD_UTF8_BYTES
        || request.protected_tokens.len() > MAX_DPR_PROTECTED_TOKENS
    {
        return false;
    }
    if !request.sources.iter().any(|source| {
        source.available && source.provider == request.source_selection.selected_provider
    }) {
        return false;
    }
    request.sources.iter().all(|source| {
        source.text.len() <= MAX_DPR_SOURCE_UTF8_BYTES
            && source.provider.as_str().len() <= MAX_COMPOSE_FIELD_UTF8_BYTES
    }) && request
        .protected_tokens
        .iter()
        .all(|token| !token.is_empty() && token.len() <= MAX_COMPOSE_FIELD_UTF8_BYTES)
}

/// Resolve the existing Groq credential seam without exposing provider errors,
/// values, or keyring output to T4 diagnostics.
pub fn load_groq_credential(store: &mut dyn SecretStore) -> Result<Credential, DprCloudErrorClass> {
    store
        .load(Provider::Groq)
        .map_err(|_| DprCloudErrorClass::CredentialUnavailable)
}

fn build_groq_request(request: &DprCloudRequest<'_>) -> Value {
    if request.small_edit_contract {
        return build_format_edit_request(request);
    }
    let sources: Vec<Value> = request
        .sources
        .iter()
        .map(|source| {
            json!({
                "provider": source.provider.as_str(),
                "available": source.available,
                "text": source.text,
                "primary": source.primary,
            })
        })
        .collect();

    let payload = json!({
        "sources": sources,
        "host_selection": {
            "selected_provider": request.source_selection.selected_provider.as_str(),
            "reason": request.source_selection.reason,
        },
        "base_fingerprint": request.base_fingerprint,
        "policy": request.policy.as_str(),
        "protected_tokens": request.protected_tokens,
        "closed_labels": CLOSED_STRUCTURED_LABELS,
        "closed_conversions": CLOSED_CONVERSIONS,
        "response_instruction": RESPONSE_INSTRUCTION,
    });

    json!({
        "model": DPR_GROQ_MODEL,
        "reasoning_effort": DPR_GROQ_REASONING_EFFORT,
        "temperature": 0,
        "stream": false,
        "max_completion_tokens": DPR_MAX_COMPLETION_TOKENS,
        "response_format": {
            "type": "json_schema",
            "json_schema": {
                "name": "voisu_dpr_structured_candidate",
                "strict": true,
                "schema": candidate_schema(),
            }
        },
        "messages": [
            {"role": "system", "content": SYSTEM_INSTRUCTION},
            {"role": "user", "content": payload.to_string()},
        ],
    })
}

fn build_format_edit_request(request: &DprCloudRequest<'_>) -> Value {
    let payload = json!({
        "version": FORMAT_EDIT_CONTRACT_VERSION,
        "base_fingerprint": request.base_fingerprint,
        "base_text": request.selected_source,
        "policy": request.policy.as_str(),
        "closed_kinds": CLOSED_FORMAT_EDIT_KINDS,
        "response_schema": format_edit_schema(),
        "response_instruction": FORMAT_EDIT_RESPONSE_INSTRUCTION,
    });

    json!({
        "model": DPR_FORMAT_GROQ_MODEL,
        "reasoning_effort": DPR_FORMAT_GROQ_REASONING_EFFORT,
        "reasoning_format": DPR_FORMAT_GROQ_REASONING_FORMAT,
        "temperature": 0,
        "stream": false,
        "max_completion_tokens": DPR_MAX_COMPLETION_TOKENS,
        "response_format": {
            "type": "json_object",
        },
        "messages": [
            {"role": "system", "content": FORMAT_EDIT_SYSTEM_INSTRUCTION},
            {"role": "user", "content": payload.to_string()},
        ],
    })
}

fn candidate_schema() -> Value {
    let nullable_enum = |values: &[&str]| {
        json!({
            "anyOf": [
                {"type": "string", "enum": values},
                {"type": "null"}
            ]
        })
    };
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["schema_version", "base_fingerprint", "reconciliation", "removals", "conversions", "layout", "labels", "derivation"],
        "properties": {
            "schema_version": {"type": "string", "const": "1"},
            "base_fingerprint": {"type": "string", "pattern": "^sha256:[0-9a-f]{64}$"},
            "reconciliation": {
                "type": "object", "additionalProperties": false,
                "required": ["selected_provider", "reason"],
                "properties": {
                    "selected_provider": {"type": "string", "enum": ["provider_a", "provider_b"]},
                    "reason": {"type": "string", "enum": CLOSED_SOURCE_SELECTION_REASONS}
                }
            },
            "removals": {
                "type": "array", "maxItems": MAX_COMPOSE_REMOVALS,
                "items": {
                    "type": "object", "additionalProperties": false,
                    "required": ["kind", "certainty", "source_provider", "source_span_text"],
                    "properties": {
                        "kind": {"type": "string", "enum": ["filler", "backtrack"]},
                        "certainty": {"type": "string", "enum": ["clear", "uncertain"]},
                        "source_provider": {"type": "string", "enum": ["provider_a", "provider_b"]},
                        "source_span_text": {"type": "string", "maxLength": MAX_COMPOSE_FIELD_UTF8_BYTES}
                    }
                }
            },
            "conversions": {
                "type": "array", "maxItems": MAX_COMPOSE_CONVERSIONS,
                "items": {
                    "type": "object", "additionalProperties": false,
                    "required": ["id", "source_provider", "source_span_text"],
                    "properties": {
                        "id": {"type": "string", "enum": CLOSED_CONVERSIONS},
                        "source_provider": {"type": "string", "enum": ["provider_a", "provider_b"]},
                        "source_span_text": {"type": "string", "maxLength": MAX_COMPOSE_FIELD_UTF8_BYTES}
                    }
                }
            },
            "layout": {
                "type": "object", "additionalProperties": false,
                "required": ["decision", "certainty"],
                "properties": {
                    "decision": {"type": "string", "enum": ["natural", "multi_paragraph", "numbered", "structured_sections"]},
                    "certainty": {"type": "string", "enum": ["clear", "uncertain"]}
                }
            },
            "labels": {
                "type": "array", "maxItems": MAX_COMPOSE_LABELS,
                "items": {
                    "type": "object", "additionalProperties": false,
                    "required": ["label", "source_provider", "source_span_text"],
                    "properties": {
                        "label": {"type": "string", "enum": CLOSED_STRUCTURED_LABELS},
                        "source_provider": {"type": "string", "enum": ["provider_a", "provider_b"]},
                        "source_span_text": {"type": "string", "maxLength": MAX_COMPOSE_FIELD_UTF8_BYTES}
                    }
                }
            },
            "derivation": {
                "type": "array", "minItems": 1, "maxItems": MAX_COMPOSE_DERIVATION_SPANS,
                "items": {
                    "type": "object", "additionalProperties": false,
                    "required": ["kind", "source_provider", "source_text", "output_text", "conversion_id", "label"],
                    "properties": {
                        "kind": {"type": "string", "enum": ["keep", "remove", "convert", "label", "layout_break"]},
                        "source_provider": nullable_enum(&["provider_a", "provider_b"]),
                        "source_text": {"type": "string", "maxLength": MAX_COMPOSE_FIELD_UTF8_BYTES},
                        "output_text": {"type": "string", "maxLength": MAX_COMPOSE_FIELD_UTF8_BYTES},
                        "conversion_id": nullable_enum(CLOSED_CONVERSIONS),
                        "label": nullable_enum(CLOSED_STRUCTURED_LABELS)
                    }
                }
            }
        }
    })
}

fn format_edit_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["version", "base_fingerprint", "edits"],
        "properties": {
            "version": {"type": "string", "const": FORMAT_EDIT_CONTRACT_VERSION},
            "base_fingerprint": {"type": "string", "pattern": "^sha256:[0-9a-f]{64}$"},
            "edits": {
                "type": "array",
                "maxItems": MAX_FORMAT_EDITS,
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["start_utf8", "end_utf8", "before", "after", "kind"],
                    "properties": {
                        "start_utf8": {"type": "integer", "minimum": 0},
                        "end_utf8": {"type": "integer", "minimum": 0},
                        "before": {"type": "string", "maxLength": MAX_FORMAT_EDIT_FIELD_UTF8_BYTES},
                        "after": {"type": "string", "maxLength": MAX_FORMAT_EDIT_FIELD_UTF8_BYTES},
                        "kind": {"type": "string", "enum": CLOSED_FORMAT_EDIT_KINDS}
                    }
                }
            }
        }
    })
}

fn classify_status(status: u16) -> Result<(), DprCloudErrorClass> {
    match status {
        200..=299 => Ok(()),
        429 => Err(DprCloudErrorClass::RateLimited),
        400..=499 => Err(DprCloudErrorClass::Http4xx),
        500..=599 => Err(DprCloudErrorClass::Http5xx),
        _ => Err(DprCloudErrorClass::Transport),
    }
}

async fn read_bounded(
    mut response: reqwest::Response,
    max_body_bytes: usize,
) -> Result<Vec<u8>, DprCloudErrorClass> {
    if response
        .content_length()
        .is_some_and(|length| length > max_body_bytes as u64)
    {
        return Err(DprCloudErrorClass::ResponseTooLarge);
    }
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if body.len().saturating_add(chunk.len()) > max_body_bytes {
                    return Err(DprCloudErrorClass::ResponseTooLarge);
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(_) => return Err(DprCloudErrorClass::Transport),
        }
    }
    Ok(body)
}

enum ParsedPayload {
    Derivation(StructuredCandidate),
    FormatEdits(FormatEditCandidate),
}

fn extract_payload(
    envelope: &[u8],
    small_edit_contract: bool,
) -> Result<ParsedPayload, DprCloudErrorClass> {
    let content = extract_message_content(envelope)?;
    if small_edit_contract {
        parse_format_edit_candidate_json(content.as_bytes())
            .map(ParsedPayload::FormatEdits)
            .map_err(|_| DprCloudErrorClass::CandidateSchema)
    } else {
        parse_structured_candidate_json(content.as_bytes())
            .map(ParsedPayload::Derivation)
            .ok_or(DprCloudErrorClass::CandidateSchema)
    }
}

fn extract_message_content(envelope: &[u8]) -> Result<String, DprCloudErrorClass> {
    let root: Value =
        serde_json::from_slice(envelope).map_err(|_| DprCloudErrorClass::ProviderEnvelope)?;
    let choices = root
        .get("choices")
        .and_then(Value::as_array)
        .filter(|choices| choices.len() == 1)
        .ok_or(DprCloudErrorClass::ProviderEnvelope)?;
    let message = choices[0]
        .get("message")
        .and_then(Value::as_object)
        .ok_or(DprCloudErrorClass::ProviderEnvelope)?;
    if message.get("refusal").is_some_and(|value| !value.is_null()) {
        return Err(DprCloudErrorClass::ProviderEnvelope);
    }
    message
        .get("content")
        .and_then(Value::as_str)
        .filter(|content| !content.trim().is_empty())
        .map(str::to_owned)
        .ok_or(DprCloudErrorClass::ProviderEnvelope)
}

fn ensure_rustls_ring_provider() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Production HTTPS always; plain HTTP only on loopback (test injection). The
/// URL is parsed so the loopback decision is the real host, never a prefix of
/// the raw authority — `http://localhost:8080@attacker.example/` is attacker
/// .example carrying userinfo, not loopback.
fn endpoint_is_allowed(endpoint: &str) -> bool {
    if endpoint.is_empty() || endpoint.contains(['\n', '\r', '\0']) {
        return false;
    }
    let Ok(url) = url::Url::parse(endpoint) else {
        return false;
    };
    endpoint_authority_is_allowed(&url)
        && match url.scheme() {
            "https" => true,
            "http" => parsed_host_is_loopback(&url),
            _ => false,
        }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use voisu_core::{
        BoundaryError, BoundaryKind, CloudOutcome, ComposeInput, ComposeSource,
        CompositionDecision, LocalBaselineOptions, Provider, SecretStore, SttProvider,
        compose_structured_candidate, organize_local_baseline,
    };

    use super::*;

    const HELLO_FINGERPRINT: &str =
        "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    fn valid_candidate() -> Value {
        json!({
            "schema_version": "1",
            "base_fingerprint": HELLO_FINGERPRINT,
            "reconciliation": {"selected_provider": "provider_a", "reason": "only_available"},
            "removals": [],
            "conversions": [],
            "layout": {"decision": "natural", "certainty": "clear"},
            "labels": [],
            "derivation": [{
                "kind": "keep",
                "source_provider": "provider_a",
                "source_text": "hello",
                "output_text": "hello",
                "conversion_id": null,
                "label": null
            }]
        })
    }

    fn provider_response(candidate: Value) -> String {
        json!({
            "choices": [{"message": {"role": "assistant", "content": candidate.to_string()}}]
        })
        .to_string()
    }

    fn request_fixture<'a>(
        sources: &'a [ComposeSource],
        selection: &'a SourceSelection,
        protected: &'a [&'a str],
    ) -> DprCloudRequest<'a> {
        DprCloudRequest {
            sources,
            source_selection: selection,
            selected_source: "hello",
            base_fingerprint: HELLO_FINGERPRINT,
            policy: RenderingPolicy::Adaptive,
            protected_tokens: protected,
            small_edit_contract: false,
        }
    }

    async fn canned_server(
        status: u16,
        response_body: String,
        delay: Duration,
    ) -> (String, oneshot::Receiver<Vec<u8>>, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let (request_tx, request_rx) = oneshot::channel();
        let count = Arc::new(AtomicUsize::new(0));
        let count_for_task = Arc::clone(&count);
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            count_for_task.fetch_add(1, Ordering::SeqCst);
            let request = read_request(&mut socket).await;
            let _ = request_tx.send(request);
            tokio::time::sleep(delay).await;
            let reason = if status == 200 { "OK" } else { "Error" };
            let reply = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            let _ = socket.write_all(reply.as_bytes()).await;
        });
        (
            format!("http://{address}/openai/v1/chat/completions"),
            request_rx,
            count,
        )
    }

    async fn read_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut chunk = [0_u8; 2048];
        let mut expected = None;
        loop {
            let read = socket.read(&mut chunk).await.expect("read");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
            if expected.is_none() {
                if let Some(header_end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let header_end = header_end + 4;
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(str::trim)
                                .and_then(|number| number.parse::<usize>().ok())
                        })
                        .expect("content length");
                    expected = Some(header_end + content_length);
                }
            }
            if expected.is_some_and(|length| request.len() >= length) {
                break;
            }
        }
        request
    }

    fn body_from_request(request: &[u8]) -> Value {
        let body_start = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4)
            .expect("headers");
        serde_json::from_slice(&request[body_start..]).expect("request JSON")
    }

    #[tokio::test]
    async fn happy_json_returns_typed_candidate_and_exact_request_contract() {
        let secret = "controlled-secret-not-for-logs";
        let (endpoint, request_rx, count) =
            canned_server(200, provider_response(valid_candidate()), Duration::ZERO).await;
        let client = DprCloudClient::with_endpoint(endpoint).expect("client");
        let credential = Credential::new(secret.to_owned()).expect("credential");
        let sources = [ComposeSource {
            provider: SttProvider::ProviderA,
            available: true,
            text: "hello".to_owned(),
            primary: true,
        }];
        let selection = SourceSelection {
            selected_provider: SttProvider::ProviderA,
            reason: "only_available".to_owned(),
        };
        let protected = ["hello"];
        let attempt = client
            .attempt_groq(
                &credential,
                &request_fixture(&sources, &selection, &protected),
                Duration::from_secs(1),
            )
            .await;

        let candidate = attempt.candidate().expect("structured candidate");
        assert_eq!(attempt.error(), None);
        let baseline = organize_local_baseline("hello", &LocalBaselineOptions::default());
        let composed = compose_structured_candidate(&ComposeInput {
            local_baseline: &baseline,
            base_fingerprint: HELLO_FINGERPRINT,
            sources: &sources,
            source_selection: &selection,
            protected_tokens: &protected,
            policy: RenderingPolicy::Adaptive,
            cloud_outcome: CloudOutcome::Succeeded,
            candidate: Some(candidate),
        });
        assert_eq!(
            composed.decision(),
            CompositionDecision::Accept,
            "compose outcome: {composed:?}"
        );
        assert_eq!(composed.rendered(), "hello");
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let raw_request = request_rx.await.expect("captured request");
        let request_text = String::from_utf8_lossy(&raw_request);
        assert!(
            request_text.lines().any(|line| {
                line.eq_ignore_ascii_case(&format!("authorization: bearer {secret}"))
            })
        );
        let body = body_from_request(&raw_request);
        assert_eq!(body["model"], DPR_GROQ_MODEL);
        assert_eq!(body["reasoning_effort"], DPR_GROQ_REASONING_EFFORT);
        assert!(body.get("reasoning_format").is_none());
        assert_eq!(body["temperature"], 0);
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
        let schema = &body["response_format"]["json_schema"]["schema"];
        assert_eq!(
            schema["properties"]["derivation"]["items"]["properties"]["source_provider"]["anyOf"]
                [0]["enum"],
            json!(["provider_a", "provider_b"])
        );
        let public_error = format!("{:?}", DprCloudErrorClass::Transport);
        assert!(!public_error.contains(secret));
        assert!(!public_error.contains("hello"));
    }

    #[tokio::test]
    async fn four_xx_five_xx_and_rate_limit_are_closed_failures_with_one_call_each() {
        for (status, expected) in [
            (302, DprCloudErrorClass::Transport),
            (400, DprCloudErrorClass::Http4xx),
            (429, DprCloudErrorClass::RateLimited),
            (503, DprCloudErrorClass::Http5xx),
        ] {
            let (endpoint, _request_rx, count) =
                canned_server(status, "sensitive provider body".to_owned(), Duration::ZERO).await;
            let client = DprCloudClient::with_endpoint(endpoint).expect("client");
            let credential = Credential::new("secret".to_owned()).expect("credential");
            let sources = [ComposeSource {
                provider: SttProvider::ProviderA,
                available: true,
                text: "hello".to_owned(),
                primary: true,
            }];
            let selection = SourceSelection {
                selected_provider: SttProvider::ProviderA,
                reason: "only_available".to_owned(),
            };
            let attempt = client
                .attempt_groq(
                    &credential,
                    &request_fixture(&sources, &selection, &[]),
                    Duration::from_secs(1),
                )
                .await;
            assert!(attempt.candidate.is_none());
            assert_eq!(attempt.error, Some(expected));
            assert_eq!(count.load(Ordering::SeqCst), 1);
            assert!(!expected.to_string().contains("sensitive provider body"));
        }
    }

    #[tokio::test]
    async fn slow_mock_is_cancelled_at_supplied_budget_without_second_attempt() {
        let (endpoint, _request_rx, count) = canned_server(
            200,
            provider_response(valid_candidate()),
            Duration::from_secs(2),
        )
        .await;
        let client = DprCloudClient::with_endpoint(endpoint).expect("client");
        let credential = Credential::new("secret".to_owned()).expect("credential");
        let sources = [ComposeSource {
            provider: SttProvider::ProviderA,
            available: true,
            text: "hello".to_owned(),
            primary: true,
        }];
        let selection = SourceSelection {
            selected_provider: SttProvider::ProviderA,
            reason: "only_available".to_owned(),
        };
        let started = Instant::now();
        let attempt = client
            .attempt_groq(
                &credential,
                &request_fixture(&sources, &selection, &[]),
                Duration::from_millis(25),
            )
            .await;
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(attempt.candidate.is_none());
        assert_eq!(attempt.error, Some(DprCloudErrorClass::DeadlineExceeded));
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn malformed_envelope_and_candidate_schema_fail_closed() {
        for (body, expected) in [
            ("not json".to_owned(), DprCloudErrorClass::ProviderEnvelope),
            (
                json!({"choices": [{"message": {"content": "free-form prose"}}]}).to_string(),
                DprCloudErrorClass::CandidateSchema,
            ),
        ] {
            let (endpoint, _request_rx, _) = canned_server(200, body, Duration::ZERO).await;
            let client = DprCloudClient::with_endpoint(endpoint).expect("client");
            let credential = Credential::new("secret".to_owned()).expect("credential");
            let sources = [ComposeSource {
                provider: SttProvider::ProviderA,
                available: true,
                text: "hello".to_owned(),
                primary: true,
            }];
            let selection = SourceSelection {
                selected_provider: SttProvider::ProviderA,
                reason: "only_available".to_owned(),
            };
            let attempt = client
                .attempt_groq(
                    &credential,
                    &request_fixture(&sources, &selection, &[]),
                    Duration::from_secs(1),
                )
                .await;
            assert!(attempt.candidate.is_none());
            assert_eq!(attempt.error, Some(expected));
        }
    }

    #[tokio::test]
    async fn invalid_request_is_rejected_before_any_http_attempt() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let endpoint = format!(
            "http://{}/openai/v1/chat/completions",
            listener.local_addr().expect("address")
        );
        let client = DprCloudClient::with_endpoint(endpoint).expect("client");
        let credential = Credential::new("secret".to_owned()).expect("credential");
        let sources = [ComposeSource {
            provider: SttProvider::ProviderA,
            available: true,
            text: "hello".to_owned(),
            primary: true,
        }];
        let selection = SourceSelection {
            selected_provider: SttProvider::ProviderA,
            reason: "not_a_closed_reason".to_owned(),
        };
        let attempt = client
            .attempt_groq(
                &credential,
                &request_fixture(&sources, &selection, &[]),
                Duration::from_secs(1),
            )
            .await;
        assert_eq!(attempt.error(), Some(DprCloudErrorClass::RequestInvalid));
        assert!(
            tokio::time::timeout(Duration::from_millis(10), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn oversized_envelope_is_rejected_without_exposing_body() {
        let oversized = "x".repeat(1024);
        let (endpoint, _request_rx, _) =
            canned_server(200, oversized.clone(), Duration::ZERO).await;
        let client = DprCloudClient::with_config(endpoint, 128).expect("client");
        let credential = Credential::new("secret".to_owned()).expect("credential");
        let sources = [ComposeSource {
            provider: SttProvider::ProviderA,
            available: true,
            text: "hello".to_owned(),
            primary: true,
        }];
        let selection = SourceSelection {
            selected_provider: SttProvider::ProviderA,
            reason: "only_available".to_owned(),
        };
        let attempt = client
            .attempt_groq(
                &credential,
                &request_fixture(&sources, &selection, &[]),
                Duration::from_secs(1),
            )
            .await;
        assert_eq!(attempt.error, Some(DprCloudErrorClass::ResponseTooLarge));
        assert!(!attempt.error.unwrap().to_string().contains(&oversized));
    }

    struct MissingStore;

    impl SecretStore for MissingStore {
        fn replace(
            &mut self,
            _provider: Provider,
            _credential: Credential,
        ) -> Result<(), BoundaryError> {
            unreachable!()
        }

        fn load(&mut self, provider: Provider) -> Result<Credential, BoundaryError> {
            assert_eq!(provider, Provider::Groq);
            Err(BoundaryError::new(
                BoundaryKind::SecretStorage,
                "sensitive keyring diagnostic",
            ))
        }
    }

    #[test]
    fn credential_resolution_uses_existing_groq_store_and_redacts_failure() {
        let error = match load_groq_credential(&mut MissingStore) {
            Ok(_) => panic!("missing credential unexpectedly resolved"),
            Err(error) => error,
        };
        assert_eq!(error, DprCloudErrorClass::CredentialUnavailable);
        assert!(!error.to_string().contains("sensitive keyring diagnostic"));
    }

    #[test]
    fn model_policy_matches_approved_constants_without_claiming_a_default() {
        assert_eq!(DPR_GROQ_MODEL, "openai/gpt-oss-20b");
        assert_eq!(DPR_GROQ_REASONING_EFFORT, "low");
        assert_eq!(DPR_FORMAT_GROQ_MODEL, "qwen/qwen3.6-27b");
        assert_eq!(DPR_FORMAT_GROQ_REASONING_EFFORT, "none");
        assert_eq!(DPR_FORMAT_GROQ_REASONING_FORMAT, "hidden");
        assert_eq!(DPR_GEMINI_FLASH_LITE_MODEL, "gemini-3.5-flash-lite");
        assert_eq!(DPR_GEMINI_FLASH_MODEL, "gemini-3.6-flash");
        const { assert!(!DPR_HAS_SOLE_PRODUCTION_READY_CLOUD_DEFAULT) };
    }

    #[test]
    fn production_endpoint_policy_allows_https_and_loopback_only() {
        assert!(DprCloudClient::with_endpoint(DPR_GROQ_ENDPOINT).is_ok());
        assert!(DprCloudClient::with_endpoint("http://127.0.0.1:1234/test").is_ok());
        assert!(DprCloudClient::with_endpoint("http://example.com/test").is_err());
        assert!(DprCloudClient::with_endpoint("file:///tmp/socket").is_err());
        // The policy parses the URL: userinfo smuggling and lookalike suffixes
        // must fail even when the raw authority prefix looks trusted.
        assert!(
            DprCloudClient::with_endpoint("http://localhost:8080@attacker.example/test").is_err()
        );
        assert!(
            DprCloudClient::with_endpoint("https://user@api.groq.com@attacker.example/test")
                .is_err()
        );
        assert!(DprCloudClient::with_endpoint("http://localhost.attacker.example/test").is_err());
    }

    fn valid_format_edits() -> Value {
        json!({
            "version": "1",
            "base_fingerprint": HELLO_FINGERPRINT,
            "edits": [{
                "start_utf8": 0,
                "end_utf8": 1,
                "before": "h",
                "after": "H",
                "kind": "casing"
            }]
        })
    }

    fn format_request_fixture<'a>(
        sources: &'a [ComposeSource],
        selection: &'a SourceSelection,
    ) -> DprCloudRequest<'a> {
        DprCloudRequest {
            sources,
            source_selection: selection,
            selected_source: "hello",
            base_fingerprint: HELLO_FINGERPRINT,
            policy: RenderingPolicy::Adaptive,
            protected_tokens: &[],
            small_edit_contract: true,
        }
    }

    #[tokio::test]
    async fn small_edit_contract_requests_qwen_json_object_not_derivation() {
        let (endpoint, request_rx, count) =
            canned_server(200, provider_response(valid_format_edits()), Duration::ZERO).await;
        let client = DprCloudClient::with_endpoint(endpoint).expect("client");
        let credential = Credential::new("secret".to_owned()).expect("credential");
        let sources = [ComposeSource {
            provider: SttProvider::ProviderA,
            available: true,
            text: "hello".to_owned(),
            primary: true,
        }];
        let selection = SourceSelection {
            selected_provider: SttProvider::ProviderA,
            reason: "only_available".to_owned(),
        };
        let attempt = client
            .attempt_groq(
                &credential,
                &format_request_fixture(&sources, &selection),
                Duration::from_secs(1),
            )
            .await;

        let edits = attempt.format_edits_candidate().expect("format edits");
        assert!(attempt.candidate().is_none());
        assert_eq!(attempt.error(), None);
        assert_eq!(edits.version, "1");
        assert_eq!(edits.edits.len(), 1);
        assert_eq!(edits.edits[0].kind.as_str(), "casing");
        assert_eq!(count.load(Ordering::SeqCst), 1);

        let body = body_from_request(&request_rx.await.expect("captured request"));
        assert_eq!(body["model"], DPR_FORMAT_GROQ_MODEL);
        assert_eq!(body["reasoning_effort"], DPR_FORMAT_GROQ_REASONING_EFFORT);
        assert_eq!(body["reasoning_format"], DPR_FORMAT_GROQ_REASONING_FORMAT);
        assert_eq!(body["temperature"], 0);
        assert_eq!(body["response_format"], json!({"type": "json_object"}));
        let user = body["messages"][1]["content"].as_str().expect("user");
        let payload: Value = serde_json::from_str(user).expect("user payload");
        assert_eq!(
            payload["response_schema"]["properties"]["edits"]["items"]["properties"]["kind"]["enum"],
            json!(CLOSED_FORMAT_EDIT_KINDS)
        );
        assert!(payload.get("closed_conversions").is_none());
        assert!(payload.get("host_selection").is_none());
        assert!(
            body["messages"][0]["content"]
                .as_str()
                .expect("system")
                .contains("Reconciliation is a different job")
        );
    }

    #[tokio::test]
    async fn small_edit_contract_rejects_derivation_and_free_form_payloads() {
        for body in [
            provider_response(valid_candidate()),
            json!({"choices": [{"message": {"content": "Goal: ship it"}}]}).to_string(),
        ] {
            let (endpoint, _request_rx, _) = canned_server(200, body, Duration::ZERO).await;
            let client = DprCloudClient::with_endpoint(endpoint).expect("client");
            let credential = Credential::new("secret".to_owned()).expect("credential");
            let sources = [ComposeSource {
                provider: SttProvider::ProviderA,
                available: true,
                text: "hello".to_owned(),
                primary: true,
            }];
            let selection = SourceSelection {
                selected_provider: SttProvider::ProviderA,
                reason: "only_available".to_owned(),
            };
            let attempt = client
                .attempt_groq(
                    &credential,
                    &format_request_fixture(&sources, &selection),
                    Duration::from_secs(1),
                )
                .await;
            assert!(attempt.candidate().is_none());
            assert!(attempt.format_edits_candidate().is_none());
            assert_eq!(attempt.error(), Some(DprCloudErrorClass::CandidateSchema));
        }
    }
}
