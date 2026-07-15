//! Shared domain, provider coordination, and IPC types for Voisu.

use std::env;
use std::future::Future;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

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

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Command {
    Start,
    Stop,
    Toggle,
    Status,
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
    pub stages: Vec<LifecycleStage>,
    pub delivery_count: u32,
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

#[derive(Debug, Deserialize, Serialize)]
pub struct Response {
    pub version: u32,
    pub ok: bool,
    pub state: Option<DaemonState>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<LifecycleEvidence>,
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
        }
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
}

#[derive(Debug)]
pub struct BoundaryError {
    kind: BoundaryKind,
    diagnostic: String,
    transcript_failure: Option<TranscriptFailureEvidence>,
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
        }
    }

    pub fn with_transcript_failure(mut self, evidence: TranscriptFailureEvidence) -> Self {
        self.transcript_failure = Some(evidence);
        self
    }

    pub fn transcript_failure(&self) -> Option<&TranscriptFailureEvidence> {
        self.transcript_failure.as_ref()
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

pub trait ReconciliationModel: Send {
    fn request(
        &mut self,
        kind: ReconciliationKind,
        sources: Vec<SourceTranscript>,
        candidate: Option<MergeResult>,
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

            let merge_result = match tokio::time::timeout(
                self.deadline,
                self.model
                    .request(ReconciliationKind::Reconcile, sources.clone(), None),
            )
            .await
            {
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
                    return clean_source_fallback(
                        &sources,
                        "cloud reconciliation deadline elapsed".to_owned(),
                        true,
                        false,
                    );
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
        let repaired = match tokio::time::timeout(
            self.deadline,
            self.model.request(
                ReconciliationKind::Repair,
                sources.to_vec(),
                Some(candidate),
            ),
        )
        .await
        {
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
                return clean_source_fallback(
                    sources,
                    "recovery deadline elapsed".to_owned(),
                    reconciliation_requested,
                    true,
                );
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
    let source = sources
        .iter()
        .filter(|source| quality_failure_reason(&source.text, std::slice::from_ref(*source)).is_none())
        .max_by_key(|source| source.provider)
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
    if script_count(trimmed) >= 3 {
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

fn script_count(text: &str) -> usize {
    let mut scripts = [false; 7];
    for character in text.chars().filter(|character| character.is_alphabetic()) {
        let code = character as u32;
        let index = match code {
            0x0041..=0x024f => 0, // Latin and Latin extensions
            0x0370..=0x03ff => 1, // Greek
            0x0400..=0x052f => 2, // Cyrillic
            0x0600..=0x06ff => 3, // Arabic
            0x0900..=0x097f => 4, // Devanagari
            0x3040..=0x30ff | 0x3400..=0x9fff => 5, // Japanese/CJK
            _ => 6,
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

pub struct ProviderCompletion {
    pub sources: Vec<SourceTranscript>,
    pub timings_ms: Vec<ProviderTiming>,
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
        deepgram?;
        groq
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
        let mut diagnostics = Vec::new();

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
                            Err(error) => diagnostics.push(error.diagnostic().to_owned()),
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
                            Err(error) => diagnostics.push(error.diagnostic().to_owned()),
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
            tokio::time::timeout(self.abort_deadline, abort_pending)
                .await
                .map_err(|_| {
                    BoundaryError::new(
                        BoundaryKind::Provider,
                        "provider deadline cleanup timed out",
                    )
                })??;
        }

        transcripts.sort_by_key(|source| source.provider);
        timings_ms.sort_by_key(|timing| timing.provider);
        if transcripts.is_empty() {
            let detail = if diagnostics.is_empty() {
                "Provider Deadline elapsed".to_owned()
            } else {
                diagnostics.join("; ")
            };
            Err(BoundaryError::new(BoundaryKind::Provider, detail))
        } else {
            Ok(ProviderCompletion {
                sources: transcripts,
                timings_ms,
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

pub trait DeliveryAdapter: Send {
    fn deliver(&mut self, transcript: Transcript) -> BoundaryFuture<'_, ()>;
}
