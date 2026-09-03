// Reconciliation: Groq merge-result validation and intent reconstruction requests.
//
// Split out of system.rs as a pure move; module-global items come from `super`.

use super::*;

pub struct MergeResultValidator {
    pipeline: TranscriptDecisionPipeline<GroqReconciliationModel>,
}

impl MergeResultValidator {
    pub fn new() -> Self {
        Self {
            pipeline: TranscriptDecisionPipeline::new(
                GroqReconciliationModel::legacy(),
                RECONCILIATION_DEADLINE,
            ),
        }
    }

    pub fn intent_reconstruction(reaper: ProviderReaper) -> Self {
        Self {
            pipeline: TranscriptDecisionPipeline::with_intent_reconstruction(
                GroqReconciliationModel {
                    reaper: Some(reaper),
                },
                INTENT_RECONSTRUCTION_DEADLINE,
                Vec::new(),
            ),
        }
    }
}

impl Default for MergeResultValidator {
    fn default() -> Self {
        Self::new()
    }
}

impl TranscriptValidator for MergeResultValidator {
    fn set_dictionary_terms(&mut self, dictionary_terms: Vec<String>) {
        self.pipeline.set_dictionary_terms(dictionary_terms);
    }

    fn validate(
        &mut self,
        sources: Vec<SourceTranscript>,
    ) -> BoundaryFuture<'_, TranscriptDecision> {
        self.pipeline.validate(sources)
    }

    fn prepare(
        &mut self,
        sources: Vec<SourceTranscript>,
    ) -> BoundaryFuture<'_, PreparedTranscriptDecision> {
        Box::pin(self.pipeline.prepare(sources))
    }

    fn reconstruct(
        &mut self,
        attempt: IntentReconstructionAttempt,
    ) -> BoundaryFuture<'_, TranscriptDecision> {
        Box::pin(self.pipeline.reconstruct(attempt))
    }
}

pub(super) struct GroqReconciliationModel {
    pub(super) reaper: Option<ProviderReaper>,
}

impl GroqReconciliationModel {
    fn legacy() -> Self {
        Self { reaper: None }
    }
}

impl ReconciliationModel for GroqReconciliationModel {
    fn request(
        &mut self,
        kind: ReconciliationKind,
        sources: Vec<SourceTranscript>,
        candidate: Option<MergeResult>,
        cancel: Arc<CancelRegistry>,
    ) -> BoundaryFuture<'_, MergeResult> {
        Box::pin(async move {
            // The whole operation — including the potentially slow synchronous
            // Secret Service lookup — runs inside ONE owned blocking task, so
            // it never blocks the async thread and the pipeline can cancel it
            // as a unit. curl observes the cancel flag through its bounded
            // wait: on cancellation the child is killed and reaped by the same
            // loop that owns its handle, and this future completes only after
            // that cleanup, keeping the reap ordered before any fallback
            // becomes observable. The post-lookup check guarantees no curl is
            // spawned once the deadline has already cancelled the request.
            tokio::task::spawn_blocking(move || {
                let credential = SecretStore::load(&mut SecretToolStore, Provider::Groq)?;
                if cancel.is_cancelled() {
                    return Err(BoundaryError::new(
                        BoundaryKind::Validation,
                        "reconciliation request cancelled",
                    ));
                }
                request_groq_reconciliation(credential, kind, sources, candidate, &cancel)
            })
            .await
            .map_err(|_| {
                BoundaryError::new(
                    BoundaryKind::Validation,
                    "reconciliation request task failed",
                )
            })?
        })
    }

    fn reconstruct_intent(
        &mut self,
        request: IntentReconstructionRequest,
        cancel: Arc<CancelRegistry>,
    ) -> BoundaryFuture<'_, MergeResult> {
        let reaper = self.reaper.clone().expect("intent validator has a reaper");
        Box::pin(async move {
            let lane = reaper.credential_lane().clone();
            let entry = lane.register();
            let mut owner = CredentialPreparationOwner::new(entry, lane, Provider::Groq);
            let capability = tokio::select! {
                capability = owner.poll_outcome() => capability,
                _ = wait_for_reconstruction_cancel(Arc::clone(&cancel)) => {
                    owner.cancel_and_drive_terminal().await;
                    return Err(BoundaryError::new(
                        BoundaryKind::Validation,
                        "Intent Reconstruction request cancelled",
                    ));
                }
            };
            let GrammarCapability::Ready(ready) = capability else {
                return Err(BoundaryError::new(
                    BoundaryKind::Validation,
                    "Groq credential unavailable for Intent Reconstruction",
                ));
            };
            let credential = ready.credential().clone();
            tokio::task::spawn_blocking(move || {
                request_groq_intent_reconstruction(credential, request, &cancel)
            })
            .await
            .map_err(|_| {
                BoundaryError::new(
                    BoundaryKind::Validation,
                    "Intent Reconstruction request task failed",
                )
            })?
        })
    }
}

async fn wait_for_reconstruction_cancel(cancel: Arc<CancelRegistry>) {
    while !cancel.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn request_groq_intent_reconstruction(
    credential: Credential,
    request: IntentReconstructionRequest,
    cancel: &CancelRegistry,
) -> Result<MergeResult, BoundaryError> {
    let endpoint = std::env::var("VOISU_GROQ_RECONCILIATION_URL")
        .unwrap_or_else(|_| "https://api.groq.com/openai/v1/chat/completions".to_owned());
    if !provider_endpoint_is_secure(&endpoint) {
        return Err(BoundaryError::new(
            BoundaryKind::Validation,
            "Groq Intent Reconstruction endpoint must use HTTPS except on loopback",
        ));
    }
    let body = groq_intent_reconstruction_request_body(&request).to_string();
    let config = format!(
        "url = \"{}\"\nheader = \"Authorization: Bearer {}\"\nheader = \"Content-Type: application/json\"\ndata = \"{}\"\n",
        curl_config_escape(&endpoint),
        curl_config_escape(credential.expose_to_boundary()),
        curl_config_escape(&body),
    );
    let outcome = run_restricted_with_deadline(
        "curl",
        &[
            "-q",
            "--config",
            "-",
            "--fail",
            "--silent",
            "--show-error",
            "--max-time",
            "5",
        ],
        Some(config.as_bytes()),
        true,
        INTENT_RECONSTRUCTION_DEADLINE,
        Some(cancel),
    )
    .map_err(|_| {
        BoundaryError::new(
            BoundaryKind::Validation,
            "Groq Intent Reconstruction request unavailable or failed",
        )
    })?;
    if !outcome.success {
        return Err(BoundaryError::new(
            BoundaryKind::Validation,
            "Groq rejected the Intent Reconstruction request",
        ));
    }
    let response: serde_json::Value = serde_json::from_slice(&outcome.stdout).map_err(|_| {
        BoundaryError::new(
            BoundaryKind::Validation,
            "Groq Intent Reconstruction returned malformed JSON",
        )
    })?;
    let content = response
        .pointer("/choices/0/message/content")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            BoundaryError::new(
                BoundaryKind::Validation,
                "Groq Intent Reconstruction omitted content",
            )
        })?;
    voisu_core::parse_intent_reconstruction_response(content)
}

pub(super) fn groq_intent_reconstruction_request_body(
    request: &IntentReconstructionRequest,
) -> serde_json::Value {
    let sources: Vec<_> = request
        .sources
        .iter()
        .map(|source| {
            serde_json::json!({
                "provider": source.provider.cli_label(),
                "text": source.text,
            })
        })
        .collect();
    serde_json::json!({
        "model": DEFAULT_GROQ_RECONCILIATION_MODEL,
        "reasoning_effort": "none",
        "temperature": 0,
        "messages": [
            {"role": "system", "content": "Infer the user's most likely intended wording from both Source Transcripts. Neither source is truth. Novel wording is allowed. Return exactly one JSON object with exactly this shape: {\"wording\":\"...\"}. The wording value must contain the final transcript. Do not add any other keys, markdown fences, or explanation. Deterministic host code owns structural layout."},
            {"role": "user", "content": serde_json::json!({
                "sources": sources,
                "dictionary": request.dictionary_terms,
            }).to_string()}
        ],
        "response_format": {"type": "json_object"}
    })
}

fn request_groq_reconciliation(
    credential: Credential,
    kind: ReconciliationKind,
    sources: Vec<SourceTranscript>,
    candidate: Option<MergeResult>,
    cancel: &CancelRegistry,
) -> Result<MergeResult, BoundaryError> {
    let endpoint = std::env::var("VOISU_GROQ_RECONCILIATION_URL")
        .unwrap_or_else(|_| "https://api.groq.com/openai/v1/chat/completions".to_owned());
    if !provider_endpoint_is_secure(&endpoint) {
        return Err(BoundaryError::new(
            BoundaryKind::Validation,
            "Groq reconciliation endpoint must use HTTPS except on loopback",
        ));
    }
    let model = std::env::var("VOISU_GROQ_RECONCILIATION_MODEL")
        .unwrap_or_else(|_| DEFAULT_GROQ_RECONCILIATION_MODEL.to_owned());
    if model.trim().is_empty() || model.contains(['\n', '\r']) {
        return Err(BoundaryError::new(
            BoundaryKind::Validation,
            "invalid Groq reconciliation model",
        ));
    }
    let source_text = sources
        .iter()
        .map(|source| format!("{}: {}", source.provider.cli_label(), source.text))
        .collect::<Vec<_>>()
        .join("\n");
    let task = match (kind, candidate) {
        (ReconciliationKind::Reconcile, _) => format!(
            "Reconcile these Source Transcripts. Return only the faithful final Transcript, with no labels, explanation, or added content.\n{source_text}"
        ),
        (ReconciliationKind::Repair, Some(candidate)) => format!(
            "Repair this unsafe candidate using only the Source Transcripts. Return only the faithful final Transcript, with no labels, explanation, or added content.\nCandidate: {}\n{source_text}",
            candidate.0
        ),
        (ReconciliationKind::Repair, None) => {
            return Err(BoundaryError::new(
                BoundaryKind::Validation,
                "reconciliation recovery omitted its candidate",
            ));
        }
    };
    let body = groq_reconciliation_request_body(&model, &task).to_string();
    let config = format!(
        "url = \"{}\"\nheader = \"Authorization: Bearer {}\"\nheader = \"Content-Type: application/json\"\ndata = \"{}\"\n",
        curl_config_escape(&endpoint),
        curl_config_escape(credential.expose_to_boundary()),
        curl_config_escape(&body),
    );
    let outcome = run_restricted_with_deadline(
        "curl",
        &[
            "-q",
            "--config",
            "-",
            "--fail",
            "--silent",
            "--show-error",
            "--max-time",
            "2",
        ],
        Some(config.as_bytes()),
        true,
        RECONCILIATION_PROCESS_DEADLINE,
        Some(cancel),
    )
    .map_err(|error| match error {
        ProcessError::TimedOut => BoundaryError::new(
            BoundaryKind::Validation,
            "reconciliation request deadline elapsed",
        ),
        _ => BoundaryError::new(
            BoundaryKind::Validation,
            "Groq reconciliation request unavailable or failed",
        ),
    })?;
    if !outcome.success {
        return Err(BoundaryError::new(
            BoundaryKind::Validation,
            "Groq rejected the reconciliation request",
        ));
    }
    let response: serde_json::Value = serde_json::from_slice(&outcome.stdout).map_err(|_| {
        BoundaryError::new(
            BoundaryKind::Validation,
            "Groq reconciliation returned malformed JSON",
        )
    })?;
    response
        .pointer("/choices/0/message/content")
        .and_then(|text| text.as_str())
        .map(|text| MergeResult(text.to_owned()))
        .ok_or_else(|| {
            BoundaryError::new(BoundaryKind::Validation, "Groq reconciliation omitted text")
        })
}

/// Build the Groq chat-completions JSON body for reconciliation.
///
/// `reasoning_effort: "none"` is attached only when `model` is exactly the
/// selected default id (`qwen/qwen3.6-27b`). Every other override — including
/// other `qwen/…` ids and GPT-OSS — omits the field unless separately supported
/// later. Attaching `none` to GPT-OSS or Llama returns HTTP 400.
pub(super) fn groq_reconciliation_request_body(model: &str, task: &str) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": model,
        "temperature": 0,
        "messages": [
            {
                "role": "system",
                "content": "You are Voisu's Transcript reconciliation model. Preserve spoken meaning and never add commentary, prompt text, or facts."
            },
            { "role": "user", "content": task }
        ]
    });
    if model == DEFAULT_GROQ_RECONCILIATION_MODEL {
        body.as_object_mut()
            .expect("reconciliation request body is a JSON object")
            .insert(
                "reasoning_effort".to_owned(),
                serde_json::Value::String("none".to_owned()),
            );
    }
    body
}
