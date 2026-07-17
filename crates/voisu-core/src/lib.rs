//! Shared domain, provider coordination, and IPC types for Voisu.

use std::collections::{HashMap, HashSet};
use std::env;
use std::future::Future;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

mod diagnostics;
pub use diagnostics::{
    correlation_id, export_record, is_secret_env_key, redacted_environment, replay_capture,
    sanitize_url, scrub_embedded_urls, scrub_secret_values, unix_millis_now, DebugAudioRecord,
    DiagnosticExport,
    DiagnosticRecord, DiagnosticStore, PruneOutcome, ReplayOutcome, RetentionPolicy,
    SourceTranscriptRecord, DEFAULT_DEBUG_AUDIO_TTL, DEFAULT_MAX_AGE, DEFAULT_MAX_RECORDS,
    EXPORT_ENV_ALLOWLIST, MAX_STORED_TEXT, REDACTED,
};

pub const PROTOCOL_VERSION: u32 = 1;

pub fn runtime_dir() -> Result<PathBuf, String> {
    let path = PathBuf::from(
        env::var_os("XDG_RUNTIME_DIR")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "XDG_RUNTIME_DIR is not set".to_owned())?,
    );
    if !path.is_absolute() {
        return Err("XDG_RUNTIME_DIR must be absolute".to_owned());
    }
    let metadata = path
        .symlink_metadata()
        .map_err(|error| format!("cannot inspect XDG_RUNTIME_DIR: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("XDG_RUNTIME_DIR must be a real directory".to_owned());
    }
    // SAFETY: geteuid has no preconditions and does not mutate memory.
    let effective_uid = unsafe { libc::geteuid() };
    if metadata.uid() != effective_uid {
        return Err("XDG_RUNTIME_DIR must be owned by the current user".to_owned());
    }
    if metadata.mode() & 0o777 != 0o700 {
        return Err("XDG_RUNTIME_DIR must have mode 0700".to_owned());
    }
    Ok(path)
}

pub fn socket_path() -> Result<PathBuf, String> {
    Ok(runtime_dir()?
        .join("voisu")
        .join(format!("v{PROTOCOL_VERSION}"))
        .join("daemon.sock"))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Command {
    Start,
    Stop,
    Toggle,
    Status,
    /// Observer-only status with the most recent terminal event retained.
    /// This is not a lifecycle command and cannot mutate daemon state.
    OverlayStatus,
    /// Returns the desktop-approved Trigger Key binding for display, or a
    /// notice that no Trigger Key is bound. Never blocks CLI start/stop/toggle.
    Shortcut,
    /// Returns the retained local diagnostic history (newest first).
    History,
    /// Returns a redacted, self-contained diagnostic export for one correlation ID.
    Export(String),
    /// Replays a fixed captured fixture at the given path through the provider
    /// and validation boundaries without capturing audio again.
    Replay(String),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DaemonState {
    Idle,
    Recording,
    Processing,
}

impl DaemonState {
    pub fn cli_label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Recording => "Recording",
            Self::Processing => "processing",
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct VersionEnvelope {
    pub version: u32,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Request {
    pub version: u32,
    pub command: Command,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleStage {
    CaptureStarted,
    ProvidersStarted,
    CaptureFinalized,
    ProvidersCompleted,
    ValidationCompleted,
    DeliveryCompleted,
    CaptureAborted,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LifecycleEvidence {
    pub recording_id: u64,
    /// The correlation ID that joins every event of this Recording across
    /// capture, chunk, provider, reconciliation, validation, Delivery, and error.
    #[serde(default)]
    pub correlation_id: String,
    pub stages: Vec<LifecycleStage>,
    pub delivery_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_method: Option<DeliveryMethod>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_fallback_reason: Option<String>,
    #[serde(default)]
    pub streamed_chunk_count: u32,
    #[serde(default)]
    pub source_transcript_providers: Vec<Provider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_chunk_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_finalized_ms: Option<u64>,
    #[serde(default)]
    pub provider_timings_ms: Vec<ProviderTiming>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_to_text_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_selection: Option<TranscriptSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
    #[serde(default)]
    pub reconciliation_requested: bool,
    #[serde(default)]
    pub recovery_attempted: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProviderTiming {
    pub provider: Provider,
    pub completed_ms: u64,
}

/// How far a provider progressed before it failed or was found absent. A history
/// record keeps this so a reader can tell a provider that never began (absent or
/// disabled) from one that broke mid-stream or missed the Provider Deadline.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderFailureStage {
    /// The provider never began for this Recording — absent, disabled, or not
    /// configured. No Source Transcript was ever attempted.
    NotStarted,
    /// The provider began but failed while streaming audio, before finalize.
    Streaming,
    /// The provider failed while producing its Source Transcript at finalize.
    Completion,
    /// The Provider Deadline elapsed before the provider produced a Source
    /// Transcript, so its result was abandoned.
    ProviderDeadline,
    /// The provider began but the Recording was torn down before it could
    /// produce a Source Transcript — a startup failure of the OTHER provider, a
    /// capture failure, or a shutdown mid-start. It never failed on its own, but
    /// it produced no Source Transcript, so its absence is recorded, not silent.
    Aborted,
}

/// A recorded provider failure or absence for one Recording: which provider, how
/// far it reached, and the boundary diagnostic. This is what makes a missing
/// Source Transcript visible instead of silent — every configured provider that
/// does not contribute a Source Transcript leaves one of these in the history
/// record. The diagnostic is a local boundary detail; export scrubs it of secret
/// values like every other free-form string.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProviderFailure {
    pub provider: Provider,
    pub stage: ProviderFailureStage,
    pub diagnostic: String,
}

impl ProviderFailure {
    pub fn new(
        provider: Provider,
        stage: ProviderFailureStage,
        diagnostic: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            stage,
            diagnostic: diagnostic.into(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Response {
    pub version: u32,
    pub ok: bool,
    pub state: Option<DaemonState>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<LifecycleEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<Vec<DiagnosticRecord>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub export: Option<DiagnosticExport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay_event: Option<OverlayEvent>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayOutcome {
    Delivered,
    QualityFailure,
    CaptureFailure,
    EmptyRecording,
    TooShortRecording,
    SilentRecording,
    RecordingDeadline,
    ProviderFailure,
    DeliveryFailure,
    OtherFailure,
    /// A newer daemon may report an outcome this client does not know. It must
    /// deserialize into a safe, generic failure rather than break the whole
    /// observer response.
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OverlayEvent {
    pub id: u64,
    /// Identifies the daemon process instance that emitted this event. The id
    /// counter resets to 1 on every daemon restart, so an observer must scope
    /// event identity by `(instance, id)`; otherwise a restarted daemon's first
    /// terminal event (id 1) collides with the last one shown and is suppressed.
    /// Defaults to 0 for responses from a daemon that predates this field.
    #[serde(default)]
    pub instance: u64,
    pub outcome: OverlayOutcome,
    pub message: String,
}

impl Response {
    pub fn success(state: DaemonState, message: impl Into<String>) -> Self {
        Self::with_evidence(true, Some(state), message, None)
    }

    pub fn rejected(state: Option<DaemonState>, message: impl Into<String>) -> Self {
        Self::with_evidence(false, state, message, None)
    }

    pub fn with_evidence(
        ok: bool,
        state: Option<DaemonState>,
        message: impl Into<String>,
        evidence: Option<LifecycleEvidence>,
    ) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            ok,
            state,
            message: message.into(),
            evidence,
            history: None,
            export: None,
            overlay_event: None,
        }
    }

    pub fn with_history(records: Vec<DiagnosticRecord>) -> Self {
        let mut response = Self::success(DaemonState::Idle, "diagnostic history");
        response.history = Some(records);
        response
    }

    pub fn with_export(export: DiagnosticExport) -> Self {
        let mut response = Self::success(DaemonState::Idle, "diagnostic export");
        response.export = Some(export);
        response
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoundaryKind {
    Capture,
    EmptyRecording,
    TooShortRecording,
    SilentRecording,
    RecordingDeadline,
    Provider,
    Validation,
    Delivery,
    SecretStorage,
    ProviderAuthentication,
    Shortcut,
}

#[derive(Debug)]
pub struct BoundaryError {
    kind: BoundaryKind,
    diagnostic: String,
    transcript_failure: Option<TranscriptFailureEvidence>,
    provider_failures: Vec<ProviderFailure>,
}

#[derive(Clone, Debug)]
pub struct TranscriptFailureEvidence {
    pub validation_reason: String,
    pub fallback_reason: Option<String>,
    pub reconciliation_requested: bool,
    pub recovery_attempted: bool,
}

impl BoundaryError {
    pub fn new(kind: BoundaryKind, diagnostic: impl Into<String>) -> Self {
        Self {
            kind,
            diagnostic: diagnostic.into(),
            transcript_failure: None,
            provider_failures: Vec::new(),
        }
    }

    pub fn with_transcript_failure(mut self, evidence: TranscriptFailureEvidence) -> Self {
        self.transcript_failure = Some(evidence);
        self
    }

    pub fn transcript_failure(&self) -> Option<&TranscriptFailureEvidence> {
        self.transcript_failure.as_ref()
    }

    /// Attaches provider-failure evidence to an error so a failure on a path
    /// that produces NO usable Source Transcript (both providers failed, or a
    /// deadline-cleanup failed) still reaches the history record instead of
    /// being discarded with the error.
    pub fn with_provider_failures(mut self, failures: Vec<ProviderFailure>) -> Self {
        self.provider_failures = failures;
        self
    }

    pub fn provider_failures(&self) -> &[ProviderFailure] {
        &self.provider_failures
    }

    pub fn kind(&self) -> BoundaryKind {
        self.kind
    }

    pub fn public_message(&self) -> &'static str {
        match self.kind {
            BoundaryKind::Capture => "Recording capture failed",
            BoundaryKind::EmptyRecording => "No audio was captured",
            BoundaryKind::TooShortRecording => "Recording is too short",
            BoundaryKind::SilentRecording => "Recording contains no speech",
            BoundaryKind::RecordingDeadline => "Recording Deadline elapsed",
            BoundaryKind::Provider => "Source Transcripts are unavailable",
            BoundaryKind::Validation => "Transcript failed quality validation",
            BoundaryKind::Delivery => "Transcript Delivery failed",
            BoundaryKind::SecretStorage => {
                "Secret storage is unavailable; set VOISU_GROQ_API_KEY or VOISU_DEEPGRAM_API_KEY for development or headless use"
            }
            BoundaryKind::ProviderAuthentication => "Provider authentication failed",
            BoundaryKind::Shortcut => "Trigger Key binding is unavailable",
        }
    }

    pub fn diagnostic(&self) -> &str {
        &self.diagnostic
    }
}

pub type BoundaryFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, BoundaryError>> + Send + 'a>>;

#[derive(Clone, Debug)]
pub struct AudioChunk(pub Vec<u8>);

#[derive(Clone, Debug)]
pub struct CapturedAudio {
    pcm_s16le_mono_16khz: Vec<u8>,
}

impl CapturedAudio {
    pub fn new(pcm_s16le_mono_16khz: Vec<u8>) -> Self {
        Self {
            pcm_s16le_mono_16khz,
        }
    }

    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    pub fn pcm_s16le_mono_16khz(&self) -> &[u8] {
        &self.pcm_s16le_mono_16khz
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Deepgram,
    Groq,
}

impl Provider {
    pub fn cli_label(self) -> &'static str {
        match self {
            Self::Deepgram => "Deepgram",
            Self::Groq => "Groq",
        }
    }

    pub fn environment_variable(self) -> &'static str {
        match self {
            Self::Deepgram => "VOISU_DEEPGRAM_API_KEY",
            Self::Groq => "VOISU_GROQ_API_KEY",
        }
    }

    pub fn secret_service_value(self) -> &'static str {
        match self {
            Self::Deepgram => "deepgram",
            Self::Groq => "groq",
        }
    }
}

/// An API credential deliberately has no `Debug` implementation, preventing
/// accidental exposure through ordinary diagnostics.
#[derive(Clone)]
pub struct Credential(Arc<str>);

impl Credential {
    pub fn new(value: String) -> Result<Self, BoundaryError> {
        if value.is_empty() || value.contains(['\n', '\r']) {
            return Err(BoundaryError::new(
                BoundaryKind::SecretStorage,
                "credential is empty or contains a line break",
            ));
        }
        Ok(Self(value.into()))
    }

    pub fn expose_to_boundary(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadinessStatus {
    Pass,
    Warn,
    Fail,
}

impl ReadinessStatus {
    pub fn cli_label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadinessCapability {
    PipeWire,
    Microphone,
    Portals,
    Clipboard,
    SecretStorage,
    Daemon,
}

impl ReadinessCapability {
    pub fn cli_label(self) -> &'static str {
        match self {
            Self::PipeWire => "PipeWire",
            Self::Microphone => "Microphone",
            Self::Portals => "Portals",
            Self::Clipboard => "Clipboard",
            Self::SecretStorage => "Secret storage",
            Self::Daemon => "Daemon",
        }
    }
}

pub struct ReadinessFinding {
    pub capability: ReadinessCapability,
    pub status: ReadinessStatus,
    pub detail: String,
}

/// Boundary for Fedora desktop capability checks. Production uses thin command
/// probes; tests inject controlled outcomes without a desktop session.
pub trait ReadinessInspector: Send {
    fn inspect(&mut self) -> Vec<ReadinessFinding>;
}

/// Boundary for desktop Secret Service. Implementations must never persist a
/// credential outside the desktop secret service.
pub trait SecretStore: Send {
    fn replace(&mut self, provider: Provider, credential: Credential) -> Result<(), BoundaryError>;
    fn load(&mut self, provider: Provider) -> Result<Credential, BoundaryError>;
}

/// Boundary for an independent, post-storage provider-auth check. It returns
/// no provider response content, only an authorization result.
pub trait ProviderAuthenticator: Send {
    fn verify(&mut self, provider: Provider, credential: Credential) -> BoundaryFuture<'_, ()>;
}

#[derive(Clone, Debug)]
pub struct SourceTranscript {
    pub provider: Provider,
    pub text: String,
}

#[derive(Debug)]
pub struct Transcript(pub String);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergeResult(pub String);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReconciliationKind {
    Reconcile,
    Repair,
}

/// Cancellation flag shared between an owner and its in-flight boundary
/// operation. It deliberately stores NO pids: signaling a raw pid is unsafe
/// once reaping happens elsewhere (a reaped pid can be recycled by the kernel
/// and the signal would land on an unrelated process). `cancel()` only sets
/// the flag; the bounded loop that OWNS each subprocess handle observes it on
/// its next poll tick and kills through its own handle — pid-reuse-safe
/// because that same loop is the only reaper, so the handle cannot be
/// recycled while unreaped.
pub struct CancelRegistry {
    cancelled: AtomicBool,
}

impl CancelRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            cancelled: AtomicBool::new(false),
        })
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst)
    }
}

/// Grace granted, after the reconciliation deadline cancels an in-flight
/// request, for the cancelled request to kill, reap, and surrender its
/// subprocess before the fallback Transcript becomes observable. A
/// cancel-honoring model completes within one subprocess poll tick plus a
/// brief reap, well inside this bound.
const RECONCILIATION_CLEANUP_GRACE: Duration = Duration::from_secs(1);

pub trait ReconciliationModel: Send {
    /// Requests a Merge Result. The request MUST observe `cancel`: once the
    /// flag is set, any subprocess it owns must be killed and reaped, and the
    /// returned future must complete promptly — the pipeline keeps the future
    /// owned after its deadline and awaits it under a bounded grace instead of
    /// detaching the work.
    fn request(
        &mut self,
        kind: ReconciliationKind,
        sources: Vec<SourceTranscript>,
        candidate: Option<MergeResult>,
        cancel: Arc<CancelRegistry>,
    ) -> BoundaryFuture<'_, MergeResult>;
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptSelection {
    NearIdenticalGroq,
    Reconciled,
    Repaired,
    SourceDeepgram,
    SourceGroq,
}

#[derive(Debug)]
pub struct TranscriptDecision {
    pub transcript: Transcript,
    pub selection: TranscriptSelection,
    pub validation_reason: String,
    pub fallback_reason: Option<String>,
    pub reconciliation_requested: bool,
    pub recovery_attempted: bool,
}

pub struct TranscriptDecisionPipeline<M> {
    model: M,
    deadline: Duration,
}

impl<M: ReconciliationModel> TranscriptDecisionPipeline<M> {
    pub fn new(model: M, deadline: Duration) -> Self {
        Self { model, deadline }
    }

    pub async fn decide(
        &mut self,
        mut sources: Vec<SourceTranscript>,
    ) -> Result<TranscriptDecision, BoundaryError> {
        sources.sort_by_key(|source| source.provider);
        if let (Some(deepgram), Some(groq)) = (
            sources.iter().find(|source| source.provider == Provider::Deepgram),
            sources.iter().find(|source| source.provider == Provider::Groq),
        ) {
            if source_similarity(&deepgram.text, &groq.text) >= 0.85 {
                if let Some(reason) = quality_failure_reason(&groq.text, &sources) {
                    return self
                        .repair_candidate(
                            &sources,
                            MergeResult(groq.text.trim().to_owned()),
                            reason,
                            false,
                        )
                        .await;
                }
                return Ok(TranscriptDecision {
                    transcript: Transcript(groq.text.trim().to_owned()),
                    selection: TranscriptSelection::NearIdenticalGroq,
                    validation_reason: "near-identical Source Transcripts passed validation"
                        .to_owned(),
                    fallback_reason: None,
                    reconciliation_requested: false,
                    recovery_attempted: false,
                });
            }

            // Source-quality gate (§3.4): the two Source Transcripts materially
            // disagreed. Before spending an LLM merge, check whether the pair is
            // catastrophically divergent by a ROBUST garbage signal — a
            // degenerate filler/repetition loop (the recording-11 word-salad
            // case), a bare fragment, or near-zero cross-source content
            // agreement between comparable sources (fluent nonsense / a
            // unique-word salad, which no intrinsic check can flag). If so, skip
            // the merge and select the Source Transcript the evidence supports.
            // Real disagreements over shared content still fall through to
            // reconciliation.
            if let Some(gate) = source_quality_gate(&deepgram.text, &groq.text) {
                // The caller passes (deepgram.text, groq.text) in that order, so
                // Left maps to Deepgram and Right to Groq. The gate itself holds
                // no provider preference.
                let winner = match gate.winner {
                    GateWinner::Left => deepgram,
                    GateWinner::Right => groq,
                };
                if quality_failure_reason(&winner.text, &sources).is_none() {
                    return Ok(TranscriptDecision {
                        transcript: Transcript(winner.text.trim().to_owned()),
                        selection: match winner.provider {
                            Provider::Deepgram => TranscriptSelection::SourceDeepgram,
                            Provider::Groq => TranscriptSelection::SourceGroq,
                        },
                        validation_reason:
                            "catastrophically divergent Source Transcripts; selected the better source without merging"
                                .to_owned(),
                        fallback_reason: Some(gate.reason),
                        reconciliation_requested: false,
                        recovery_attempted: false,
                    });
                }
                // The better source itself failed a quality guardrail: fall
                // through and let reconciliation/repair handle it.
            }

            let merge_result = {
                let cancel = CancelRegistry::new();
                let request = self.model.request(
                    ReconciliationKind::Reconcile,
                    sources.clone(),
                    None,
                    Arc::clone(&cancel),
                );
                tokio::pin!(request);
                match tokio::time::timeout(self.deadline, request.as_mut()).await {
                    Ok(Ok(merge_result)) => merge_result,
                    Ok(Err(error)) => {
                        return clean_source_fallback(
                            &sources,
                            format!("cloud reconciliation failed: {}", error.diagnostic()),
                            true,
                            false,
                        );
                    }
                    Err(_) => {
                        // The deadline elapsed with the request still owned
                        // (pinned above, never dropped): cancel it so the model
                        // kills and reaps any subprocess it spawned, then await
                        // the SAME future under a bounded grace so no
                        // reconciliation work survives past the fallback
                        // becoming observable.
                        cancel.cancel();
                        let _ =
                            tokio::time::timeout(RECONCILIATION_CLEANUP_GRACE, request.as_mut())
                                .await;
                        return clean_source_fallback(
                            &sources,
                            "cloud reconciliation deadline elapsed".to_owned(),
                            true,
                            false,
                        );
                    }
                }
            };
            if let Some(reason) = quality_failure_reason(&merge_result.0, &sources) {
                return self
                    .repair_candidate(&sources, merge_result, reason, true)
                    .await;
            }
            return Ok(TranscriptDecision {
                transcript: Transcript(merge_result.0.trim().to_owned()),
                selection: TranscriptSelection::Reconciled,
                validation_reason: "Merge Result passed validation".to_owned(),
                fallback_reason: None,
                reconciliation_requested: true,
                recovery_attempted: false,
            });
        }

        let source = sources.first().ok_or_else(|| {
            BoundaryError::new(BoundaryKind::Validation, "no Source Transcript")
        })?;
        if let Some(reason) = quality_failure_reason(&source.text, &sources) {
            return self
                .repair_candidate(
                    &sources,
                    MergeResult(source.text.trim().to_owned()),
                    reason,
                    false,
                )
                .await;
        }
        Ok(TranscriptDecision {
            transcript: Transcript(source.text.trim().to_owned()),
            selection: match source.provider {
                Provider::Deepgram => TranscriptSelection::SourceDeepgram,
                Provider::Groq => TranscriptSelection::SourceGroq,
            },
            validation_reason: "Source Transcript passed validation".to_owned(),
            fallback_reason: None,
            reconciliation_requested: false,
            recovery_attempted: false,
        })
    }

    async fn repair_candidate(
        &mut self,
        sources: &[SourceTranscript],
        candidate: MergeResult,
        reason: &'static str,
        reconciliation_requested: bool,
    ) -> Result<TranscriptDecision, BoundaryError> {
        let repaired = {
            let cancel = CancelRegistry::new();
            let request = self.model.request(
                ReconciliationKind::Repair,
                sources.to_vec(),
                Some(candidate),
                Arc::clone(&cancel),
            );
            tokio::pin!(request);
            match tokio::time::timeout(self.deadline, request.as_mut()).await {
                Ok(Ok(repaired)) => repaired,
                Ok(Err(error)) => {
                    return clean_source_fallback(
                        sources,
                        format!("recovery failed: {}", error.diagnostic()),
                        reconciliation_requested,
                        true,
                    );
                }
                Err(_) => {
                    // Same owned-handle discipline as the reconcile path: the
                    // request future stays pinned across its deadline, so cancel
                    // and await it under the bounded grace — its subprocess must
                    // be killed and reaped before the fallback is observable.
                    cancel.cancel();
                    let _ = tokio::time::timeout(RECONCILIATION_CLEANUP_GRACE, request.as_mut())
                        .await;
                    return clean_source_fallback(
                        sources,
                        "recovery deadline elapsed".to_owned(),
                        reconciliation_requested,
                        true,
                    );
                }
            }
        };
        if let Some(repair_reason) = quality_failure_reason(&repaired.0, sources) {
            return clean_source_fallback(
                sources,
                format!("recovery produced {repair_reason}"),
                reconciliation_requested,
                true,
            );
        }
        Ok(TranscriptDecision {
            transcript: Transcript(repaired.0.trim().to_owned()),
            selection: TranscriptSelection::Repaired,
            validation_reason: format!("repaired {reason}"),
            fallback_reason: None,
            reconciliation_requested,
            recovery_attempted: true,
        })
    }
}

fn clean_source_fallback(
    sources: &[SourceTranscript],
    reason: String,
    reconciliation_requested: bool,
    recovery_attempted: bool,
) -> Result<TranscriptDecision, BoundaryError> {
    // Select among the quality-safe sources by the SAME cross-source-evidence
    // comparator the divergence gate uses — never a fixed provider preference,
    // and never an intrinsic score alone, which a fluent unique-word salad can
    // inflate past accurate repetitive dictation.
    let safe: Vec<&SourceTranscript> = sources
        .iter()
        .filter(|source| {
            quality_failure_reason(&source.text, std::slice::from_ref(*source)).is_none()
        })
        .collect();
    let source = match safe.as_slice() {
        [] => None,
        [only] => Some(*only),
        [left, right, ..] => Some(
            match select_better_source(
                &normalized_words(&left.text),
                &normalized_words(&right.text),
            ) {
                GateWinner::Left => *left,
                GateWinner::Right => *right,
            },
        ),
    };
    let source = source
        .ok_or_else(|| {
            let validation_reason = format!("{reason}; neither Source Transcript is safe");
            BoundaryError::new(
                BoundaryKind::Validation,
                validation_reason.clone(),
            )
            .with_transcript_failure(TranscriptFailureEvidence {
                validation_reason,
                fallback_reason: Some(reason.clone()),
                reconciliation_requested,
                recovery_attempted,
            })
        })?;
    Ok(TranscriptDecision {
        transcript: Transcript(source.text.trim().to_owned()),
        selection: match source.provider {
            Provider::Deepgram => TranscriptSelection::SourceDeepgram,
            Provider::Groq => TranscriptSelection::SourceGroq,
        },
        validation_reason: "clean Source Transcript passed validation".to_owned(),
        fallback_reason: Some(reason),
        reconciliation_requested,
        recovery_attempted,
    })
}

/// A Source Transcript shorter than roughly a third of the other's length is a
/// fragment, not a comparable transcription of the same speech.
const DIVERGENCE_LENGTH_RATIO_FLOOR: f64 = 0.34;

/// The side of the compared pair a gate selected. The caller maps this back to
/// the concrete Provider, so the gate itself carries no provider preference.
enum GateWinner {
    Left,
    Right,
}

/// The decision to skip the LLM merge and select a better Source Transcript.
struct QualityGate {
    winner: GateWinner,
    reason: String,
}

/// English function words plus common spoken fillers, excluded from content
/// density and content-count measurement. A word salad from context-free slices
/// is dominated by these; a real technical dictation is not.
const STOPWORDS: [&str; 94] = [
    "a", "an", "and", "or", "but", "of", "to", "in", "on", "at", "for", "with", "is", "are", "was",
    "were", "be", "been", "being", "am", "the", "this", "that", "these", "those", "it", "its", "as",
    "by", "from", "into", "onto", "over", "under", "out", "up", "down", "off", "so", "then", "than",
    "we", "you", "i", "he", "she", "they", "them", "our", "your", "my", "me", "his", "her", "their",
    "do", "does", "did", "not", "no", "yes", "if", "when", "while", "about", "before", "after",
    "near", "would", "could", "should", "will", "can", "um", "uh", "uhh", "yeah", "like", "just",
    "kind", "sort", "mean", "know", "well", "okay", "ok", "there", "here", "gonna", "wanna", "sorta",
    "kinda", "really", "actually",
];

fn is_stopword(word: &str) -> bool {
    STOPWORDS.contains(&word)
}

fn distinct_content_words(words: &[String]) -> HashSet<&str> {
    words
        .iter()
        .filter(|word| !is_stopword(word))
        .map(String::as_str)
        .collect()
}

/// A content-density quality score in [0, 1], used ONLY to break a
/// clean-source fallback tie (never to decide gating). It deliberately does NOT
/// reward lexical uniqueness — an earlier type-token-ratio term let a salad of
/// all-unique words outscore accurate dictation that repeats real content words
/// (e.g. "cache … cache invalidation … cache"). It rewards content-word density
/// and the count of distinct content words, penalizing only adjacent-word
/// stutter, so repeating real content is never scored below word salad.
fn source_quality(words: &[String]) -> f64 {
    let total = words.len();
    if total == 0 {
        return 0.0;
    }
    let content_count = words.iter().filter(|word| !is_stopword(word)).count();
    let distinct_content = distinct_content_words(words).len();
    let content_fraction = content_count as f64 / total as f64;
    let richness = (distinct_content as f64 / 8.0).min(1.0);
    let duplication = words
        .windows(2)
        .filter(|pair| pair[0] == pair[1])
        .count() as f64
        / total as f64;
    (0.6 * content_fraction + 0.4 * richness) * (1.0 - duplication)
}

/// True when a Source Transcript is internally degenerate — a filler or
/// repetition loop with almost no distinct content (context-free 1 s slices, or
/// a "the/and/to/is" loop). This is a ROBUST garbage signal: it triggers on
/// near-absent content, NOT on mere repetition, so legitimate jargon-heavy or
/// naturally repetitive dictation ("the cache … the cache") is never flagged.
fn is_degenerate(words: &[String]) -> bool {
    let total = words.len();
    if total < 6 {
        // Too short to distinguish degeneracy from a terse-but-valid utterance.
        return false;
    }
    let content_count = words.iter().filter(|word| !is_stopword(word)).count();
    let distinct_content = distinct_content_words(words).len();
    let content_fraction = content_count as f64 / total as f64;
    content_fraction < 0.25 || distinct_content < 3
}

/// Two Source Transcripts of the same audio must agree on a meaningful share of
/// content words. Below this containment (shared distinct content words over
/// the smaller distinct-content set) they cannot both be transcriptions of the
/// same speech: one of them is garbage, and merging would poison the result.
const CONTENT_OVERLAP_FLOOR: f64 = 0.2;

/// Sources with fewer distinct content words than this are too short for the
/// cross-agreement gate to judge — two terse commands ("book the room" vs
/// "schedule the review") can honestly share nothing, so they reconcile.
const MIN_COMPARABLE_CONTENT: usize = 5;

/// Cross-confirmation differences below this margin are noise, not a decision.
const CONFIRMATION_MARGIN: f64 = 0.15;

/// Evidence for choosing between two disagreeing Source Transcripts, ordered by
/// robustness. `confirmation` and `cohesion` are the primary signals because
/// neither can be inflated by a fluent salad: confirmation requires the OTHER
/// source to agree, and cohesion requires revisiting a real topic term, which a
/// stream of unique nonsense words never does.
struct SourceEvidence {
    /// Fraction of this source's content-word occurrences that appear in the
    /// other source's distinct content vocabulary.
    confirmation: f64,
    /// Distinct content words this source returns to at non-adjacent positions.
    cohesion: usize,
    /// Intrinsic content-density score — a last-resort tiebreak only, because
    /// it is the one signal a fluent salad can game.
    quality: f64,
}

fn source_evidence(own: &[String], other: &[String]) -> SourceEvidence {
    let other_content = distinct_content_words(other);
    let content: Vec<&str> = own
        .iter()
        .filter(|word| !is_stopword(word))
        .map(String::as_str)
        .collect();
    let confirmation = if content.is_empty() {
        0.0
    } else {
        content
            .iter()
            .filter(|word| other_content.contains(**word))
            .count() as f64
            / content.len() as f64
    };
    SourceEvidence {
        confirmation,
        cohesion: topical_cohesion(own),
        quality: source_quality(own),
    }
}

/// Counts distinct content words a transcript returns to at separated
/// positions. Real dictation revisits its topic terms ("cache ... cache
/// invalidation"); a salad of unique words never does. Adjacent repeats are
/// stutter, not cohesion, so they deliberately do not count.
fn topical_cohesion(words: &[String]) -> usize {
    let mut positions: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, word) in words.iter().enumerate() {
        if !is_stopword(word) {
            positions.entry(word.as_str()).or_default().push(index);
        }
    }
    positions
        .values()
        .filter(|occurrences| {
            occurrences
                .windows(2)
                .any(|pair| pair[1] - pair[0] > 1)
        })
        .count()
}

/// Chooses between two disagreeing Source Transcripts by ordered cross-source
/// evidence, never by provider and never by an intrinsic score alone:
/// cross-confirmation first, topical cohesion second, intrinsic content density
/// last. On a full tie it keeps the later element — the same rare, low-stakes
/// exact-tie behavior the fallback has always had.
fn select_better_source(left: &[String], right: &[String]) -> GateWinner {
    let left_evidence = source_evidence(left, right);
    let right_evidence = source_evidence(right, left);
    if (left_evidence.confirmation - right_evidence.confirmation).abs() > CONFIRMATION_MARGIN {
        return if left_evidence.confirmation > right_evidence.confirmation {
            GateWinner::Left
        } else {
            GateWinner::Right
        };
    }
    if left_evidence.cohesion != right_evidence.cohesion {
        return if left_evidence.cohesion > right_evidence.cohesion {
            GateWinner::Left
        } else {
            GateWinner::Right
        };
    }
    if left_evidence.quality > right_evidence.quality {
        GateWinner::Left
    } else {
        GateWinner::Right
    }
}

/// Decides whether to skip the LLM merge for two materially disagreeing Source
/// Transcripts and select the better one. It gates on three robust garbage
/// signals: exactly one source is a degenerate filler/repetition loop, one is a
/// bare fragment (extreme length ratio), or the two share near-zero content
/// words despite comparable length — two transcriptions of the same audio
/// cannot do that, so one is fluent nonsense or a word salad. Sources that
/// clear all three (real disagreements over shared content) return `None` and
/// go to the reconciliation model.
fn source_quality_gate(left: &str, right: &str) -> Option<QualityGate> {
    let left_words = normalized_words(left);
    let right_words = normalized_words(right);
    let fewer = left_words.len().min(right_words.len());
    let more = left_words.len().max(right_words.len());
    if fewer == 0 {
        return None;
    }
    let left_degenerate = is_degenerate(&left_words);
    let right_degenerate = is_degenerate(&right_words);

    if left_degenerate == right_degenerate {
        let length_ratio = fewer as f64 / more as f64;
        if length_ratio < DIVERGENCE_LENGTH_RATIO_FLOOR {
            // One source is a fragment: the fuller transcription carries the
            // content, so select it and skip a merge with a stub.
            let winner = if left_words.len() >= right_words.len() {
                GateWinner::Left
            } else {
                GateWinner::Right
            };
            return Some(QualityGate {
                winner,
                reason: format!(
                    "catastrophically divergent (length ratio {length_ratio:.2} below {DIVERGENCE_LENGTH_RATIO_FLOOR:.2}); selected the fuller Source Transcript, the other is a fragment"
                ),
            });
        }
        // Cross-agreement check (§3.4): two comparable-length, individually
        // coherent transcriptions of the SAME audio must still agree on a
        // meaningful share of content words. Near-zero agreement means one of
        // them is garbage — fluent nonsense or a unique-word salad that no
        // intrinsic check can flag — and an LLM merge would let it poison the
        // result. Short pairs are exempt: terse commands can honestly share
        // nothing.
        if !left_degenerate {
            let left_content = distinct_content_words(&left_words);
            let right_content = distinct_content_words(&right_words);
            let smaller_content = left_content.len().min(right_content.len());
            if smaller_content >= MIN_COMPARABLE_CONTENT {
                let shared = left_content.intersection(&right_content).count();
                let overlap = shared as f64 / smaller_content as f64;
                if overlap < CONTENT_OVERLAP_FLOOR {
                    let winner = select_better_source(&left_words, &right_words);
                    return Some(QualityGate {
                        winner,
                        reason: format!(
                            "catastrophically divergent (cross-source content-word overlap {overlap:.2} below {CONTENT_OVERLAP_FLOOR:.2}); selected the Source Transcript better supported by cross-source evidence"
                        ),
                    });
                }
            }
        }
        // Sources agree enough to be transcriptions of the same speech (or are
        // both garbage, or too short to judge): no confident single winner, so
        // reconcile rather than guess.
        return None;
    }

    // Exactly one source is a degenerate garbage loop: select the coherent one.
    let winner = if left_degenerate {
        GateWinner::Right
    } else {
        GateWinner::Left
    };
    Some(QualityGate {
        winner,
        reason:
            "catastrophically divergent (one Source Transcript is a degenerate filler/repetition loop); selected the coherent Source Transcript"
                .to_owned(),
    })
}

fn source_similarity(left: &str, right: &str) -> f64 {
    let left = normalized_words(left);
    let right = normalized_words(right);
    let longest = left.len().max(right.len());
    if longest == 0 {
        return 1.0;
    }
    1.0 - word_edit_distance(&left, &right) as f64 / longest as f64
}

fn quality_failure_reason(
    candidate: &str,
    sources: &[SourceTranscript],
) -> Option<&'static str> {
    let trimmed = candidate.trim();
    if trimmed.is_empty() || trimmed.contains('\0') || trimmed.len() > 100_000 {
        return Some("invalid candidate text");
    }
    let lower = trimmed.to_lowercase();
    const PROMPT_ARTIFACTS: [&str; 8] = [
        "ignore previous instructions",
        "ignore all instructions",
        "system prompt",
        "system:",
        "assistant:",
        "<|system|>",
        "<|assistant|>",
        "### instruction",
    ];
    if PROMPT_ARTIFACTS
        .iter()
        .any(|artifact| lower.contains(artifact))
    {
        return Some("prompt artifact");
    }
    const META_REASONING: [&str; 6] = [
        "i think the user said",
        "the user said",
        "my final answer",
        "here is the transcript",
        "here is the reconciled",
        "based on the source",
    ];
    if META_REASONING.iter().any(|artifact| lower.contains(artifact)) {
        return Some("meta-reasoning");
    }
    const HALLUCINATED_SUFFIXES: [&str; 5] = [
        "thank you for watching",
        "thanks for watching",
        "like and subscribe",
        "subtitles by",
        "transcribed by",
    ];
    if HALLUCINATED_SUFFIXES
        .iter()
        .any(|suffix| lower.contains(suffix))
    {
        return Some("hallucinated suffix");
    }
    if script_count(trimmed) >= 3
        || trimmed
            .split_whitespace()
            .any(token_mixes_confusable_scripts)
    {
        return Some("mixed-script garbage");
    }
    let source_words = sources
        .iter()
        .map(|source| normalized_words(&source.text).len())
        .max()
        .unwrap_or(0);
    let candidate_words = normalized_words(trimmed).len();
    if source_words > 0 && candidate_words > source_words.saturating_mul(2).saturating_add(8) {
        return Some("suspicious expansion");
    }
    None
}

/// Latin, Greek, and Cyrillic letters are visually confusable: a single token
/// drawing letters from more than one of these scripts is a homoglyph or
/// garbage signature (e.g. a Latin word smuggling a Cyrillic "а"), while
/// legitimate bilingual dictation keeps each token in one script — so mixing
/// scripts across separate tokens stays permitted.
fn token_mixes_confusable_scripts(token: &str) -> bool {
    let mut latin = false;
    let mut greek = false;
    let mut cyrillic = false;
    for character in token.chars().filter(|character| character.is_alphabetic()) {
        match confusable_script(character) {
            Some(ConfusableScript::Latin) => latin = true,
            Some(ConfusableScript::Greek) => greek = true,
            Some(ConfusableScript::Cyrillic) => cyrillic = true,
            None => {}
        }
    }
    usize::from(latin) + usize::from(greek) + usize::from(cyrillic) >= 2
}

#[derive(Clone, Copy)]
enum ConfusableScript {
    Latin,
    Greek,
    Cyrillic,
}

/// Classifies a character into one of the visually confusable scripts by its
/// Unicode Script property, with the range tables completed by hand across
/// EVERY block each script occupies — a homoglyph drawn from an extended
/// block (Greek Extended, Cyrillic Extended-B, ...) must classify the same as
/// its base-block siblings.
fn confusable_script(character: char) -> Option<ConfusableScript> {
    match character as u32 {
        0x0041..=0x024f // Basic Latin, Latin-1 Supplement, Extended-A/B
        | 0x1e00..=0x1eff // Latin Extended Additional
        | 0x2c60..=0x2c7f // Latin Extended-C
        | 0xa720..=0xa7ff // Latin Extended-D
        | 0xab30..=0xab6f // Latin Extended-E
        | 0x10780..=0x107bf // Latin Extended-F
        | 0x1df00..=0x1dfff // Latin Extended-G
        => Some(ConfusableScript::Latin),
        0x0370..=0x03ff // Greek and Coptic
        | 0x1f00..=0x1fff // Greek Extended
        => Some(ConfusableScript::Greek),
        0x0400..=0x052f // Cyrillic, Cyrillic Supplement
        | 0x1c80..=0x1c8f // Cyrillic Extended-C
        | 0x2de0..=0x2dff // Cyrillic Extended-A
        | 0xa640..=0xa69f // Cyrillic Extended-B
        | 0x1e030..=0x1e08f // Cyrillic Extended-D
        => Some(ConfusableScript::Cyrillic),
        _ => None,
    }
}

fn script_count(text: &str) -> usize {
    let mut scripts = [false; 7];
    for character in text.chars().filter(|character| character.is_alphabetic()) {
        let index = match confusable_script(character) {
            Some(ConfusableScript::Latin) => 0,
            Some(ConfusableScript::Greek) => 1,
            Some(ConfusableScript::Cyrillic) => 2,
            None => match character as u32 {
                0x0600..=0x06ff => 3, // Arabic
                0x0900..=0x097f => 4, // Devanagari
                0x3040..=0x30ff | 0x3400..=0x9fff => 5, // Japanese/CJK
                _ => 6,
            },
        };
        scripts[index] = true;
    }
    scripts.into_iter().filter(|present| *present).count()
}

fn normalized_words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|word| {
            word.chars()
                .filter(|character| character.is_alphanumeric())
                .flat_map(char::to_lowercase)
                .collect::<String>()
        })
        .filter(|word| !word.is_empty())
        .collect()
}

fn word_edit_distance(left: &[String], right: &[String]) -> usize {
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0; right.len() + 1];
    for (left_index, left_word) in left.iter().enumerate() {
        current[0] = left_index + 1;
        for (right_index, right_word) in right.iter().enumerate() {
            current[right_index + 1] = if left_word == right_word {
                previous[right_index]
            } else {
                1 + previous[right_index]
                    .min(current[right_index])
                    .min(previous[right_index + 1])
            };
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

pub trait AudioCapture: Send {
    fn begin(&mut self, recording_id: u64) -> Result<Box<dyn ActiveCapture>, BoundaryError>;
}

pub trait ActiveCapture: Send {
    /// Yields the next live audio chunk for this Recording, or `None` once the
    /// capture has no further chunks to stream before it is finished.
    fn next_chunk(&mut self) -> BoundaryFuture<'_, Option<AudioChunk>>;
    fn finish(&mut self) -> BoundaryFuture<'_, CapturedAudio>;
    fn abort(self: Box<Self>) -> BoundaryFuture<'static, ()>;
}

pub trait TranscriptProvider: Send {
    fn start(&mut self, recording_id: u64) -> Result<Box<dyn ProviderStream>, BoundaryError>;
}

pub trait ProviderStream: Send {
    fn provider(&self) -> Provider;
    fn send_audio(&mut self, chunk: AudioChunk) -> BoundaryFuture<'_, ()>;
    fn abort(self: Box<Self>) -> BoundaryFuture<'static, ()>;
    fn complete(&mut self, audio: CapturedAudio) -> BoundaryFuture<'_, SourceTranscript>;
}

pub struct ProviderStreams {
    pub deepgram: Box<dyn ProviderStream>,
    pub groq: Box<dyn ProviderStream>,
}

pub struct ProviderCoordinator {
    deadline: Duration,
    abort_deadline: Duration,
    streams: ProviderStreams,
}

#[derive(Debug)]
pub struct ProviderCompletion {
    pub sources: Vec<SourceTranscript>,
    pub timings_ms: Vec<ProviderTiming>,
    /// Every configured provider that did NOT contribute a Source Transcript for
    /// this Recording — one that failed producing its transcript or that missed
    /// the Provider Deadline — with its stage and boundary diagnostic. This is
    /// what keeps a missing Source Transcript visible instead of silent.
    pub provider_failures: Vec<ProviderFailure>,
}

impl ProviderCoordinator {
    pub fn start(deadline: Duration, abort_deadline: Duration, streams: ProviderStreams) -> Self {
        Self {
            deadline,
            abort_deadline,
            streams,
        }
    }

    pub async fn stream_audio(&mut self, chunk: AudioChunk) -> Result<(), BoundaryError> {
        let deepgram = self.streams.deepgram.send_audio(chunk.clone());
        let groq = self.streams.groq.send_audio(chunk);
        let (deepgram, groq) = tokio::join!(deepgram, groq);
        // A live streaming failure aborts the Recording. Attribute it to the
        // failing provider(s) as a Streaming-stage ProviderFailure so the
        // abort path can carry it into history instead of losing which provider
        // broke and where.
        let mut failures = Vec::new();
        let mut first_error: Option<BoundaryError> = None;
        for (provider, result) in [(Provider::Deepgram, deepgram), (Provider::Groq, groq)] {
            if let Err(error) = result {
                failures.push(ProviderFailure::new(
                    provider,
                    ProviderFailureStage::Streaming,
                    error.diagnostic().to_owned(),
                ));
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error.with_provider_failures(failures)),
            None => Ok(()),
        }
    }

    pub async fn abort(self) -> Result<(), BoundaryError> {
        let deepgram = self.streams.deepgram.abort();
        let groq = self.streams.groq.abort();
        tokio::time::timeout(self.deadline, async move {
            let (deepgram, groq) = tokio::join!(deepgram, groq);
            deepgram?;
            groq
        })
        .await
        .map_err(|_| {
            BoundaryError::new(BoundaryKind::Provider, "provider abort deadline elapsed")
        })?
    }

    pub async fn complete(
        self,
        audio: CapturedAudio,
    ) -> Result<Vec<SourceTranscript>, BoundaryError> {
        Ok(self.complete_with_timings(audio).await?.sources)
    }

    pub async fn complete_with_timings(
        self,
        audio: CapturedAudio,
    ) -> Result<ProviderCompletion, BoundaryError> {
        let started = tokio::time::Instant::now();
        let ProviderStreams {
            mut deepgram,
            mut groq,
        } = self.streams;
        let mut deepgram_done = false;
        let mut groq_done = false;
        let mut deadline_elapsed = false;
        let mut transcripts = Vec::new();
        let mut timings_ms = Vec::new();
        let mut provider_failures: Vec<ProviderFailure> = Vec::new();
        // A failure aborting the losing (deadline) stream. It becomes the error
        // ONLY when no provider succeeded; when a winner exists it must never
        // erase that winner, so it is instead annotated onto the loser's entry.
        let mut cleanup_error: Option<BoundaryError> = None;

        {
            let deepgram_completion = deepgram.complete(audio.clone());
            let groq_completion = groq.complete(audio);
            tokio::pin!(deepgram_completion, groq_completion);
            let deadline = tokio::time::sleep(self.deadline);
            tokio::pin!(deadline);

            while !deepgram_done || !groq_done {
                tokio::select! {
                    // Bias toward provider results: if a valid Source Transcript is
                    // ready in the same poll as the Provider Deadline, honor the
                    // transcript instead of discarding it at the deadline instant.
                    biased;
                    result = &mut deepgram_completion, if !deepgram_done => {
                        deepgram_done = true;
                        match result {
                            Ok(source) => {
                                timings_ms.push(ProviderTiming {
                                    provider: source.provider,
                                    completed_ms: duration_millis(started.elapsed()),
                                });
                                transcripts.push(source);
                            }
                            Err(error) => provider_failures.push(ProviderFailure::new(
                                Provider::Deepgram,
                                ProviderFailureStage::Completion,
                                error.diagnostic().to_owned(),
                            )),
                        }
                    }
                    result = &mut groq_completion, if !groq_done => {
                        groq_done = true;
                        match result {
                            Ok(source) => {
                                timings_ms.push(ProviderTiming {
                                    provider: source.provider,
                                    completed_ms: duration_millis(started.elapsed()),
                                });
                                transcripts.push(source);
                            }
                            Err(error) => provider_failures.push(ProviderFailure::new(
                                Provider::Groq,
                                ProviderFailureStage::Completion,
                                error.diagnostic().to_owned(),
                            )),
                        }
                    }
                    _ = &mut deadline => {
                        deadline_elapsed = true;
                        break;
                    },
                }
            }
        }

        if deadline_elapsed {
            // A provider that never produced a Source Transcript before the
            // Provider Deadline is abandoned below — record its absence so it is
            // visible in history rather than silently missing.
            if !deepgram_done {
                provider_failures.push(ProviderFailure::new(
                    Provider::Deepgram,
                    ProviderFailureStage::ProviderDeadline,
                    "Provider Deadline elapsed before completion",
                ));
            }
            if !groq_done {
                provider_failures.push(ProviderFailure::new(
                    Provider::Groq,
                    ProviderFailureStage::ProviderDeadline,
                    "Provider Deadline elapsed before completion",
                ));
            }
            let abort_pending = async move {
                let deepgram_abort = async move {
                    if deepgram_done {
                        Ok(())
                    } else {
                        deepgram.abort().await
                    }
                };
                let groq_abort = async move {
                    if groq_done {
                        Ok(())
                    } else {
                        groq.abort().await
                    }
                };
                let (deepgram_result, groq_result) = tokio::join!(deepgram_abort, groq_abort);
                deepgram_result?;
                groq_result
            };
            cleanup_error = match tokio::time::timeout(self.abort_deadline, abort_pending).await {
                Ok(inner) => inner.err(),
                Err(_) => Some(BoundaryError::new(
                    BoundaryKind::Provider,
                    "provider deadline cleanup timed out",
                )),
            };
        }

        transcripts.sort_by_key(|source| source.provider);
        timings_ms.sort_by_key(|timing| timing.provider);
        provider_failures.sort_by_key(|failure| failure.provider);
        if transcripts.is_empty() {
            // No provider produced a Source Transcript. A cleanup failure keeps
            // its exact message here; otherwise build the detail from the
            // collected failures. Either way, carry every failure into the error
            // so history shows each provider's absence, not a bare error.
            let error = match cleanup_error {
                Some(error) => error,
                None => {
                    let detail = if provider_failures.is_empty() {
                        "Provider Deadline elapsed".to_owned()
                    } else {
                        provider_failures
                            .iter()
                            .map(|failure| failure.diagnostic.clone())
                            .collect::<Vec<_>>()
                            .join("; ")
                    };
                    BoundaryError::new(BoundaryKind::Provider, detail)
                }
            };
            Err(error.with_provider_failures(provider_failures))
        } else {
            // A winner survived. If aborting the loser failed, annotate the
            // loser's deadline entry so the cleanup failure stays visible — but
            // NEVER discard the winner's Source Transcript for it.
            if let Some(cleanup_error) = cleanup_error {
                for failure in provider_failures
                    .iter_mut()
                    .filter(|failure| failure.stage == ProviderFailureStage::ProviderDeadline)
                {
                    failure.diagnostic = format!(
                        "{}; cleanup failed: {}",
                        failure.diagnostic,
                        cleanup_error.diagnostic()
                    );
                }
            }
            Ok(ProviderCompletion {
                sources: transcripts,
                timings_ms,
                provider_failures,
            })
        }
    }
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

pub trait TranscriptValidator: Send {
    fn validate(
        &mut self,
        sources: Vec<SourceTranscript>,
    ) -> BoundaryFuture<'_, TranscriptDecision>;
}

impl<M: ReconciliationModel> TranscriptValidator for TranscriptDecisionPipeline<M> {
    fn validate(
        &mut self,
        sources: Vec<SourceTranscript>,
    ) -> BoundaryFuture<'_, TranscriptDecision> {
        Box::pin(async move { self.decide(sources).await })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryMethod {
    /// The compositor processed Voisu's libei frame. This intentionally does
    /// not claim that the focused application accepted or inserted the text;
    /// libei exposes no application-level acknowledgement.
    CompositorSubmitted,
    ClipboardFallback,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliveryOutcome {
    pub method: DeliveryMethod,
    pub fallback_reason: Option<String>,
}

impl DeliveryOutcome {
    pub fn compositor_submitted() -> Self {
        Self {
            method: DeliveryMethod::CompositorSubmitted,
            fallback_reason: None,
        }
    }

    pub fn clipboard_fallback(reason: impl Into<String>) -> Self {
        Self {
            method: DeliveryMethod::ClipboardFallback,
            fallback_reason: Some(reason.into()),
        }
    }
}

pub trait DeliveryAdapter: Send {
    fn deliver(&mut self, transcript: Transcript) -> BoundaryFuture<'_, DeliveryOutcome>;
}

/// The desktop-approved Trigger Key binding, surfaced to the user during setup.
/// `description` is a display string (for example `"Super+Alt+V"`) obtained from
/// the Global Shortcuts portal; it is never a secret and never a device path.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TriggerKeyBinding {
    pub description: String,
}

impl TriggerKeyBinding {
    pub fn new(description: impl Into<String>) -> Self {
        Self {
            description: description.into(),
        }
    }
}

/// Boundary for the desktop Global Shortcuts portal
/// (`org.freedesktop.portal.GlobalShortcuts`). Production binds the Trigger Key
/// through the portal so Voisu never touches raw input devices; tests inject a
/// controlled portal that replays desktop responses. Binding MUST fail closed:
/// an unavailable portal or a denied permission returns a `Shortcut` boundary
/// error rather than a fabricated binding, and the daemon keeps CLI
/// start/stop/toggle usable regardless.
pub trait ShortcutPortal: Send {
    fn bind(&mut self) -> BoundaryFuture<'_, Box<dyn ShortcutSession>>;
}

/// What a live Global Shortcuts session observed next. The distinction matters
/// to the listener: revocation is final, while a portal restart must clear the
/// stale binding and then rebind once the portal returns.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShortcutEvent {
    /// The user pressed the Trigger Key.
    Activated,
    /// The desktop closed the session (permission revoked). Do not rebind.
    Revoked,
    /// The portal vanished from the bus (crash or shutdown). The binding is
    /// stale and must be cleared; the session keeps waiting for a new owner.
    PortalLost,
    /// A (new) portal owns the bus name again. The session is dead; the
    /// listener should drop it and bind a fresh session.
    PortalRestarted,
}

/// A live Global Shortcuts session that yields Trigger Key activations. The
/// session owns whatever portal subscription it created and surrenders it when
/// dropped.
pub trait ShortcutSession: Send {
    /// The desktop-approved binding for display during setup.
    fn binding(&self) -> TriggerKeyBinding;

    /// Awaits the next session event: a Trigger Key activation, a desktop
    /// revocation, or a portal loss/restart transition. A `Shortcut` boundary
    /// error signals a stream failure the listener treats as final retirement.
    fn next_event(&mut self) -> BoundaryFuture<'_, ShortcutEvent>;
}
