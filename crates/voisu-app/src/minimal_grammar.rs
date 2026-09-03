//! Groq Minimal Grammar request/response adapter (Smart Writing SW6 / spec §7).
//!
//! This layer owns only the fixed GPT-OSS request contract and the untrusted
//! chat-completions envelope. [`crate::grammar_http`] owns transport, while
//! `voisu_core::apply_grammar_candidate_json` remains the sole safety/composer
//! authority. Any adapter failure leaves the local Formatting baseline intact.

use std::fmt;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use voisu_core::{
    FormattingBaseline, MAX_GRAMMAR_EDITS, MAX_VALIDATED_TRANSCRIPT_UTF8_BYTES,
    text_sha256_fingerprint,
};

use crate::grammar_http::{GrammarHttpClient, GrammarHttpError, MAX_GRAMMAR_RESPONSE_BYTES};

pub const MINIMAL_GRAMMAR_MODEL: &str = "openai/gpt-oss-20b";
pub const MINIMAL_GRAMMAR_REASONING_EFFORT: &str = "low";
pub const MINIMAL_GRAMMAR_STREAM: bool = false;
pub const MAX_MINIMAL_GRAMMAR_COMPLETION_TOKENS: u32 = 2_048;
pub const MINIMAL_GRAMMAR_RESPONSE_FORMAT: &str = "json_schema";
pub const MINIMAL_GRAMMAR_SCHEMA_STRICT: bool = true;
pub const GRAMMAR_RESULT_CUTOFF_FROM_GATE_ENTRY: Duration = Duration::from_millis(800);

pub const MINIMAL_GRAMMAR_SYSTEM_INSTRUCTION: &str = "Return only localized edits under these rules: G_THERE_IS_PLURAL_QUANTITY replaces the whole token 'is' with 'are' only in 'there is <two through twelve or 2 through 12> issues' when tokens are separated only by ASCII spaces; G_LETS_MEET_CONTRACTION replaces sentence-initial whole-token 'lets' with \"let's\" only immediately before 'meet' with ASCII spaces; G_DIDNT_APOSTROPHE replaces the whole token 'didnt' with \"didn't\". Preserve meaning, vocabulary, tone, and negation. Use half-open UTF-8 byte offsets into the supplied Validated Transcript. Return an empty edits list when uncertain.";

const RULE_IDS: [&str; 3] = [
    "G_THERE_IS_PLURAL_QUANTITY",
    "G_LETS_MEET_CONTRACTION",
    "G_DIDNT_APOSTROPHE",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MinimalGrammarError {
    Transport(GrammarHttpError),
    ResultCutoff,
    InvalidBaselineIdentity,
    InvalidProviderEnvelope,
}

impl fmt::Display for MinimalGrammarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(f, "{error}"),
            Self::ResultCutoff => write!(f, "grammar result missed absolute cutoff"),
            Self::InvalidBaselineIdentity => {
                write!(f, "grammar baseline identity was invalid")
            }
            Self::InvalidProviderEnvelope => write!(f, "grammar provider envelope was invalid"),
        }
    }
}

impl std::error::Error for MinimalGrammarError {}

impl From<GrammarHttpError> for MinimalGrammarError {
    fn from(value: GrammarHttpError) -> Self {
        Self::Transport(value)
    }
}

/// Fixed-model adapter over one process-owned async HTTP client.
#[derive(Clone, Debug)]
pub struct MinimalGrammarAdapter {
    client: GrammarHttpClient,
}

impl MinimalGrammarAdapter {
    pub fn production() -> Result<Self, MinimalGrammarError> {
        Ok(Self {
            client: GrammarHttpClient::production()?,
        })
    }

    /// Constructor injection is deliberately limited to an already-built
    /// transport. Production endpoint policy remains in `GrammarHttpClient`.
    #[must_use]
    pub fn new(client: GrammarHttpClient) -> Self {
        Self { client }
    }

    /// Send exactly one request using an already-ready bearer credential.
    ///
    /// The caller supplies the Final Transform Gate entry instant. The HTTP
    /// client's 700 ms limit and this absolute 800 ms cutoff both apply; no
    /// result is accepted at or after the latter. Errors are closed reasons for
    /// Formatting-only fallback and never contain provider content or secrets.
    pub async fn request_candidate(
        &self,
        bearer_token: &str,
        validated_transcript: &str,
        baseline: &FormattingBaseline,
        gate_entry: Instant,
    ) -> Result<Vec<u8>, MinimalGrammarError> {
        if validated_transcript.len() > MAX_VALIDATED_TRANSCRIPT_UTF8_BYTES
            || baseline.base_fingerprint() != text_sha256_fingerprint(validated_transcript)
            || !baseline.verify_derivation_digest()
        {
            return Err(MinimalGrammarError::InvalidBaselineIdentity);
        }
        let cutoff = gate_entry
            .checked_add(GRAMMAR_RESULT_CUTOFF_FROM_GATE_ENTRY)
            .ok_or(MinimalGrammarError::ResultCutoff)?;
        let remaining = cutoff.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(MinimalGrammarError::ResultCutoff);
        }

        let body = build_request(validated_transcript, baseline);
        let response = tokio::time::timeout(remaining, self.client.post_json(bearer_token, &body))
            .await
            .map_err(|_| MinimalGrammarError::ResultCutoff)??;

        if Instant::now() >= cutoff {
            return Err(MinimalGrammarError::ResultCutoff);
        }
        extract_candidate(&response.body)
    }
}

fn build_request(validated_transcript: &str, baseline: &FormattingBaseline) -> Value {
    let user_content = json!({
        "validated_transcript": validated_transcript,
        "base_version": baseline.base_version(),
        "base_fingerprint": baseline.base_fingerprint(),
    })
    .to_string();

    json!({
        "model": MINIMAL_GRAMMAR_MODEL,
        "reasoning_effort": MINIMAL_GRAMMAR_REASONING_EFFORT,
        "stream": MINIMAL_GRAMMAR_STREAM,
        "max_completion_tokens": MAX_MINIMAL_GRAMMAR_COMPLETION_TOKENS,
        "response_format": {
            "type": MINIMAL_GRAMMAR_RESPONSE_FORMAT,
            "json_schema": {
                "name": "minimal_grammar_edits",
                "strict": MINIMAL_GRAMMAR_SCHEMA_STRICT,
                "schema": candidate_schema(),
            }
        },
        "messages": [
            {"role": "system", "content": MINIMAL_GRAMMAR_SYSTEM_INSTRUCTION},
            {"role": "user", "content": user_content},
        ],
    })
}

fn candidate_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["base_version", "base_fingerprint", "edits"],
        "properties": {
            "base_version": {"type": "string", "maxLength": 256},
            "base_fingerprint": {
                "type": "string",
                "pattern": "^sha256:[0-9a-f]{64}$"
            },
            "edits": {
                "type": "array",
                "maxItems": MAX_GRAMMAR_EDITS,
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["id", "rule_id", "start_utf8", "end_utf8", "before", "after"],
                    "properties": {
                        "id": {"type": "string", "maxLength": 256},
                        "rule_id": {"type": "string", "enum": RULE_IDS},
                        "start_utf8": {"type": "integer", "minimum": 0},
                        "end_utf8": {"type": "integer", "minimum": 0},
                        "before": {"type": "string", "maxLength": 256},
                        "after": {"type": "string", "maxLength": 256}
                    }
                }
            }
        }
    })
}

fn extract_candidate(body: &[u8]) -> Result<Vec<u8>, MinimalGrammarError> {
    if body.len() > MAX_GRAMMAR_RESPONSE_BYTES {
        return Err(MinimalGrammarError::InvalidProviderEnvelope);
    }
    let root: Value =
        serde_json::from_slice(body).map_err(|_| MinimalGrammarError::InvalidProviderEnvelope)?;
    let choices = root
        .get("choices")
        .and_then(Value::as_array)
        .filter(|choices| choices.len() == 1)
        .ok_or(MinimalGrammarError::InvalidProviderEnvelope)?;
    let message = choices[0]
        .get("message")
        .and_then(Value::as_object)
        .ok_or(MinimalGrammarError::InvalidProviderEnvelope)?;
    if message.get("refusal").is_some_and(|value| !value.is_null()) {
        return Err(MinimalGrammarError::InvalidProviderEnvelope);
    }
    let content = message
        .get("content")
        .and_then(Value::as_str)
        .filter(|content| !content.trim().is_empty())
        .ok_or(MinimalGrammarError::InvalidProviderEnvelope)?;
    if content.len() > MAX_GRAMMAR_RESPONSE_BYTES {
        return Err(MinimalGrammarError::InvalidProviderEnvelope);
    }
    // Reject non-JSON and trailing data here. Candidate shape and every safety
    // predicate remain the core gate's responsibility.
    serde_json::from_str::<Value>(content)
        .map_err(|_| MinimalGrammarError::InvalidProviderEnvelope)?;
    Ok(content.as_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use voisu_core::{
        FormatOptions, GrammarSafetyOptions, apply_grammar_candidate_json,
        format_validated_for_grammar,
    };

    fn baseline(text: &str) -> FormattingBaseline {
        format_validated_for_grammar(text, FormatOptions::default())
    }

    async fn canned_server(
        status: u16,
        response_body: String,
        delay: Duration,
    ) -> (String, oneshot::Receiver<Value>, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let (request_tx, request_rx) = oneshot::channel();
        let count = Arc::new(AtomicUsize::new(0));
        let count_for_task = Arc::clone(&count);
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            count_for_task.fetch_add(1, Ordering::SeqCst);
            let request = read_request(&mut socket).await;
            let body_start = request
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|index| index + 4)
                .expect("headers");
            let parsed = serde_json::from_slice(&request[body_start..]).expect("request JSON");
            let _ = request_tx.send(parsed);
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

    fn provider_response(content: Value) -> String {
        json!({
            "choices": [{
                "message": {"role": "assistant", "content": content.to_string()}
            }]
        })
        .to_string()
    }

    #[test]
    fn constants_and_strict_schema_match_companion() {
        assert_eq!(MINIMAL_GRAMMAR_MODEL, "openai/gpt-oss-20b");
        assert_eq!(MINIMAL_GRAMMAR_REASONING_EFFORT, "low");
        assert_eq!(MAX_MINIMAL_GRAMMAR_COMPLETION_TOKENS, 2_048);
        assert_eq!(
            GRAMMAR_RESULT_CUTOFF_FROM_GATE_ENTRY,
            Duration::from_millis(800)
        );
        let schema = candidate_schema();
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(
            schema["properties"]["edits"]["items"]["additionalProperties"],
            false
        );
        assert_eq!(
            schema["properties"]["edits"]["items"]["properties"]["rule_id"]["enum"],
            json!(RULE_IDS)
        );

        let companion: Value = serde_json::from_str(include_str!(
            "../../../docs/research/smart-writing-spec-constants-2026-08-09.json"
        ))
        .expect("constants companion");
        assert_eq!(
            companion["model"]["MINIMAL_GRAMMAR_MODEL"],
            MINIMAL_GRAMMAR_MODEL
        );
        assert_eq!(
            companion["model"]["MINIMAL_GRAMMAR_REASONING_EFFORT"],
            MINIMAL_GRAMMAR_REASONING_EFFORT
        );
        assert_eq!(
            companion["model"]["MINIMAL_GRAMMAR_STREAM"],
            MINIMAL_GRAMMAR_STREAM
        );
        assert_eq!(
            companion["limits"]["MAX_MINIMAL_GRAMMAR_COMPLETION_TOKENS"],
            MAX_MINIMAL_GRAMMAR_COMPLETION_TOKENS
        );
        assert_eq!(
            companion["timing"]["GRAMMAR_RESULT_CUTOFF_FROM_GATE_ENTRY"],
            800
        );
    }

    #[test]
    fn request_omits_forbidden_api_fields_and_has_transcript_only_user_body() {
        let text = "there is two issues";
        let baseline = baseline(text);
        let request = build_request(text, &baseline);
        let object = request.as_object().expect("request object");
        assert_eq!(object.len(), 6);
        for omitted in ["n", "temperature", "tools", "store"] {
            assert!(!object.contains_key(omitted));
        }
        assert_eq!(request["model"], MINIMAL_GRAMMAR_MODEL);
        assert_eq!(request["stream"], false);
        let messages = request["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["content"], MINIMAL_GRAMMAR_SYSTEM_INSTRUCTION);
        let user: Value =
            serde_json::from_str(messages[1]["content"].as_str().expect("user content"))
                .expect("user JSON");
        assert_eq!(user.as_object().expect("user object").len(), 3);
        assert_eq!(user["validated_transcript"], text);
        assert_eq!(user["base_version"], baseline.base_version());
        assert_eq!(user["base_fingerprint"], baseline.base_fingerprint());
        for forbidden in [
            "source_transcript",
            "app",
            "window",
            "field",
            "screen",
            "clipboard",
            "surrounding_text",
            "dictionary",
            "names",
            "audio",
        ] {
            assert!(user.get(forbidden).is_none());
        }
    }

    #[tokio::test]
    async fn canned_harness_sends_exact_contract_and_extracts_one_candidate() {
        let text = "there is two issues";
        let baseline = baseline(text);
        let candidate = json!({
            "base_version": baseline.base_version(),
            "base_fingerprint": baseline.base_fingerprint(),
            "edits": [{
                "id": "there-plural-1",
                "rule_id": "G_THERE_IS_PLURAL_QUANTITY",
                "start_utf8": 6,
                "end_utf8": 8,
                "before": "is",
                "after": "are"
            }]
        });
        let (endpoint, request_rx, count) =
            canned_server(200, provider_response(candidate.clone()), Duration::ZERO).await;
        let adapter =
            MinimalGrammarAdapter::new(GrammarHttpClient::with_endpoint(endpoint).expect("client"));
        let received = adapter
            .request_candidate("ready-token", text, &baseline, Instant::now())
            .await
            .expect("candidate");
        assert_eq!(
            serde_json::from_slice::<Value>(&received).expect("candidate JSON"),
            candidate
        );
        let composed = apply_grammar_candidate_json(
            text,
            baseline.base_version(),
            &baseline,
            &received,
            GrammarSafetyOptions::default(),
        );
        assert_eq!(composed.rendered, "There are two issues.");
        assert!(composed.diagnostics.is_empty());
        let request = request_rx.await.expect("request captured");
        assert_eq!(request, build_request(text, &baseline));
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn empty_multiple_non_content_refusal_and_trailing_envelopes_fallback() {
        let cases = [
            json!({"choices": []}).to_string(),
            json!({"choices": [{"message":{"content":"{}"}}, {"message":{"content":"{}"}}]})
                .to_string(),
            json!({"choices": [{"message":{"content":null}}]}).to_string(),
            json!({"choices": [{"message":{"content":"   "}}]}).to_string(),
            json!({"choices": [{"message":{"content":"{}", "refusal":"no"}}]}).to_string(),
            json!({"choices": [{"message":{"content":"{} trailing"}}]}).to_string(),
        ];
        for body in cases {
            assert_eq!(
                extract_candidate(body.as_bytes()),
                Err(MinimalGrammarError::InvalidProviderEnvelope)
            );
        }
    }

    #[tokio::test]
    async fn non_success_and_absolute_cutoff_are_closed_fallbacks() {
        let text = "didnt go";
        let baseline = baseline(text);
        for status in [429, 500] {
            let (endpoint, _request, count) =
                canned_server(status, "{}".to_owned(), Duration::ZERO).await;
            let adapter = MinimalGrammarAdapter::new(
                GrammarHttpClient::with_endpoint(endpoint).expect("client"),
            );
            assert!(
                matches!(adapter.request_candidate("token", text, &baseline, Instant::now()).await, Err(MinimalGrammarError::Transport(GrammarHttpError::NonSuccessStatus { status: actual })) if actual == status)
            );
            assert_eq!(count.load(Ordering::SeqCst), 1);
        }

        let (endpoint, _request, count) =
            canned_server(200, provider_response(json!({"edits": []})), Duration::ZERO).await;
        let adapter =
            MinimalGrammarAdapter::new(GrammarHttpClient::with_endpoint(endpoint).expect("client"));
        assert_eq!(
            adapter
                .request_candidate(
                    "token",
                    text,
                    &baseline,
                    Instant::now() - Duration::from_millis(800),
                )
                .await,
            Err(MinimalGrammarError::ResultCutoff)
        );
        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "expired gate must not start HTTP"
        );
    }

    #[tokio::test]
    async fn delayed_result_cannot_cross_800_ms_gate_cutoff() {
        let text = "didnt go";
        let baseline = baseline(text);
        let (endpoint, _request, count) = canned_server(
            200,
            provider_response(json!({
                "base_version": baseline.base_version(),
                "base_fingerprint": baseline.base_fingerprint(),
                "edits": []
            })),
            Duration::from_millis(250),
        )
        .await;
        let adapter = MinimalGrammarAdapter::new(
            GrammarHttpClient::with_config(
                endpoint,
                Duration::from_millis(700),
                MAX_GRAMMAR_RESPONSE_BYTES,
            )
            .expect("client"),
        );
        // Leave 200 ms for ordinary CI scheduling, then prove a response that
        // would finish later is dropped at the absolute gate cutoff.
        let gate_entry = Instant::now() - Duration::from_millis(600);
        assert_eq!(
            adapter
                .request_candidate("token", text, &baseline, gate_entry)
                .await,
            Err(MinimalGrammarError::ResultCutoff)
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
