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
}

impl BoundaryError {
    pub fn new(kind: BoundaryKind, diagnostic: impl Into<String>) -> Self {
        Self {
            kind,
            diagnostic: diagnostic.into(),
        }
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
    fn complete(self: Box<Self>, audio: CapturedAudio)
    -> BoundaryFuture<'static, SourceTranscript>;
}

pub struct ProviderStreams {
    pub deepgram: Box<dyn ProviderStream>,
    pub groq: Box<dyn ProviderStream>,
}

pub struct ProviderCoordinator {
    deadline: Duration,
    streams: ProviderStreams,
}

pub struct ProviderCompletion {
    pub sources: Vec<SourceTranscript>,
    pub timings_ms: Vec<ProviderTiming>,
}

impl ProviderCoordinator {
    pub fn start(deadline: Duration, streams: ProviderStreams) -> Self {
        Self { deadline, streams }
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
        let deepgram = self.streams.deepgram.complete(audio.clone());
        let groq = self.streams.groq.complete(audio);
        tokio::pin!(deepgram, groq);
        let deadline = tokio::time::sleep(self.deadline);
        tokio::pin!(deadline);
        let mut deepgram_done = false;
        let mut groq_done = false;
        let mut transcripts = Vec::new();
        let mut timings_ms = Vec::new();
        let mut diagnostics = Vec::new();

        while !deepgram_done || !groq_done {
            tokio::select! {
                // Bias toward provider results: if a valid Source Transcript is
                // ready in the same poll as the Provider Deadline, honor the
                // transcript instead of discarding it at the deadline instant.
                biased;
                result = &mut deepgram, if !deepgram_done => {
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
                result = &mut groq, if !groq_done => {
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
                _ = &mut deadline => break,
            }
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
    ) -> Result<Transcript, BoundaryError>;
}

pub trait DeliveryAdapter: Send {
    fn deliver(&mut self, transcript: Transcript) -> BoundaryFuture<'_, ()>;
}
