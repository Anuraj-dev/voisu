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

mod session;
pub use session::{
    clipboard_candidates, install_instruction, resolve_session, ClipboardTool, PackageManager,
    SessionKind, SessionResolution, PACKAGE_MANAGERS,
};

mod wav;
pub use wav::{scan_wav_pcm, WavScan};

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
    /// Observes audio levels newer than the caller's stateless sequence cursor.
    /// The daemon answers this directly without entering the lifecycle actor.
    Level { after_seq: u64 },
    /// Returns the desktop-approved Trigger Key binding for display, or a
    /// notice that no Trigger Key is bound. Never blocks CLI start/stop/toggle.
    Shortcut,
    /// Returns the retained local diagnostic history (newest first).
    History,
    /// Returns a redacted, self-contained diagnostic export for one correlation ID.
    Export(ExportCorrelationId),
    /// Replays a fixed captured fixture at the given path through the provider
    /// and validation boundaries without capturing audio again.
    Replay(ReplayFixturePath),
}

/// Correlation ID accepted by the diagnostic-export command.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ExportCorrelationId(String);

impl ExportCorrelationId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Fixture path accepted by the replay command.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ReplayFixturePath(String);

impl ReplayFixturePath {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn into_inner(self) -> String {
        self.0
    }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated_by: Option<CaptureLimit>,
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
    /// capture failure, a shutdown mid-start, or a processing panic that lost the
    /// task-local provider outcome. It never failed on its own, but it produced no
    /// Source Transcript, so its absence is recorded, not silent.
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level_frames: Option<Vec<LevelFrame>>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LevelFrame {
    pub seq: u64,
    pub bands: [u8; 20],
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
            level_frames: None,
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

    pub fn with_level_frames(level_frames: Vec<LevelFrame>) -> Self {
        let mut response = Self::with_evidence(true, None, "audio levels", None);
        response.level_frames = Some(level_frames);
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
    public_message: Option<&'static str>,
    permanent: bool,
    transcript_failure: Option<Box<TranscriptFailureEvidence>>,
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
            public_message: None,
            permanent: false,
            transcript_failure: None,
            provider_failures: Vec::new(),
        }
    }

    /// Marks a failure that retrying cannot resolve — e.g. the desktop or user
    /// refused the Trigger Key. Callers that would otherwise retry a boundary
    /// use this to stop instead of reattempting a decision already made.
    pub fn permanent(mut self) -> Self {
        self.permanent = true;
        self
    }

    /// Whether retrying this failure is futile because the outcome will not
    /// change (a deliberate refusal), as opposed to a transient unavailability.
    pub fn is_permanent(&self) -> bool {
        self.permanent
    }

    pub fn with_public_message(mut self, message: &'static str) -> Self {
        self.public_message = Some(message);
        self
    }

    pub fn with_transcript_failure(mut self, evidence: TranscriptFailureEvidence) -> Self {
        self.transcript_failure = Some(Box::new(evidence));
        self
    }

    pub fn transcript_failure(&self) -> Option<&TranscriptFailureEvidence> {
        self.transcript_failure.as_deref()
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
        self.public_message.unwrap_or(match self.kind {
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
        })
    }

    pub fn diagnostic(&self) -> &str {
        &self.diagnostic
    }
}

pub type BoundaryFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, BoundaryError>> + Send + 'a>>;

#[derive(Clone, Debug)]
pub struct AudioChunk(pub Vec<u8>);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureLimit {
    Buffer,
    RecordingDeadline,
}

#[derive(Clone, Debug)]
pub struct CapturedAudio {
    pcm_s16le_mono_16khz: Vec<u8>,
    truncated_by: Option<CaptureLimit>,
}

impl CapturedAudio {
    pub fn new(pcm_s16le_mono_16khz: Vec<u8>) -> Self {
        Self {
            pcm_s16le_mono_16khz,
            truncated_by: None,
        }
    }

    pub fn truncated(pcm_s16le_mono_16khz: Vec<u8>, limit: CaptureLimit) -> Self {
        Self {
            pcm_s16le_mono_16khz,
            truncated_by: Some(limit),
        }
    }

    pub fn with_truncation(mut self, limit: CaptureLimit) -> Self {
        self.truncated_by = Some(limit);
        self
    }

    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    pub fn pcm_s16le_mono_16khz(&self) -> &[u8] {
        &self.pcm_s16le_mono_16khz
    }

    pub fn truncated_by(&self) -> Option<CaptureLimit> {
        self.truncated_by
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
    /// A check that does not apply and was deliberately not run (e.g. a disabled
    /// provider's key). Never a failure.
    Skip,
}

impl ReadinessStatus {
    pub fn cli_label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
            Self::Skip => "SKIP",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadinessCapability {
    Session,
    PipeWire,
    Microphone,
    Portals,
    Clipboard,
    SecretStorage,
    Daemon,
    /// Whether the systemd --user manager carries this session's display
    /// variables, without which Delivery from the daemon cannot reach the
    /// server.
    ServiceEnvironment,
}

impl ReadinessCapability {
    pub fn cli_label(self) -> &'static str {
        match self {
            Self::Session => "Session",
            Self::PipeWire => "PipeWire",
            Self::Microphone => "Microphone",
            Self::Portals => "Portals",
            Self::Clipboard => "Clipboard",
            Self::SecretStorage => "Secret storage",
            Self::Daemon => "Daemon",
            Self::ServiceEnvironment => "Service env",
        }
    }
}

/// A single doctor check. Doctor prints one terse line per finding
/// (`label  value  STATUS`); `action` is a runnable remediation shown on its own
/// indented line when present (typically on FAIL), and `detail` is the reasoning
/// shown only under `--verbose`.
pub struct ReadinessFinding {
    pub capability: ReadinessCapability,
    pub status: ReadinessStatus,
    pub detail: String,
    /// The terse value column (e.g. `X11 (Cinnamon)`, `1.0.5`). `None` prints an
    /// empty column.
    pub value: Option<String>,
    /// A runnable remediation, shown indented under the check line. `None` for a
    /// check that needs no action.
    pub action: Option<String>,
}

impl ReadinessFinding {
    pub fn new(
        capability: ReadinessCapability,
        status: ReadinessStatus,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            capability,
            status,
            detail: detail.into(),
            value: None,
            action: None,
        }
    }

    #[must_use]
    pub fn with_value(mut self, value: impl Into<String>) -> Self {
        self.value = Some(value.into());
        self
    }

    #[must_use]
    pub fn with_action(mut self, action: impl Into<String>) -> Self {
        self.action = Some(action.into());
        self
    }
}

/// Boundary for Fedora desktop capability checks. Production uses thin command
/// probes; tests inject controlled outcomes without a desktop session.
pub trait ReadinessInspector: Send {
    fn inspect(&mut self) -> Vec<ReadinessFinding>;
}

/// Where a provider's key currently lives, so keep/replace and migration
/// prompts can be an informed choice rather than a blind one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyLocation {
    /// Stored in the desktop keyring (Secret Service) — the secure home.
    Keyring,
    /// Only in the 0600 plaintext fallback file (saved while the keyring was
    /// unavailable); a candidate for migration back into the keyring.
    PlaintextFile,
    /// Supplied by an environment variable, which wins at runtime over anything
    /// stored.
    EnvOverride,
}

/// A diagnosis of a provider's key: where it lives (with the credential), or why
/// it could not be read. `Absent` is a definitive "nothing stored"; `Locked`,
/// `Unavailable`, and `ToolMissing` mean the keyring could not be consulted, so
/// callers must steer the user to fix the keyring rather than assume no key.
pub enum KeyDiagnosis {
    Found {
        location: KeyLocation,
        credential: Credential,
    },
    /// The provider's environment override variable is set but its value
    /// cannot form a credential (it is empty or contains a line break). The
    /// override still wins at runtime, so a stored key is shadowed by a broken
    /// one — callers must steer the user to unset or fix the variable, never
    /// report the stored key as the effective one.
    EnvOverrideInvalid,
    /// The keyring is reachable and holds no key, and no fallback file exists.
    Absent,
    /// The keyring is locked or refused access.
    Locked,
    /// No desktop Secret Service is running or activatable.
    Unavailable,
    /// The `secret-tool` helper binary is not installed.
    ToolMissing,
}

impl KeyDiagnosis {
    /// The location of a found key, if any.
    pub fn location(&self) -> Option<KeyLocation> {
        match self {
            Self::Found { location, .. } => Some(*location),
            _ => None,
        }
    }

    /// Whether a usable key was found (anywhere).
    pub fn is_configured(&self) -> bool {
        matches!(self, Self::Found { .. })
    }
}

/// Boundary for desktop Secret Service. Implementations must never persist a
/// credential outside the desktop secret service except through an explicit,
/// loudly-announced fallback.
pub trait SecretStore: Send {
    fn replace(&mut self, provider: Provider, credential: Credential) -> Result<(), BoundaryError>;
    fn load(&mut self, provider: Provider) -> Result<Credential, BoundaryError>;

    /// Diagnoses where a provider's key lives (or why it cannot be read), for
    /// informed keep/replace and migration prompts. The default cannot tell one
    /// failure from another, so it reports `Keyring`/`Absent` only.
    fn diagnose(&mut self, provider: Provider) -> KeyDiagnosis {
        match self.load(provider) {
            Ok(credential) => KeyDiagnosis::Found {
                location: KeyLocation::Keyring,
                credential,
            },
            Err(_) => KeyDiagnosis::Absent,
        }
    }
}

/// Boundary for an independent, post-storage provider-auth check. It returns
/// no provider response content, only an authorization result.
pub trait ProviderAuthenticator: Send {
    fn verify(&mut self, provider: Provider, credential: Credential) -> BoundaryFuture<'_, ()>;
}

/// The classified outcome of a live per-provider credential round trip.
///
/// Classifying once, here, lets every surface — the setup wizard, `voisu
/// doctor`, `voisu auth verify`, and daemon logs — report the same actionable
/// meaning instead of a bare HTTP status. A wrong key is the only definitively
/// bad state; throttling, quota, and unreachability are transient and are not
/// the key's fault.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderKeyStatus {
    /// The credential authenticated (2xx).
    Valid,
    /// The provider rejected the credential (401/403): the key is wrong.
    InvalidKey,
    /// The provider is throttling this key right now (429 with `Retry-After`).
    /// The key is fine, so this is transient.
    RateLimited,
    /// The key's allowance is spent (bare 429, no `Retry-After`).
    QuotaExhausted,
    /// The provider could not be reached or returned another status (network
    /// failure, 5xx, timeout). Transient and not the key's fault.
    Unreachable,
}

impl ProviderKeyStatus {
    /// Classifies an HTTP status and whether a `Retry-After` header accompanied
    /// it. Pure so the mapping is unit-tested without any network.
    pub fn classify(http_status: u16, retry_after_present: bool) -> Self {
        match http_status {
            200..=299 => Self::Valid,
            401 | 403 => Self::InvalidKey,
            429 if retry_after_present => Self::RateLimited,
            429 => Self::QuotaExhausted,
            _ => Self::Unreachable,
        }
    }

    /// Whether the credential authenticated.
    pub fn is_valid(self) -> bool {
        matches!(self, Self::Valid)
    }

    /// Whether this is a transient condition (the key may be fine) rather than a
    /// definitively wrong key.
    pub fn is_transient(self) -> bool {
        matches!(
            self,
            Self::RateLimited | Self::QuotaExhausted | Self::Unreachable
        )
    }

    /// The readiness severity a `voisu doctor` line should carry: only a wrong
    /// key is a hard failure; throttling, quota, and unreachability are warnings.
    pub fn readiness(self) -> ReadinessStatus {
        match self {
            Self::Valid => ReadinessStatus::Pass,
            Self::InvalidKey => ReadinessStatus::Fail,
            Self::RateLimited | Self::QuotaExhausted | Self::Unreachable => ReadinessStatus::Warn,
        }
    }

    /// A one-line, user-facing explanation. `InvalidKey` always names the fix so
    /// no surface leaves the user guessing.
    pub fn headline(self) -> &'static str {
        match self {
            Self::Valid => "key valid",
            Self::InvalidKey => "key invalid — run `voisu setup`",
            Self::RateLimited => "rate-limited (transient — try again shortly)",
            Self::QuotaExhausted => "free-tier quota exhausted",
            Self::Unreachable => "provider unreachable (transient)",
        }
    }
}

/// Free-tier guidance shown when a key is missing, invalid, or over quota, so a
/// friend knows the provider's free allowance covers daily dictation. Figures
/// are from the distribution research digest (§5/§9).
pub fn provider_free_tier_hint(provider: Provider) -> &'static str {
    match provider {
        Provider::Deepgram => {
            "Deepgram grants $200 of free credit with no card (about a year at 1–2 h/day); create a key at https://console.deepgram.com"
        }
        Provider::Groq => {
            "Groq's free tier covers ~2000 requests and 28,800 audio-seconds per day — ample for daily dictation; create a key at https://console.groq.com/keys"
        }
    }
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
    dictionary_terms: Vec<String>,
}

impl<M: ReconciliationModel> TranscriptDecisionPipeline<M> {
    pub fn new(model: M, deadline: Duration) -> Self {
        Self {
            model,
            deadline,
            dictionary_terms: Vec::new(),
        }
    }

    pub fn with_dictionary_terms(
        model: M,
        deadline: Duration,
        dictionary_terms: Vec<String>,
    ) -> Self {
        Self {
            model,
            deadline,
            dictionary_terms,
        }
    }

    pub fn set_dictionary_terms(&mut self, dictionary_terms: Vec<String>) {
        self.dictionary_terms = dictionary_terms;
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
                let lexically_identical =
                    normalized_words(&deepgram.text) == normalized_words(&groq.text);
                // Word-for-word equal texts are decided on all three formatting
                // signals; texts that differ in words are decided by
                // `lexically_different_selection`, which lets evidence overturn
                // the Groq default only when the whole difference is a single
                // misheard span, so no formatting win can change the words the
                // user is handed.
                let (winner, evidence) = if lexically_identical {
                    near_identical_selection(&deepgram.text, &groq.text, &self.dictionary_terms)
                } else {
                    lexically_different_selection(
                        &deepgram.text,
                        &groq.text,
                        &self.dictionary_terms,
                    )
                };
                let selected = match winner {
                    GateWinner::Left => deepgram,
                    GateWinner::Right => groq,
                };
                if let Some(reason) =
                    non_contraction_quality_failure_reason(&selected.text, &sources)
                {
                    return self
                        .repair_candidate(
                            &sources,
                            MergeResult(selected.text.trim().to_owned()),
                            reason,
                            false,
                        )
                        .await;
                }
                return Ok(TranscriptDecision {
                    transcript: Transcript(selected.text.trim().to_owned()),
                    selection: match selected.provider {
                        Provider::Deepgram => TranscriptSelection::SourceDeepgram,
                        Provider::Groq => TranscriptSelection::NearIdenticalGroq,
                    },
                    validation_reason: format!(
                        "near-identical Source Transcripts passed validation; {evidence}"
                    ),
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
                if non_contraction_quality_failure_reason(&winner.text, &sources).is_none() {
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
            if let Some(failure) = quality_failure_reason(&merge_result.0, &sources) {
                let reason = failure.reason();
                if failure.is_contraction() {
                    return contraction_source_fallback(&sources, reason, true, false);
                }
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
        if let Some(reason) = non_contraction_quality_failure_reason(&source.text, &sources) {
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
        reason: String,
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
        let failure = quality_failure_reason(&repaired.0, sources);
        if let Some(failure) = &failure {
            if !failure.is_contraction() {
                return clean_source_fallback(
                    sources,
                    format!("recovery produced {}", failure.reason()),
                    reconciliation_requested,
                    true,
                );
            }
        }
        if !is_source_derived(&repaired.0, sources) {
            return clean_source_fallback(
                sources,
                "recovery produced words no Source Transcript contains".to_owned(),
                reconciliation_requested,
                true,
            );
        }
        if let Some(failure) = failure {
            // The repair contracted past the merge floor. It is built out of
            // words the providers heard and is otherwise clean, so it is still
            // the user's speech — but a complete Source Transcript carries more
            // of it, and preferring one is exactly what the floor is for. The
            // fallback follows the spec's contraction rule: the LONGER Source
            // Transcript, the user never receiving less than one provider
            // heard.
            //
            // The floor decides PREFERENCE, not delivery. When the offending
            // text was in both Source Transcripts, neither is safe and this
            // repair is all that is left of the Recording; refusing it there
            // was the round-1 P0. So a failure to find a clean source hands the
            // repair over rather than losing the dictation — with the measured
            // contraction on the diagnostic record, because a silently
            // delivered précis is the case an operator tuning the floor most
            // needs to see.
            if let Ok(decision) = contraction_source_fallback(
                sources,
                format!("recovery produced {}", failure.reason()),
                reconciliation_requested,
                true,
            ) {
                return Ok(decision);
            }
            return Ok(TranscriptDecision {
                transcript: Transcript(repaired.0.trim().to_owned()),
                selection: TranscriptSelection::Repaired,
                validation_reason: format!(
                    "repaired {reason}; delivered a contracted repair because neither Source Transcript is clean"
                ),
                fallback_reason: Some(failure.reason()),
                reconciliation_requested,
                recovery_attempted: true,
            });
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

/// Delivers a Source Transcript after a merge or repair was rejected for
/// contracting.
///
/// The rejected candidate is deliberately NOT consulted. The guard has just
/// declared it untrustworthy for having deleted words, so using it to arbitrate
/// which source to deliver is circular — and when the merge simply reproduced
/// the shorter source (a provider that truncated its tail), the "corroboration"
/// is guaranteed and the guard hands over exactly the contraction it rejected.
///
/// So the LONGER Source Transcript wins by default: the user must never receive
/// less than one provider heard. The single exception is padding — a source
/// that is longer only because it repeats what it already said heard no more
/// than its sibling, and losing that surplus loses nothing.
fn contraction_source_fallback(
    sources: &[SourceTranscript],
    reason: String,
    reconciliation_requested: bool,
    recovery_attempted: bool,
) -> Result<TranscriptDecision, BoundaryError> {
    let safe = quality_safe_sources(sources);
    let source = match safe.as_slice() {
        [] => None,
        [only] => Some(*only),
        [left, right, ..] => {
            let left_words = normalized_words(&left.text);
            let right_words = normalized_words(&right.text);
            let left_padded = surplus_is_self_repetition(&left_words, &right_words);
            let right_padded = surplus_is_self_repetition(&right_words, &left_words);
            Some(match (left_padded, right_padded) {
                (true, false) => *right,
                (false, true) => *left,
                // Neither side is padding (or both are): deliver the longer
                // text. An exact tie keeps Groq, the standing default.
                _ => {
                    if left_words.len() > right_words.len() {
                        *left
                    } else {
                        *right
                    }
                }
            })
        }
    };
    let source = source.ok_or_else(|| {
        source_fallback_refusal(
            &reason,
            reconciliation_requested,
            recovery_attempted,
        )
    })?;
    Ok(TranscriptDecision {
        transcript: Transcript(source.text.trim().to_owned()),
        selection: match source.provider {
            Provider::Deepgram => TranscriptSelection::SourceDeepgram,
            Provider::Groq => TranscriptSelection::SourceGroq,
        },
        validation_reason:
            "longer Source Transcript delivered after a rejected contraction".to_owned(),
        fallback_reason: Some(reason),
        reconciliation_requested,
        recovery_attempted,
    })
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
    let safe = quality_safe_sources(sources);
    let source = match safe.as_slice() {
        [] => None,
        [only] => Some(*only),
        [left, right, ..] => Some(
            match select_better_source(
                &normalized_words(&left.text),
                &normalized_words(&right.text),
            )
            .0
            {
                GateWinner::Left => *left,
                GateWinner::Right => *right,
            },
        ),
    };
    let source = source.ok_or_else(|| {
        source_fallback_refusal(
            &reason,
            reconciliation_requested,
            recovery_attempted,
        )
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

fn quality_safe_sources<'a>(
    sources: impl IntoIterator<Item = &'a SourceTranscript>,
) -> Vec<&'a SourceTranscript> {
    sources
        .into_iter()
        .filter(|source| {
            non_contraction_quality_failure_reason(&source.text, std::slice::from_ref(source))
                .is_none()
        })
        .collect()
}

fn source_fallback_refusal(
    reason: &str,
    reconciliation_requested: bool,
    recovery_attempted: bool,
) -> BoundaryError {
    let validation_reason = format!("{reason}; neither Source Transcript is safe");
    BoundaryError::new(BoundaryKind::Validation, validation_reason.clone())
        .with_transcript_failure(TranscriptFailureEvidence {
            validation_reason,
            fallback_reason: Some(reason.to_owned()),
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

/// A transcript whose content words are revisited so relentlessly that fewer
/// than this fraction of content-word occurrences are distinct is a repetition
/// loop, not dictation. Legitimate repetitive technical dictation sits well
/// above it (the "cache … cache invalidation … cache" fixture is ~0.64); a
/// loop cycling a couple of words through filler collapses far below.
const CONTENT_REPETITION_FLOOR: f64 = 0.4;

/// The repetition-loop check needs at least this many content-word occurrences
/// before a low distinct ratio means anything — a short utterance repeating one
/// term ("test test test done") is terse, not degenerate.
const MIN_REPETITION_CONTENT: usize = 8;

/// A repetition loop needs at least this many distinct content words before
/// cross-source evidence can judge it. Below it ("start stop reset" three
/// times) the vocabulary is too small to distinguish a loop from genuinely
/// repeated command speech, so those shapes go to reconciliation instead.
const MIN_LOOP_VOCABULARY: usize = 4;

/// True when a Source Transcript is internally degenerate — a filler or
/// repetition loop with almost no distinct content (context-free 1 s slices, or
/// a "the/and/to/is" loop). This is a ROBUST garbage signal: it triggers on
/// near-absent content, NOT on mere repetition, so legitimate jargon-heavy,
/// naturally repetitive, or short-command dictation ("start stop reset" three
/// times) is never flagged. Repetition loops that DO carry content words are
/// judged with cross-source evidence in `is_garbage_against` instead, because
/// a single-source view cannot tell them from genuine repeated speech.
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

/// True when a Source Transcript has the SHAPE of a repetition loop: a
/// judgeable content vocabulary recycled so relentlessly that the distinct
/// share of content occurrences collapses. Shape alone is not a verdict —
/// genuine command dictation can repeat itself — so the loop is condemned only
/// by cross-source evidence in `is_garbage_against`.
fn is_repetition_loop(words: &[String]) -> bool {
    let content_count = words.iter().filter(|word| !is_stopword(word)).count();
    let distinct_content = distinct_content_words(words).len();
    content_count >= MIN_REPETITION_CONTENT
        && distinct_content >= MIN_LOOP_VOCABULARY
        && (distinct_content as f64) < CONTENT_REPETITION_FLOOR * content_count as f64
}

/// True when everything `candidate` says beyond `other` is `candidate` saying
/// again what it already said — padding, not speech the other side missed.
///
/// The surplus is the multiset of candidate words `other` cannot account for,
/// so it is order-insensitive: a reordered but equally complete sibling leaves
/// no surplus. The surplus counts as padding only when ALL of these hold:
/// - it carries at least `MIN_REPETITION_CONTENT` content words, below which
///   the shape says nothing (and the few words at stake are worth keeping);
/// - it introduces no content word the candidate did not already use in its
///   corroborated part — one genuinely new word means real speech was heard;
/// - it is itself a repetition loop by the same `CONTENT_REPETITION_FLOOR` the
///   garbage gate uses.
///
/// The conjunction is deliberately strict, because the cost of a false positive
/// is delivering less than a provider heard. A tail one provider caught and the
/// other truncated brings new content words, or is not internally repetitive,
/// and so survives every clause.
fn surplus_is_self_repetition(candidate: &[String], other: &[String]) -> bool {
    let mut budget: HashMap<&str, usize> = HashMap::new();
    for word in other {
        *budget.entry(word.as_str()).or_default() += 1;
    }
    let mut surplus: Vec<&str> = Vec::new();
    for word in candidate {
        match budget.get_mut(word.as_str()) {
            Some(remaining) if *remaining > 0 => *remaining -= 1,
            _ => surplus.push(word.as_str()),
        }
    }
    let content: Vec<&str> = surplus
        .into_iter()
        .filter(|word| !is_stopword(word))
        .collect();
    if content.len() < MIN_REPETITION_CONTENT {
        return false;
    }
    let distinct: HashSet<&str> = content.iter().copied().collect();
    // A word `other` never used cannot have been corroborated earlier in the
    // candidate either, because every corroborated occurrence was matched
    // against `other`'s budget. So an unknown word here is new content.
    if distinct.iter().any(|word| !budget.contains_key(word)) {
        return false;
    }
    (distinct.len() as f64) < CONTENT_REPETITION_FLOOR * content.len() as f64
}

/// The single garbage verdict for one side of a disagreeing pair, judged by
/// intrinsic degeneracy plus cross-source evidence (`confirmed` is the set of
/// this side's distinct content words that the one-to-one phonetic matching
/// paired with the other side). A transcript is garbage when it is a
/// near-content-free filler loop, or a repetition loop that either
/// - steals its material: the majority of its RECYCLED (multi-occurrence)
///   content words are confirmed by the other source, so the loop is an echo
///   of the other transcript's content rather than independent speech; or
/// - is hollow: it has a comparable vocabulary yet the other source confirms
///   less than the agreement floor of it, so its relentless repetition is
///   self-generated noise around at most an accidental shared word or two.
///   The floor is the SAME `CONTENT_OVERLAP_FLOOR` the agreement gate uses, so
///   no confirmed-fraction band opens between "hollow" and "agrees enough to
///   reconcile" for a loop whose vocabulary is the smaller of the pair.
///
/// Genuine repeated speech survives both: its recycled commands are its own
/// (no theft majority), and any real overlap with the other source keeps it
/// from being hollow. A loop with meaningful overlap but no recycled-word
/// theft is ambiguous and deliberately NOT garbage — that shape reconciles.
fn is_garbage_against(own: &[String], confirmed: &HashSet<&str>) -> bool {
    if is_degenerate(own) {
        return true;
    }
    if !is_repetition_loop(own) {
        return false;
    }
    let mut occurrences: HashMap<&str, usize> = HashMap::new();
    for word in own.iter().filter(|word| !is_stopword(word)) {
        *occurrences.entry(word.as_str()).or_default() += 1;
    }
    let recycled: Vec<&str> = occurrences
        .iter()
        .filter(|(_, count)| **count >= 2)
        .map(|(word, _)| *word)
        .collect();
    let confirmed_recycled = recycled
        .iter()
        .filter(|word| confirmed.contains(**word))
        .count();
    let stolen = confirmed_recycled * 2 > recycled.len();
    let distinct_content = distinct_content_words(own).len();
    let hollow = distinct_content >= MIN_COMPARABLE_CONTENT
        && (confirmed.len() as f64) < CONTENT_OVERLAP_FLOOR * distinct_content as f64;
    stolen || hollow
}

/// Two Source Transcripts of the same audio must agree on a meaningful share of
/// content words. Below this containment (one-to-one matched distinct content
/// words over the smaller distinct-content set) they cannot both be
/// transcriptions of the same speech: one of them is garbage, and merging
/// would poison the result.
const CONTENT_OVERLAP_FLOOR: f64 = 0.2;

/// Sources with fewer distinct content words than this are too short for the
/// cross-agreement gate to judge — two terse commands ("book the room" vs
/// "schedule the review") can honestly share nothing, so they reconcile.
const MIN_COMPARABLE_CONTENT: usize = 5;

/// Cross-confirmation differences below this margin are noise, not a decision.
const CONFIRMATION_MARGIN: f64 = 0.15;

/// Which evidence tier decided a selection between two disagreeing sources.
/// The first two tiers rest on cross-source evidence a salad cannot fake; the
/// last three are heuristic guesses over intrinsic structure and are surfaced
/// as low-confidence in the §3.5 diagnostic reason.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SelectionEvidence {
    /// Exactly one side is garbage under `is_garbage_against`.
    Garbage,
    /// One side's distinct content is confirmed by the other side beyond the
    /// noise margin.
    Confirmation,
    /// Only topical cohesion (revisited topic terms) separated the sides.
    Cohesion,
    /// Only intrinsic content density separated the sides.
    Quality,
    /// No evidence at all: the deterministic default (the later source, Groq)
    /// was applied.
    Default,
}

impl SelectionEvidence {
    /// True when the winner was picked by an intrinsic, gameable signal or by
    /// the bare deterministic default rather than by cross-source evidence.
    fn is_low_confidence(self) -> bool {
        matches!(self, Self::Cohesion | Self::Quality | Self::Default)
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

/// The single cross-source agreement computation: a one-to-one matching
/// between the two distinct content vocabularies, where a pair matches when
/// the words are equal or sound alike (edit-distance homophone tolerance, so
/// "cache"/"cash" and "failed"/"sailed" count as the same heard word).
/// Matching is ONE-TO-ONE — each word on either side is consumable exactly
/// once, so six salad words orbiting the same real word ("bat hat mat rat pat
/// sat" around "cat") claim at most one match between them — and SYMMETRIC BY
/// CONSTRUCTION: candidate pairs are taken greedily in a global order keyed by
/// (edit distance, unordered word pair), which is invariant under swapping the
/// arguments, so the matching never depends on which provider held which text.
fn phonetic_matching<'words>(
    left: &HashSet<&'words str>,
    right: &HashSet<&'words str>,
) -> Vec<(&'words str, &'words str)> {
    let mut candidates: Vec<(usize, &str, &str)> = left
        .iter()
        .flat_map(|left_word| {
            right
                .iter()
                .filter(|right_word| words_sound_alike(left_word, right_word))
                .map(|right_word| {
                    (
                        char_edit_distance(left_word, right_word),
                        *left_word,
                        *right_word,
                    )
                })
        })
        .collect();
    candidates.sort_unstable_by_key(|(distance, left_word, right_word)| {
        (
            *distance,
            *left_word.min(right_word),
            *left_word.max(right_word),
        )
    });
    let mut left_used: HashSet<&str> = HashSet::new();
    let mut right_used: HashSet<&str> = HashSet::new();
    let mut matching = Vec::new();
    for (_, left_word, right_word) in candidates {
        if !left_used.contains(left_word) && !right_used.contains(right_word) {
            left_used.insert(left_word);
            right_used.insert(right_word);
            matching.push((left_word, right_word));
        }
    }
    matching
}

/// Two content words within a third of their length in edit distance are close
/// enough to be alternate spellings or homophones of the same heard word
/// ("failed"/"sailed", "cache"/"cash", "during"/"touring"), while unrelated
/// vocabulary — even topically adjacent nonsense — almost never lands inside
/// the bound. Words of three letters or fewer are one flip away from unrelated
/// vocabulary ("cat"/"bat"), so they must match exactly.
fn words_sound_alike(left: &str, right: &str) -> bool {
    let longer = left.chars().count().max(right.chars().count());
    if longer == 0 {
        return false;
    }
    if longer <= 3 {
        return left == right;
    }
    char_edit_distance(left, right) <= longer.div_ceil(3)
}

fn char_edit_distance(left: &str, right: &str) -> usize {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0; right.len() + 1];
    for (left_index, left_char) in left.iter().enumerate() {
        current[0] = left_index + 1;
        for (right_index, right_char) in right.iter().enumerate() {
            current[right_index + 1] = if left_char == right_char {
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

/// Chooses between two disagreeing Source Transcripts by ordered evidence,
/// never by a fixed provider preference. The strong tiers rest on cross-source
/// evidence: a one-sided garbage verdict (degenerate filler, stolen-word loop,
/// or hollow loop), then asymmetric distinct-word confirmation. When both are
/// silent no heuristic truly knows — topical cohesion, then content density,
/// then the deterministic default (the later source, Groq) still pick a
/// winner, but the returned `SelectionEvidence` marks those tiers as
/// low-confidence so the gate can say so in the §3.5 diagnostic record.
fn select_better_source(left: &[String], right: &[String]) -> (GateWinner, SelectionEvidence) {
    let left_content = distinct_content_words(left);
    let right_content = distinct_content_words(right);
    let matching = phonetic_matching(&left_content, &right_content);
    let left_confirmed: HashSet<&str> = matching.iter().map(|(word, _)| *word).collect();
    let right_confirmed: HashSet<&str> = matching.iter().map(|(_, word)| *word).collect();

    let left_garbage = is_garbage_against(left, &left_confirmed);
    let right_garbage = is_garbage_against(right, &right_confirmed);
    if left_garbage != right_garbage {
        let winner = if left_garbage {
            GateWinner::Right
        } else {
            GateWinner::Left
        };
        return (winner, SelectionEvidence::Garbage);
    }

    // Distinct words, never occurrences: a salad repeating one stolen word
    // gains nothing, because repetition of an already-confirmed word is not
    // additional cross-source agreement.
    let confirmation = |confirmed: &HashSet<&str>, content: &HashSet<&str>| {
        if content.is_empty() {
            0.0
        } else {
            confirmed.len() as f64 / content.len() as f64
        }
    };
    let left_confirmation = confirmation(&left_confirmed, &left_content);
    let right_confirmation = confirmation(&right_confirmed, &right_content);
    if (left_confirmation - right_confirmation).abs() > CONFIRMATION_MARGIN {
        let winner = if left_confirmation > right_confirmation {
            GateWinner::Left
        } else {
            GateWinner::Right
        };
        return (winner, SelectionEvidence::Confirmation);
    }

    let left_cohesion = topical_cohesion(left);
    let right_cohesion = topical_cohesion(right);
    if left_cohesion != right_cohesion {
        let winner = if left_cohesion > right_cohesion {
            GateWinner::Left
        } else {
            GateWinner::Right
        };
        return (winner, SelectionEvidence::Cohesion);
    }

    let left_quality = source_quality(left);
    let right_quality = source_quality(right);
    if left_quality > right_quality {
        (GateWinner::Left, SelectionEvidence::Quality)
    } else if right_quality > left_quality {
        (GateWinner::Right, SelectionEvidence::Quality)
    } else {
        (GateWinner::Right, SelectionEvidence::Default)
    }
}

/// Decides whether to skip the LLM merge for two materially disagreeing Source
/// Transcripts and select the better one (§3.4). One symmetric computation —
/// the one-to-one phonetic-tolerant matching of distinct content words —
/// feeds every decision:
/// 1. Exactly one source is garbage (degenerate filler loop, a repetition loop
///    stealing the majority of its recycled words from the other source, or a
///    hollow loop nothing of which is confirmed): select the other.
/// 2. One source is a bare fragment (extreme length ratio): select the fuller.
/// 3. Two comparable vocabularies agree on almost nothing — two transcriptions
///    of the same audio cannot do that, so one is fluent nonsense or a word
///    salad no intrinsic check can flag: gate, and select by `select_better_source`,
///    recording a low-confidence marker when only weak evidence decided.
///
/// Pairs that clear all three (real disagreements over shared or
/// phonetically-aligned content, terse pairs, homophone spellings) return
/// `None` and go to the reconciliation model.
fn source_quality_gate(left: &str, right: &str) -> Option<QualityGate> {
    let left_words = normalized_words(left);
    let right_words = normalized_words(right);
    let fewer = left_words.len().min(right_words.len());
    let more = left_words.len().max(right_words.len());
    if fewer == 0 {
        return None;
    }
    let left_content = distinct_content_words(&left_words);
    let right_content = distinct_content_words(&right_words);
    let matching = phonetic_matching(&left_content, &right_content);
    let left_confirmed: HashSet<&str> = matching.iter().map(|(word, _)| *word).collect();
    let right_confirmed: HashSet<&str> = matching.iter().map(|(_, word)| *word).collect();
    let left_garbage = is_garbage_against(&left_words, &left_confirmed);
    let right_garbage = is_garbage_against(&right_words, &right_confirmed);

    if left_garbage != right_garbage {
        // Exactly one source is garbage: select the coherent one.
        let winner = if left_garbage {
            GateWinner::Right
        } else {
            GateWinner::Left
        };
        return Some(QualityGate {
            winner,
            reason:
                "catastrophically divergent (one Source Transcript is a degenerate filler/repetition loop without independent cross-source support); selected the coherent Source Transcript"
                    .to_owned(),
        });
    }

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

    // Cross-agreement check: two comparable, individually coherent
    // transcriptions of the SAME audio must still agree — exactly or
    // phonetically — on a meaningful share of content words. Near-zero
    // agreement means one of them is garbage, and an LLM merge would let it
    // poison the result. Short pairs are exempt: terse commands can honestly
    // share nothing. Both-garbage pairs are also exempt — with no coherent
    // side to select, reconciliation is the only honest move.
    if !left_garbage {
        let smaller_content = left_content.len().min(right_content.len());
        if smaller_content >= MIN_COMPARABLE_CONTENT {
            let agreement = matching.len() as f64 / smaller_content as f64;
            if agreement < CONTENT_OVERLAP_FLOOR {
                let (winner, evidence) = select_better_source(&left_words, &right_words);
                let low_confidence = if evidence.is_low_confidence() {
                    "; low-confidence selection: cross-source evidence could not distinguish the Source Transcripts"
                } else {
                    ""
                };
                return Some(QualityGate {
                    winner,
                    reason: format!(
                        "catastrophically divergent (cross-source content agreement {agreement:.2} below {CONTENT_OVERLAP_FLOOR:.2}); selected the Source Transcript better supported by cross-source evidence{low_confidence}"
                    ),
                });
            }
        }
    }

    // Sources agree enough to be transcriptions of the same speech (or are
    // both garbage, or too short to judge): no confident single winner, so
    // reconcile rather than guess.
    None
}

struct FormattingEvidence {
    capitalised_sentence_starts: usize,
    sentence_starts: usize,
    all_caps: bool,
    sentence_punctuation_boundaries: usize,
    dictionary_matches: usize,
}

/// The outcome of comparing two Source Transcripts on formatting evidence.
struct FormattingComparison {
    left_signals: usize,
    right_signals: usize,
    measurements: String,
}

impl FormattingComparison {
    /// True when one side won at least one signal and the other won none.
    fn favours_left(&self) -> bool {
        self.left_signals > 0 && self.right_signals == 0
    }
}

fn near_identical_selection(
    left: &str,
    right: &str,
    dictionary_terms: &[String],
) -> (GateWinner, String) {
    let comparison = compare_formatting(left, right, dictionary_terms);
    let measurements = &comparison.measurements;
    if comparison.favours_left() {
        (
            GateWinner::Left,
            format!("selected Deepgram on one-sided formatting evidence ({measurements})"),
        )
    } else {
        (
            GateWinner::Right,
            format!(
                "defaulted to Groq because formatting evidence was not one-sided ({measurements})"
            ),
        )
    }
}

/// B2 for near-identical Source Transcripts that are NOT word-for-word equal
/// after normalisation — the shape the spec's own motivating case has: Deepgram
/// "Voisu" against Groq "voice so".
///
/// One rule, stated once: formatting and dictionary evidence may overturn the
/// Groq default ONLY when the whole lexical difference is a single misheard
/// span (`single_misheard_span`) — the texts are word-for-word identical
/// outside one contiguous, positionally aligned window where one provider
/// heard a span one way and the other another. Delivering either side then
/// renders every word the user spoke; the evidence merely picks the rendering.
///
/// Everything else keeps the default. A word only one side has ("not", a
/// padded "Okay. Okay.") makes one span empty; a difference spread across two
/// sites (a dropped negation beside a split term) widens the window past a
/// single mishearing; unrelated words in the window fail the sound-alike
/// check. In each of those shapes the sides genuinely differ in CONTENT, and
/// no amount of capitalisation, punctuation, or dictionary spelling may hand
/// the user different words than the standing default would.
fn lexically_different_selection(
    left: &str,
    right: &str,
    dictionary_terms: &[String],
) -> (GateWinner, String) {
    let comparison = compare_formatting(left, right, dictionary_terms);
    let measurements = &comparison.measurements;
    let groq_default = |why: &str| {
        (
            GateWinner::Right,
            format!(
                "lexically different near-identical Source Transcripts; defaulted to Groq {why} ({measurements})"
            ),
        )
    };
    if !comparison.favours_left() {
        return groq_default("because formatting evidence was not one-sided");
    }
    let left_words = normalized_words(left);
    let right_words = normalized_words(right);
    let Some((left_span, right_span)) = single_misheard_span(&left_words, &right_words) else {
        return groq_default("because the Source Transcripts differ by more than one misheard span");
    };
    let length_note = if left_words.len() == right_words.len() {
        "equal length".to_owned()
    } else {
        format!("{} vs {} words", left_words.len(), right_words.len())
    };
    (
        GateWinner::Left,
        format!(
            "lexically different Source Transcripts ({length_note}) whose only difference is the single span \"{}\" heard as \"{}\"; selected Deepgram on one-sided formatting evidence ({measurements})",
            left_span.join(" "),
            right_span.join(" "),
        ),
    )
}

/// The one place two normalised word sequences disagree, when their whole
/// difference has the shape of a single mishearing — otherwise `None`.
///
/// Strip the longest common prefix, then the longest common suffix of what
/// remains. The leftover spans are the entire lexical difference, contiguous
/// and positionally aligned by construction. They qualify only if:
///
/// 1. Both are non-empty. An empty span means one side heard words the other
///    simply lacks — a dropped word or padding — and preferring the short side
///    deletes speech while preferring the long side rewards padding.
/// 2. One span is exactly one word and neither exceeds two. A mishearing of a
///    single heard span is one word for one ("dictation"/"dictations"), one
///    split into two ("voisu"/"voice so"), or two joined into one. A wider
///    window is not one mishearing but different speech — this is what stops
///    an extra real word from riding a genuine split ("pravah cli debug"
///    against "pravah-cli" is a three-word span, rejected before any
///    character arithmetic can be gamed).
/// 3. Run together, the spans sound alike (`words_sound_alike`). Within the
///    shape-capped window this cannot be stretched by adding material — rule 2
///    already forbids it — so the residual it admits is the genuine homophone
///    ("notify"/"not modify"), where either choice risks the meaning and the
///    default is no safer than the evidence.
fn single_misheard_span<'words>(
    left: &'words [String],
    right: &'words [String],
) -> Option<(&'words [String], &'words [String])> {
    let prefix = left
        .iter()
        .zip(right)
        .take_while(|(left_word, right_word)| left_word == right_word)
        .count();
    let suffix = left[prefix..]
        .iter()
        .rev()
        .zip(right[prefix..].iter().rev())
        .take_while(|(left_word, right_word)| left_word == right_word)
        .count();
    let left_span = &left[prefix..left.len() - suffix];
    let right_span = &right[prefix..right.len() - suffix];
    let spans_shaped_like_one_mishearing = left_span.len().min(right_span.len()) == 1
        && left_span.len().max(right_span.len()) <= 2;
    (spans_shaped_like_one_mishearing
        && words_sound_alike(&left_span.concat(), &right_span.concat()))
    .then_some((left_span, right_span))
}

fn compare_formatting(
    left: &str,
    right: &str,
    dictionary_terms: &[String],
) -> FormattingComparison {
    let left_evidence = formatting_evidence(left, dictionary_terms);
    let right_evidence = formatting_evidence(right, dictionary_terms);
    let mut left_signals = 0;
    let mut right_signals = 0;

    let capitalisation_score = |evidence: &FormattingEvidence| {
        if evidence.all_caps || evidence.sentence_starts == 0 {
            0.0
        } else {
            evidence.capitalised_sentence_starts as f64 / evidence.sentence_starts as f64
        }
    };
    let left_capitalisation = capitalisation_score(&left_evidence);
    let right_capitalisation = capitalisation_score(&right_evidence);
    if left_capitalisation > right_capitalisation {
        left_signals += 1;
    } else if right_capitalisation > left_capitalisation {
        right_signals += 1;
    }
    if left_evidence.sentence_punctuation_boundaries
        > right_evidence.sentence_punctuation_boundaries
    {
        left_signals += 1;
    } else if right_evidence.sentence_punctuation_boundaries
        > left_evidence.sentence_punctuation_boundaries
    {
        right_signals += 1;
    }
    if left_evidence.dictionary_matches > right_evidence.dictionary_matches {
        left_signals += 1;
    } else if right_evidence.dictionary_matches > left_evidence.dictionary_matches {
        right_signals += 1;
    }

    let measurements = format!(
        "capitalised sentence starts {}/{}{} vs {}/{}{}, sentence punctuation boundaries {} vs {}, dictionary matches {} vs {}",
        left_evidence.capitalised_sentence_starts,
        left_evidence.sentence_starts,
        if left_evidence.all_caps { " (all-caps)" } else { "" },
        right_evidence.capitalised_sentence_starts,
        right_evidence.sentence_starts,
        if right_evidence.all_caps { " (all-caps)" } else { "" },
        left_evidence.sentence_punctuation_boundaries,
        right_evidence.sentence_punctuation_boundaries,
        left_evidence.dictionary_matches,
        right_evidence.dictionary_matches,
    );
    FormattingComparison {
        left_signals,
        right_signals,
        measurements,
    }
}

fn formatting_evidence(text: &str, dictionary_terms: &[String]) -> FormattingEvidence {
    let (capitalised_sentence_starts, sentence_starts) = sentence_start_capitalisation(text);
    let alphabetic = text.chars().filter(|character| character.is_alphabetic()).count();
    let lowercase = text.chars().filter(|character| character.is_lowercase()).count();
    FormattingEvidence {
        capitalised_sentence_starts,
        sentence_starts,
        all_caps: alphabetic >= 4 && lowercase == 0,
        sentence_punctuation_boundaries: sentence_boundary_credit(text),
        dictionary_matches: distinct_nonoverlapping_dictionary_matches(text, dictionary_terms),
    }
}

fn sentence_punctuation_boundaries(text: &str) -> usize {
    let mut boundaries = 0;
    let mut in_boundary = false;
    for character in text.chars() {
        if matches!(character, '.' | '?' | '!') {
            if !in_boundary {
                boundaries += 1;
                in_boundary = true;
            }
        } else {
            in_boundary = false;
        }
    }
    boundaries
}

fn sentence_boundary_credit(text: &str) -> usize {
    // Past roughly one boundary per six words, more punctuation is more likely
    // over-segmentation than evidence of better sentence structure. Saturate
    // there so a period after every word cannot manufacture a winning signal.
    let plausible_boundaries = normalized_words(text).len().div_ceil(6).max(1);
    sentence_punctuation_boundaries(text).min(plausible_boundaries)
}

/// The text's final sentence — everything after the last sentence terminator
/// that ends a word — with leading non-alphanumerics trimmed. A '.' inside a
/// token ("amara.org", "otter.ai") does not end a sentence, so an outro
/// carrying a dotted attribution still reads as one final sentence. A text
/// with no terminator is itself the final sentence.
fn final_sentence(text: &str) -> &str {
    let trimmed = text.trim_end_matches(|character: char| !character.is_alphanumeric());
    let start = trimmed
        .char_indices()
        .filter(|&(index, character)| {
            matches!(character, '.' | '?' | '!')
                && trimmed[index + character.len_utf8()..]
                    .chars()
                    .next()
                    .is_some_and(char::is_whitespace)
        })
        .map(|(index, character)| index + character.len_utf8())
        .next_back()
        .unwrap_or(0);
    trimmed[start..].trim_start_matches(|character: char| !character.is_alphanumeric())
}

fn sentence_start_capitalisation(text: &str) -> (usize, usize) {
    let mut at_sentence_start = true;
    let mut capitalised = 0;
    let mut total = 0;
    for character in text.chars() {
        if character.is_alphabetic() && at_sentence_start {
            total += 1;
            if character.is_uppercase() {
                capitalised += 1;
            }
            at_sentence_start = false;
        } else if matches!(character, '.' | '?' | '!') {
            at_sentence_start = true;
        }
    }
    (capitalised, total)
}

fn distinct_nonoverlapping_dictionary_matches(text: &str, terms: &[String]) -> usize {
    let mut candidates: Vec<(&str, usize, usize)> = terms
        .iter()
        .map(String::as_str)
        .filter(|term| !term.is_empty())
        .collect::<HashSet<_>>()
        .into_iter()
        .flat_map(|term| {
            text.match_indices(term).filter_map(move |(start, matched)| {
                let before_is_boundary = text[..start]
                    .chars()
                    .next_back()
                    .is_none_or(|character| !character.is_alphanumeric());
                let end = start + matched.len();
                let after_is_boundary = text[end..]
                    .chars()
                    .next()
                    .is_none_or(|character| !character.is_alphanumeric());
                (before_is_boundary && after_is_boundary).then_some((term, start, end))
            })
        })
        .collect();
    candidates.sort_by_key(|(term, start, _)| (std::cmp::Reverse(term.len()), *start));

    let mut selected_terms = HashSet::new();
    let mut selected_spans: Vec<(usize, usize)> = Vec::new();
    for (term, start, end) in candidates {
        if selected_terms.contains(term)
            || selected_spans
                .iter()
                .any(|(selected_start, selected_end)| start < *selected_end && *selected_start < end)
        {
            continue;
        }
        selected_terms.insert(term);
        selected_spans.push((start, end));
    }
    selected_terms.len()
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

/// The worst observed silent contraction retained 87% of the longest source.
/// Rejecting below 90% catches all four production cases (77–87%) while an
/// exact 90% merge remains valid.
const MERGE_CONTRACTION_RATIO_FLOOR: f64 = 0.90;

enum QualityFailure {
    Other(&'static str),
    Contraction {
        ratio: f64,
        candidate_words: usize,
        source_words: usize,
    },
}

impl QualityFailure {
    fn is_contraction(&self) -> bool {
        matches!(self, Self::Contraction { .. })
    }

    fn reason(&self) -> String {
        match self {
            Self::Other(reason) => (*reason).to_owned(),
            Self::Contraction {
                ratio,
                candidate_words,
                source_words,
            } => format!(
                "suspicious contraction ratio {ratio:.4} below {MERGE_CONTRACTION_RATIO_FLOOR:.2} ({candidate_words} candidate words, {source_words} longest-source words)"
            ),
        }
    }
}

/// Every quality guard EXCEPT the merge contraction floor.
///
/// The floor exists to catch a reconcile merge that silently summarises the two
/// Source Transcripts, and it is measured against the LONGEST source. That
/// comparison is meaningful only for the merge output. It is meaningless — and
/// harmful — for text that is legitimately shorter than the sources: a Source
/// Transcript is not a merge of its sibling, and a repair exists precisely to
/// delete unsafe spans from a candidate.
fn non_contraction_quality_failure_reason(
    candidate: &str,
    sources: &[SourceTranscript],
) -> Option<String> {
    quality_failure_reason(candidate, sources).and_then(|failure| {
        if failure.is_contraction() {
            None
        } else {
            Some(failure.reason())
        }
    })
}

/// Whether every content word of a candidate is a word some Source Transcript
/// actually contains.
///
/// This is the guard the length bounds cannot be: repair asks a safety-tuned
/// model to rebuild an unsafe candidate USING ONLY the Source Transcripts, and
/// such a model may instead decline — "I can't help with that." is short,
/// clean, single-script, and trips nothing, yet it is not the user's speech.
/// Derivation catches it by its one distinguishing property: its vocabulary is
/// the model's, not the providers'. It is also the reason a repair may shrink
/// without limit, which no ratio can allow — deleting a hallucinated tail or
/// collapsing a repetition loop removes most of the words but invents none.
///
/// Stopwords are exempt. A repair that closes the gap left by a deleted span
/// may need a connective, and no refusal is recognisable by its function words.
///
/// A candidate with NO content words at all is not derived. Derivation is
/// positive evidence — at least one content word the providers actually heard —
/// and an all-stopword text offers none. The refusal shapes make this the
/// load-bearing half of the guard: "I can't do that." and "I won't do that."
/// normalise to pure stopwords ("can't" expands to "can not"), so a vacuous
/// pass here would type a model's refusal as the user's dictation.
fn is_source_derived(candidate: &str, sources: &[SourceTranscript]) -> bool {
    let source_words: Vec<String> = sources
        .iter()
        .flat_map(|source| normalized_words(&source.text))
        .collect();
    let vocabulary = distinct_content_words(&source_words);
    let candidate_words = normalized_words(candidate);
    let mut content_words = candidate_words
        .iter()
        .filter(|word| !is_stopword(word))
        .peekable();
    content_words.peek().is_some()
        && content_words.all(|word| vocabulary.contains(word.as_str()))
}

fn quality_failure_reason(
    candidate: &str,
    sources: &[SourceTranscript],
) -> Option<QualityFailure> {
    let trimmed = candidate.trim();
    if trimmed.is_empty() || trimmed.contains('\0') || trimmed.len() > 100_000 {
        return Some(QualityFailure::Other("invalid candidate text"));
    }
    let lower = trimmed.to_lowercase();
    // Injection markers, matched anywhere: an instruction smuggled into the
    // audio is unsafe wherever it lands, and none of these is ordinary speech —
    // they are imperative instruction forms, colon-terminated role labels, or
    // markup no speaker utters. The one entry that WAS ordinary speech,
    // "system prompt", is gone: "let us change the system prompt" is a sentence
    // this product's users dictate, and routing it into repair mangles or loses
    // it. The removal does forfeit one leak shape — a model prefacing its leak
    // with the label, "Here is the system prompt: ..." — which no surviving
    // marker catches. That trade is deliberate: the false positive cost whole
    // dictations, while the forfeited catch only lets a labelled leak reach
    // the user as visible text to delete.
    const PROMPT_ARTIFACTS: [&str; 7] = [
        "ignore previous instructions",
        "ignore all instructions",
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
        return Some(QualityFailure::Other("prompt artifact"));
    }
    // Meta-reasoning is a model narrating its own task, and a model that leaks
    // it leaks it as a PREAMBLE. The same words are ordinary English inside a
    // sentence: "Right, so the user said the deployment failed last night" is
    // speech, and as a bare substring "the user said" routed that dictation
    // into repair, where the round-1 floor could refuse it outright. These
    // markers therefore count only at the very start of the text, after any
    // leading markup a model may wrap them in — the one position a leaked
    // preamble always occupies.
    //
    // The residual is a leak that begins mid-text. That is the cheap direction:
    // it hands the user a visible sentence to delete, whereas the false
    // positive it replaces could cost the whole dictation. (Injection is the
    // other list; nothing here is unsafe text, only untidy text.)
    const META_REASONING: [&str; 6] = [
        "i think the user said",
        "the user said",
        "my final answer",
        "here is the transcript",
        "here is the reconciled",
        "based on the source",
    ];
    let preamble = lower.trim_start_matches(|character: char| !character.is_alphanumeric());
    if META_REASONING
        .iter()
        .any(|artifact| preamble.starts_with(artifact))
    {
        return Some(QualityFailure::Other("meta-reasoning"));
    }
    // The archetypal ASR outro hallucinations, learned from captioned video.
    // All five are TAIL artifacts — a model appends them as their own closing
    // sentence — so, symmetrically with the meta-reasoning preamble anchor,
    // they count only when they begin the text's final sentence. The same
    // words mid-sentence are ordinary dictation ("...and the recording was
    // transcribed by Whisper."), and a false positive costs a detour through
    // repair that can shorten or lose real speech.
    const HALLUCINATED_SUFFIXES: [&str; 5] = [
        "thank you for watching",
        "thanks for watching",
        "like and subscribe",
        "subtitles by",
        "transcribed by",
    ];
    let outro = final_sentence(&lower);
    if HALLUCINATED_SUFFIXES
        .iter()
        .any(|suffix| outro.starts_with(suffix))
    {
        return Some(QualityFailure::Other("hallucinated suffix"));
    }
    if script_count(trimmed) >= 3
        || trimmed
            .split_whitespace()
            .any(token_mixes_confusable_scripts)
    {
        return Some(QualityFailure::Other("mixed-script garbage"));
    }
    let source_words = sources
        .iter()
        .map(|source| normalized_words(&source.text).len())
        .max()
        .unwrap_or(0);
    let candidate_words = normalized_words(trimmed).len();
    if source_words > 0 {
        let contraction_ratio = candidate_words as f64 / source_words as f64;
        if sources.len() >= 2 && contraction_ratio < MERGE_CONTRACTION_RATIO_FLOOR {
            return Some(QualityFailure::Contraction {
                ratio: contraction_ratio,
                candidate_words,
                source_words,
            });
        }
        if candidate_words > source_words.saturating_mul(2).saturating_add(8) {
            return Some(QualityFailure::Other("suspicious expansion"));
        }
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
        .flat_map(|word| {
            let word = word
                .chars()
                .filter_map(|character| match character {
                    '\u{2019}' => Some('\''),
                    character if character.is_alphanumeric() || character == '\'' => {
                        Some(character)
                    }
                    _ => None,
                })
                .flat_map(char::to_lowercase)
                .collect::<String>();
            let expansion: &[&str] = match word.as_str() {
                "aren't" => &["are", "not"],
                "can't" => &["can", "not"],
                "couldn't" => &["could", "not"],
                "didn't" => &["did", "not"],
                "doesn't" => &["does", "not"],
                "don't" => &["do", "not"],
                "hadn't" => &["had", "not"],
                "hasn't" => &["has", "not"],
                "haven't" => &["have", "not"],
                "needn't" => &["need", "not"],
                "isn't" => &["is", "not"],
                "let's" => &["let", "us"],
                "mustn't" => &["must", "not"],
                "shan't" => &["shall", "not"],
                "shouldn't" => &["should", "not"],
                "they're" => &["they", "are"],
                "wasn't" => &["was", "not"],
                "we're" => &["we", "are"],
                "weren't" => &["were", "not"],
                "won't" => &["will", "not"],
                "wouldn't" => &["would", "not"],
                "you're" => &["you", "are"],
                "i'm" => &["i", "am"],
                "he's" => &["he", "is"],
                "she's" => &["she", "is"],
                "it's" => &["it", "is"],
                "that's" => &["that", "is"],
                "there's" => &["there", "is"],
                "here's" => &["here", "is"],
                "what's" => &["what", "is"],
                "where's" => &["where", "is"],
                "who's" => &["who", "is"],
                "how's" => &["how", "is"],
                "i've" => &["i", "have"],
                "we've" => &["we", "have"],
                "they've" => &["they", "have"],
                "you've" => &["you", "have"],
                "i'll" => &["i", "will"],
                "we'll" => &["we", "will"],
                "they'll" => &["they", "will"],
                "you'll" => &["you", "will"],
                "i'd" => &["i", "would"],
                "he'd" => &["he", "would"],
                "she'd" => &["she", "would"],
                "we'd" => &["we", "would"],
                "they'd" => &["they", "would"],
                "you'd" => &["you", "would"],
                _ => return vec![word.replace('\'', "")],
            };
            expansion.iter().map(|part| (*part).to_owned()).collect()
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
    fn set_dictionary_terms(&mut self, _dictionary_terms: Vec<String>) {}

    fn validate(
        &mut self,
        sources: Vec<SourceTranscript>,
    ) -> BoundaryFuture<'_, TranscriptDecision>;
}

impl<M: ReconciliationModel> TranscriptValidator for TranscriptDecisionPipeline<M> {
    fn set_dictionary_terms(&mut self, dictionary_terms: Vec<String>) {
        TranscriptDecisionPipeline::set_dictionary_terms(self, dictionary_terms);
    }

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
    /// Captures any per-Recording Delivery precondition at the moment Recording
    /// starts. Most adapters have no precondition and keep the no-op default.
    fn recording_started(&mut self) -> BoundaryFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn deliver(&mut self, transcript: Transcript) -> BoundaryFuture<'_, DeliveryOutcome>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowIdentity {
    /// Opaque compositor-defined stable token: KWin internalId UUID or
    /// Hyprland window address. Never a title or caption.
    pub stable_id: String,
    /// Corroborating fields for diagnostics only — never the comparison key.
    pub process_id: Option<u32>,
    pub app_id: Option<String>,
}

pub trait FocusProbe: Send {
    /// `None` means focus cannot be determined. Callers must fail closed rather
    /// than treating an unavailable focus observation as unchanged.
    fn current(&mut self) -> BoundaryFuture<'_, Option<WindowIdentity>>;
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
/// to the listener: a closed session or a portal restart clears the stale
/// binding and rebinds; permanence comes only from a refused bind (portal
/// response 1), never from any event here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShortcutEvent {
    /// The user pressed the Trigger Key.
    Activated,
    /// The desktop emitted `Session.Closed`. The XDG Session protocol defines
    /// this only as "the session ended" and GlobalShortcuts carries no reason,
    /// so it is NOT proof of a deliberate revocation — a compositor or backend
    /// reset (e.g. across suspend) closes the session the same way. The binding
    /// is stale and must be cleared, but the listener rebinds with bounded
    /// backoff. A genuine revocation is expected to surface as a refused bind
    /// (portal response 1) on a later attempt — that refusal, not the closure,
    /// is what permanently retires the Trigger Key; if the portal neither
    /// accepts nor refuses the rebind, re-attempts continue.
    SessionClosed,
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

    /// Awaits the next session event: a Trigger Key activation, a session
    /// closure, or a portal loss/restart transition. A `Shortcut` boundary error
    /// signals that the underlying connection ended (all streams closed) — a
    /// recoverable stream failure the listener answers by rebinding.
    fn next_event(&mut self) -> BoundaryFuture<'_, ShortcutEvent>;
}
