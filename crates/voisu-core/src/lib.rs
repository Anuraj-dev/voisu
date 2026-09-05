//! Shared domain, provider coordination, and IPC types for Voisu.

use std::collections::{HashMap, HashSet};
use std::env;
use std::future::Future;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

mod session;
pub use session::{
    ClipboardTool, PACKAGE_MANAGERS, PackageManager, SessionKind, SessionResolution,
    clipboard_candidates, install_instruction, resolve_session,
};

mod wav;
pub use wav::{WavScan, scan_wav_pcm};

mod diagnostics;
pub use diagnostics::{
    ConfidenceArbitrationDiagnostic, ConfidenceArbitrationRejection, DEFAULT_DEBUG_AUDIO_TTL,
    DEFAULT_MAX_AGE, DEFAULT_MAX_RECORDS, DebugAudioRecord, DiagnosticExport, DiagnosticRecord,
    DiagnosticStore, EXPORT_ENV_ALLOWLIST, EnglishEligibilityOutcome,
    IntentReconstructionDiagnostic, IntentReconstructionEligibility, IntentReconstructionOutcome,
    MAX_CONFIDENCE_ARBITRATION_REJECTIONS, MAX_MODEL_ID_UTF8_BYTES,
    MAX_SMART_WRITING_DIAGNOSTIC_EDITS, MAX_SMART_WRITING_DIAGNOSTIC_TEXT_UTF8_BYTES,
    MAX_SMART_WRITING_EDIT_FIELD_UTF8_BYTES, MAX_SMART_WRITING_FREE_TEXT_UTF8_BYTES,
    MAX_STORED_TEXT, PruneOutcome, REDACTED, ReplayOutcome, RetentionPolicy,
    SMART_WRITING_DIAGNOSTIC_VERSION, SmartWritingDiagnostic, SmartWritingEditEvidence,
    SmartWritingMode, SmartWritingOutcome, SmartWritingReasonCode, SourceCoverageRecord,
    SourceSelectionConfidence, SourceSelectionDiagnostic, SourceTranscriptRecord, TELEMETRY_SCHEMA,
    TEXT_SHA256_FINGERPRINT_LEN, clamp_stored_transcript_text, clamp_utf8_bytes, correlation_id,
    export_record, is_secret_env_key, is_text_sha256_fingerprint, redacted_environment,
    replay_capture, sanitize_url, scrub_embedded_urls, scrub_secret_values,
    text_sha256_fingerprint, unix_millis_now,
};

mod confidence_arbitration;

mod dpr_diagnostics;
#[cfg(not(feature = "dpr-eval-late-retain"))]
pub use dpr_diagnostics::DPR_EVALUATION_LANE_COMPILE_GATED;
pub use dpr_diagnostics::{
    DPR_DIAGNOSTIC_VERSION, DPR_LOCAL_FALLBACK_MESSAGE, DprDeliveryEvidence, DprDiagnostic,
    DprDiagnosticEvent, DprDiagnosticEventName, DprDiagnosticMode, DprFeedbackKind,
    DprSpanAdjudication, DprSpanRejection, MAX_DPR_DIAGNOSTIC_EVENTS,
};
#[cfg(feature = "dpr-eval-late-retain")]
pub use dpr_diagnostics::{
    DprAcceptedLateCandidate, DprLateEvaluationRecord, MAX_DPR_RETAINED_LATE_TEXT_UTF8_BYTES,
};

mod formatting_commands;
pub use formatting_commands::{
    CommandEvent, CommandKind, NumberedListItem, ParsedCommands, SourceSpan,
    parse_formatting_commands,
};

mod formatting;
pub use formatting::{
    FORMATTER_CONTRACT_ID, FormatOptions, FormattingBaseline, LOCAL_FORMATTER_WORK_DEADLINE,
    MAX_VALIDATED_TRANSCRIPT_UTF8_BYTES, SourceAnchor, VALIDATED_TRANSCRIPT_VERSION, WritingMode,
    format_validated, format_validated_for_grammar, format_validated_with,
};

mod grammar_safety;
pub use grammar_safety::{
    GrammarDiagnostic, GrammarErrorCode, GrammarOutcome, GrammarSafetyOptions, GrammarSafetyResult,
    MAX_GRAMMAR_DIAGNOSTIC_UTF8_BYTES, MAX_GRAMMAR_EDIT_FIELD_UTF8_BYTES, MAX_GRAMMAR_EDITS,
    MAX_GRAMMAR_JSON_DEPTH, MAX_GRAMMAR_JSON_NODES, MAX_GRAMMAR_RESPONSE_BYTES,
    apply_grammar_candidate_json,
};

mod prompt_rendering;
pub use prompt_rendering::{
    CLOSED_STRUCTURED_LABELS, CloudRequest, DEFAULT_RENDERING_POLICY, DELIVERY_AUTO_SEND,
    DELIVERY_DEADLINE, DELIVERY_DEADLINE_MS, DELIVERY_LIVE_TYPE, DELIVERY_REPLACE_DELIVERED,
    DELIVERY_STATE_UNSENT, RenderingPolicy, RenderingRoute, TimingCertainty,
};

mod dictation_grammar;

mod local_baseline;
pub use local_baseline::{
    LOCAL_BASELINE_CONTRACT_ID, LocalBaseline, LocalBaselineOptions, LocalTiming, PauseBoundary,
    leftover_admits_format_cloud, organize_local_baseline,
};

mod intent_routing;
pub use intent_routing::{
    BROWSER_SHORT_WORDS, COMPLEXITY_CLOUD_THRESHOLD, IntentObservation, MESSAGING_SHORT_WORDS,
    ProcessClass, ProcessHint, ProviderState, RoutingDecision, RuleId,
    SECTION_CUES_FOR_LENGTH_ASSIST, ScoreContribution, SurfaceHint, TimingHint, is_command_shaped,
    route_intent,
};

mod vocabulary;

mod compose_gate;
pub use compose_gate::{
    CLOSED_CONVERSIONS, CLOSED_SOURCE_SELECTION_REASONS, COMPOSE_GATE_CONTRACT_ID, CloudOutcome,
    ComposeCertainty, ComposeErrorCode, ComposeInput, ComposeOutcome, ComposeSource,
    ComposeSpanRejection, ComposeSpanSummary, CompositionDecision, ConversionClaim, DeliveryFlags,
    DerivationSpan, FallbackTrigger, LabelClaim, LayoutClaim, LayoutDecision,
    MAX_COMPOSE_CONVERSIONS, MAX_COMPOSE_DERIVATION_SPANS, MAX_COMPOSE_FIELD_UTF8_BYTES,
    MAX_COMPOSE_LABELS, MAX_COMPOSE_REMOVALS, Reconciliation, RemovalClaim, RemovalKind,
    SourceSelection, SpanKind, StructuredCandidate, SttProvider, compose_structured_candidate,
    parse_structured_candidate_json,
};

mod format_edits;
pub use format_edits::{
    CLOSED_FORMAT_EDIT_KINDS, FORMAT_EDIT_CONTRACT_ID, FORMAT_EDIT_CONTRACT_VERSION, FormatEdit,
    FormatEditCandidate, FormatEditErrorCode, FormatEditKind, FormatEditOutcome, FormatEditSafety,
    MAX_FORMAT_EDIT_FIELD_UTF8_BYTES, MAX_FORMAT_EDIT_JSON_DEPTH, MAX_FORMAT_EDIT_JSON_NODES,
    MAX_FORMAT_EDIT_RESPONSE_BYTES, MAX_FORMAT_EDITS, apply_format_edit_candidate_json,
    apply_format_edit_candidate_json_with, apply_format_edits, apply_format_edits_with,
    parse_format_edit_candidate_json,
};

// Paged diagnostic responses are negotiated per REQUEST (see `Request::paged`),
// never by protocol version. The version is interpolated into the socket path,
// the single-instance lock, and the store directory, so bumping it partitions
// the whole namespace and an already-running older daemon becomes unreachable
// instead of reporting a mismatch. A purely additive, defaulted wire field costs
// nothing in either skew direction and keeps one namespace.
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

/// Voisu's durable per-user state directory: `$XDG_STATE_HOME/voisu`, falling
/// back to `~/.local/state/voisu`.
///
/// Unlike [`runtime_dir`], this survives logout and reboot. `systemd-logind`
/// removes `/run/user/$UID` when the user's last session ends, so anything kept
/// under the runtime directory has an effective lifetime of "since last login" —
/// a retention policy measured in days cannot be honoured there. The daemon unit
/// already provisions this location (`StateDirectory=voisu`).
///
/// Returning the path does not make it safe to write to: the caller creates it
/// private and verifies ownership, exactly as the runtime directory is checked
/// before anything is written there.
pub fn state_dir() -> Result<PathBuf, String> {
    let root = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".local/state"))
        })
        .ok_or_else(|| "neither XDG_STATE_HOME nor HOME names an absolute path".to_owned())?;
    Ok(root.join("voisu"))
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
    Level {
        after_seq: u64,
    },
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
    /// Asks the daemon to split a large diagnostic history or export across
    /// contiguous [`DiagnosticPage`] frames rather than one frame.
    ///
    /// Purely additive and defaulted: a client that predates paging omits the
    /// field and is served the single-frame response it has always received, so
    /// paging needs no protocol version bump in either skew direction. Only
    /// `history` and `export` ever produce pages; every other command answers in
    /// one frame regardless.
    #[serde(default, skip_serializing_if = "is_not_set")]
    pub paged: bool,
}

/// `skip_serializing_if` for an optional boolean flag: an unset flag is omitted
/// so a request that does not use paging is byte-identical to one from a client
/// that has never heard of it.
fn is_not_set(value: &bool) -> bool {
    !*value
}

impl Request {
    /// A request answered in a single response frame.
    pub fn new(command: Command) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            command,
            paged: false,
        }
    }

    /// A request whose diagnostic payload may be split across pages.
    pub fn paged(command: Command) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            command,
            paged: true,
        }
    }
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_failures: Vec<ProviderFailure>,
    /// DEPRECATED for latency analysis: measured from recording START, so it
    /// includes the user's whole speech duration — a 60 s dictation looks 60 s
    /// "slower" than a 3 s one. Kept (and still written) only so old history
    /// lines and old parsers stay readable; prefer `recording_duration_ms` and
    /// `stop_to_delivered_ms`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_to_text_ms: Option<u64>,
    /// Recording start → stop: the duration of the user's actual speech. The
    /// denominator that makes the stop-anchored fields comparable across
    /// dictations of different lengths.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_duration_ms: Option<u64>,
    /// Stop → the final transcript settled: validation and reconciliation (the
    /// late-reconstruction window) have resolved and the delivered text is
    /// known. Absent when no transcript ever settled (an abort before the
    /// providers completed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_to_finalized_ms: Option<u64>,
    /// Stop → Delivery completed (the moment `delivery_count` increments).
    /// Unlike the deprecated `release_to_text_ms`, this excludes speech
    /// duration, so it is comparable across dictations of any length.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_to_delivered_ms: Option<u64>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_selection_diagnostic: Option<SourceSelectionDiagnostic>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent_reconstruction: Option<IntentReconstructionDiagnostic>,
    /// Slice B4 additive confidence-arbitration evidence. Absent when
    /// arbitration did not run (single provider, missing confidence, or a
    /// selection it never touches), so pre-B4 responses and history lines
    /// stay parseable in both skew directions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence_arbitration: Option<ConfidenceArbitrationDiagnostic>,
    /// Replay-only additive evidence: the provider-tagged Source Transcript
    /// texts the replay produced and the final selected Transcript text, so a
    /// machine reader can score a replayed fixture without scraping human
    /// output. Live Recordings never populate these — the persisted history
    /// record already carries the same texts — and `default` plus
    /// `skip_serializing_if` keep every pre-existing response and reader
    /// byte-compatible in both skew directions. Texts are clamped to
    /// [`MAX_STORED_TEXT`] exactly like history records.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_transcripts: Vec<SourceTranscriptRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_transcript: Option<String>,
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PasteActionState {
    /// The configured Hyprland paste action was found and verified live.
    Verified,
    /// Clipboard delivery is selected, but no safe paste action was verified.
    ClipboardOnly,
    /// The selected Delivery mode does not use a Hyprland paste action.
    NotRequired,
}

/// Readiness captured in the daemon's own environment at process start.
///
/// This is intentionally reported by the daemon rather than inferred by the
/// interactive CLI: a systemd user service can retain an old display endpoint
/// after a compositor/session change even while the CLI has a healthy one.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DaemonReadiness {
    pub session_type: Option<String>,
    pub wayland_display: Option<String>,
    pub x11_display: Option<String>,
    /// X11 authority path inherited by the daemon, never the authority data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x_authority: Option<String>,
    /// Hyprland compositor instance identity captured by the daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hyprland_instance_signature: Option<String>,
    pub delivery_mode: String,
    pub paste_action: PasteActionState,
    /// The first clipboard writer available in the daemon's PATH, if any.
    /// Presence alone does not mean it can reach the daemon's display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clipboard_backend: Option<String>,
    /// Proven by a bounded read-only backend probe in the daemon process.
    pub clipboard_usable: bool,
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
    pub diagnostic_page: Option<DiagnosticPage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay_event: Option<OverlayEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level_frames: Option<Vec<LevelFrame>>,
    /// Milliseconds of headroom left before the Recording Deadline stops the
    /// live Recording, or `None` when nothing is recording. Presentation only:
    /// the Deadline is enforced by capture against its own clock, and this
    /// value exists so the Overlay can warn the user before the cut without
    /// holding a second copy of the limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_remaining_ms: Option<u64>,
    /// Additive daemon-owned session and Delivery diagnostics. Older daemons do
    /// not send this field and older clients ignore it on the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness: Option<DaemonReadiness>,
}

/// One transport page of a serialized diagnostic history or export.
///
/// Pages are zero-based and contiguous. `payload` is a byte-for-byte UTF-8
/// fragment of the command's complete JSON value; concatenating payloads
/// through the page with `last = true` reconstructs that value.
#[derive(Debug, Deserialize, Serialize)]
pub struct DiagnosticPage {
    pub sequence: u64,
    pub last: bool,
    pub payload: String,
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
            diagnostic_page: None,
            overlay_event: None,
            level_frames: None,
            recording_remaining_ms: None,
            readiness: None,
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
    pub source_selection_diagnostic: SourceSelectionDiagnostic,
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

/// Strip anchored ASR outro hallucinations from one Source Transcript text.
///
/// Pure-outro text becomes empty. Genuine speech with an anchored final outro
/// keeps the speech and loses only that outro. Mid-sentence mentions of the
/// same closed phrases are preserved. After a strip, stopword-only or trivial
/// head remainders (e.g. "Please", "OK", "Yeah") also clear so they never
/// select or Deliver; multi-word genuine speech is kept.
#[must_use]
pub fn sanitize_source_transcript_text(text: &str) -> String {
    let mut current = text.trim().to_owned();
    let mut stripped_any = false;
    while let Some(stripped) = strip_one_anchored_hallucinated_outro(&current) {
        stripped_any = true;
        current = stripped;
    }
    if stripped_any && !remainder_is_substantial_speech(&current) {
        return String::new();
    }
    current
}

/// Clear pure-outro sources and strip only anchored final outros from each
/// Source Transcript. Empty strings after sanitization mean the source carried
/// no usable speech.
#[must_use]
pub fn sanitize_source_transcripts(
    sources: impl IntoIterator<Item = SourceTranscript>,
) -> Vec<SourceTranscript> {
    sources
        .into_iter()
        .map(|source| SourceTranscript {
            provider: source.provider,
            text: sanitize_source_transcript_text(&source.text),
        })
        .collect()
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
const RECONCILIATION_CLEANUP_GRACE: Duration = Duration::from_secs(3);

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

    fn reconstruct_intent(
        &mut self,
        request: IntentReconstructionRequest,
        cancel: Arc<CancelRegistry>,
    ) -> BoundaryFuture<'_, MergeResult> {
        self.request(ReconciliationKind::Reconcile, request.sources, None, cancel)
    }
}

#[derive(Clone, Debug)]
pub struct IntentReconstructionRequest {
    pub sources: Vec<SourceTranscript>,
    pub dictionary_terms: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IntentReconstructionResponse {
    #[serde(alias = "inferred_text")]
    wording: String,
}

pub fn parse_intent_reconstruction_response(content: &str) -> Result<MergeResult, BoundaryError> {
    serde_json::from_str::<IntentReconstructionResponse>(content)
        .map(|response| MergeResult(response.wording))
        .map_err(|_| {
            BoundaryError::new(
                BoundaryKind::Validation,
                "Intent Reconstruction returned invalid shape",
            )
        })
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptSelection {
    Complementary,
    NearIdenticalGroq,
    Reconciled,
    Repaired,
    IntentReconstructed,
    SourceDeepgram,
    SourceGroq,
}

#[derive(Clone, Debug)]
pub struct IntentReconstructionEvidence {
    pub eligibility: IntentReconstructionEligibility,
    pub outcome: IntentReconstructionOutcome,
    pub candidate: Option<String>,
}

impl IntentReconstructionEvidence {
    fn skipped(eligibility: IntentReconstructionEligibility) -> Self {
        Self {
            eligibility,
            outcome: IntentReconstructionOutcome::Skipped,
            candidate: None,
        }
    }
}

#[derive(Debug)]
pub struct TranscriptDecision {
    pub transcript: Transcript,
    pub selection: TranscriptSelection,
    pub validation_reason: String,
    pub fallback_reason: Option<String>,
    pub reconciliation_requested: bool,
    pub recovery_attempted: bool,
    pub source_selection_diagnostic: SourceSelectionDiagnostic,
    pub intent_reconstruction: Option<IntentReconstructionEvidence>,
    /// Slice B4: additive confidence-arbitration evidence. `None` whenever
    /// arbitration did not run (no both-sources-and-both-evidence case, or a
    /// selection arbitration never touches) — the delivered text is then
    /// byte-identical to the pre-B4 pipeline.
    pub confidence_arbitration: Option<ConfidenceArbitrationDiagnostic>,
}

#[derive(Debug)]
pub struct IntentReconstructionAttempt {
    pub request: IntentReconstructionRequest,
    pub eligibility: IntentReconstructionEligibility,
    classification: SourcePairClassification,
    fallback: Result<TranscriptDecision, BoundaryError>,
}

#[derive(Debug)]
pub enum PreparedTranscriptDecision {
    Ready(TranscriptDecision),
    Reconstruct(IntentReconstructionAttempt),
}

pub struct TranscriptDecisionPipeline<M> {
    model: M,
    deadline: Duration,
    dictionary_terms: Vec<String>,
    /// The USER's personal dictionary terms, for the constrained
    /// post-correction pass. Deliberately separate from `dictionary_terms`
    /// (the merged vocabulary used for selection evidence and reconciliation
    /// context): only user-owned terms may rewrite the Transcript, and an
    /// empty user dictionary must leave the pipeline's output byte-identical.
    user_vocabulary: Vec<String>,
    /// Word-level provider confidence evidence for the current Recording's
    /// transcripts, used to gate corrections on the Deepgram-sourced final
    /// Transcript (see `vocabulary`).
    word_confidences: Vec<ProviderWordConfidences>,
    intent_reconstruction: bool,
}

impl<M: ReconciliationModel> TranscriptDecisionPipeline<M> {
    pub fn new(model: M, deadline: Duration) -> Self {
        Self {
            model,
            deadline,
            dictionary_terms: Vec::new(),
            user_vocabulary: Vec::new(),
            word_confidences: Vec::new(),
            intent_reconstruction: false,
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
            user_vocabulary: Vec::new(),
            word_confidences: Vec::new(),
            intent_reconstruction: false,
        }
    }

    pub fn with_intent_reconstruction(
        model: M,
        deadline: Duration,
        dictionary_terms: Vec<String>,
    ) -> Self {
        Self {
            model,
            deadline,
            dictionary_terms,
            user_vocabulary: Vec::new(),
            word_confidences: Vec::new(),
            intent_reconstruction: true,
        }
    }

    pub fn set_dictionary_terms(&mut self, dictionary_terms: Vec<String>) {
        self.dictionary_terms = dictionary_terms;
    }

    /// Sets the user's personal dictionary terms for the post-correction pass.
    /// The daemon derives them from the SAME single dictionary snapshot that
    /// feeds keyterms/whisper prompt/`dictionary_terms`, so one Recording never
    /// mixes vocabulary versions.
    pub fn set_user_vocabulary(&mut self, user_vocabulary: Vec<String>) {
        self.user_vocabulary = user_vocabulary;
    }

    /// Sets the word-level confidence evidence for this Recording's provider
    /// transcripts. Empty by default; the daemon supplies what its providers
    /// retained before validation runs.
    pub fn set_word_confidences(&mut self, word_confidences: Vec<ProviderWordConfidences>) {
        self.word_confidences = word_confidences;
    }

    pub async fn prepare(
        &mut self,
        mut sources: Vec<SourceTranscript>,
    ) -> Result<PreparedTranscriptDecision, BoundaryError> {
        if !self.intent_reconstruction {
            return self
                .decide(sources)
                .await
                .map(PreparedTranscriptDecision::Ready);
        }

        sources = sanitize_source_transcripts(sources);
        sources.retain(|source| !source.text.is_empty());
        sources.sort_by_key(|source| source.provider);
        if let (Some(deepgram), Some(groq)) = (
            sources
                .iter()
                .find(|source| source.provider == Provider::Deepgram),
            sources
                .iter()
                .find(|source| source.provider == Provider::Groq),
        ) {
            let classification = source_pair_classification(&deepgram.text, &groq.text);
            if let Some(eligibility) = classification.intent_eligibility() {
                let mut fallback = safe_source_fallback(
                    &sources,
                    "Intent Reconstruction fallback prepared".to_owned(),
                    true,
                    false,
                );
                if let Some(confidence) = classification.diagnostic_confidence()
                    && let Ok(decision) = &mut fallback
                {
                    decision.source_selection_diagnostic.confidence = Some(confidence);
                }
                return Ok(PreparedTranscriptDecision::Reconstruct(
                    IntentReconstructionAttempt {
                        request: IntentReconstructionRequest {
                            sources,
                            dictionary_terms: self.dictionary_terms.clone(),
                        },
                        eligibility,
                        classification,
                        fallback,
                    },
                ));
            }
        }

        let mut decision = self.decide(sources).await?;
        let eligibility = if decision.selection == TranscriptSelection::Repaired {
            IntentReconstructionEligibility::RepairPath
        } else if decision.source_selection_diagnostic.sources.len() == 1 {
            IntentReconstructionEligibility::SingleSource
        } else {
            IntentReconstructionEligibility::NearIdenticalHighConfidence
        };
        decision.intent_reconstruction = Some(IntentReconstructionEvidence::skipped(eligibility));
        Ok(PreparedTranscriptDecision::Ready(decision))
    }

    /// Completes a prepared Intent Reconstruction attempt, with the user's
    /// constrained vocabulary corrections applied to whatever final Transcript
    /// the attempt ends in — an accepted reconstruction or its fallback. The
    /// fallback decision is built UNCORRECTED by `safe_source_fallback` during
    /// `prepare` (it never passes through the correcting `decide` wrapper);
    /// this wrapper is what corrects it, exactly as it corrects an accepted
    /// reconstruction. The correction is idempotent, so a decision that had
    /// already been corrected is re-applied as a no-op.
    pub async fn reconstruct(
        &mut self,
        attempt: IntentReconstructionAttempt,
    ) -> Result<TranscriptDecision, BoundaryError> {
        let decision = self.reconstruct_uncorrected(attempt).await?;
        Ok(self.apply_user_vocabulary_correction(decision))
    }

    async fn reconstruct_uncorrected(
        &mut self,
        attempt: IntentReconstructionAttempt,
    ) -> Result<TranscriptDecision, BoundaryError> {
        let IntentReconstructionAttempt {
            request,
            eligibility,
            classification,
            fallback,
        } = attempt;
        let sources = request.sources.clone();
        let cancel = CancelRegistry::new();
        let model_request = self.model.reconstruct_intent(request, Arc::clone(&cancel));
        let candidate = match bounded_model_call(model_request, cancel, self.deadline).await {
            Ok(candidate) => candidate,
            Err(ModelCallFailure::Failed(error)) => {
                return intent_fallback(
                    fallback,
                    eligibility,
                    IntentReconstructionOutcome::Failed,
                    format!("Intent Reconstruction failed: {}", error.diagnostic()),
                    None,
                );
            }
            Err(ModelCallFailure::DeadlineElapsed) => {
                return intent_fallback(
                    fallback,
                    eligibility,
                    IntentReconstructionOutcome::Deadline,
                    "Intent Reconstruction deadline elapsed".to_owned(),
                    None,
                );
            }
        };
        let wording = candidate.0.trim();
        let bounded_candidate = Some(clamp_utf8_bytes(wording, MAX_STORED_TEXT));
        if wording.is_empty()
            || wording.len() > MAX_INTENT_RECONSTRUCTION_UTF8_BYTES
            || sanitize_source_transcript_text(wording) != wording
        {
            return intent_fallback(
                fallback,
                eligibility,
                IntentReconstructionOutcome::Rejected,
                "Intent Reconstruction returned empty, oversized, or outro text".to_owned(),
                bounded_candidate,
            );
        }
        Ok(TranscriptDecision {
            transcript: Transcript(wording.to_owned()),
            selection: TranscriptSelection::IntentReconstructed,
            validation_reason: "Intent Reconstruction passed validation".to_owned(),
            fallback_reason: None,
            reconciliation_requested: true,
            recovery_attempted: false,
            source_selection_diagnostic: source_selection_diagnostic(
                &sources,
                None,
                classification.diagnostic_confidence(),
            ),
            intent_reconstruction: Some(IntentReconstructionEvidence {
                eligibility,
                outcome: IntentReconstructionOutcome::Accepted,
                candidate: bounded_candidate,
            }),
            confidence_arbitration: None,
        })
    }

    /// Produces the final Transcript decision, with confidence-aware
    /// divergence-point arbitration inside selection and the user's
    /// constrained vocabulary corrections applied to the chosen final text.
    ///
    /// Placement: the uncorrected decision selects the incumbent whole
    /// (a Source Transcript, a merge, or a repair). When BOTH providers
    /// delivered a Source Transcript AND both retained word-confidence
    /// evidence for their own text, the B4 arbitration pass
    /// may then flip decisively-more-confident divergence points to the other
    /// provider's words (slice B4) — still inside selection, before any
    /// correction. The user-vocabulary correction then runs LAST, on whatever
    /// text selection delivered: it is an exact whole-token rewrite with
    /// user-owned vocabulary that preserves every content word's count, so
    /// the quality-guard outcomes vetted inside the decision remain valid,
    /// while `is_source_derived` — which must reject words no provider
    /// literally heard — is never handed a correction to reject.
    ///
    /// Provenance rule (pinned): a flip changes the delivered text to mixed
    /// provenance, but the word-confidence evidence the correction gate reads
    /// stays the SELECTED (backbone) provider's — arbitration never re-tags
    /// or merges evidence streams, so the correction gate's documented rules
    /// apply unchanged.
    pub async fn decide(
        &mut self,
        sources: Vec<SourceTranscript>,
    ) -> Result<TranscriptDecision, BoundaryError> {
        let decision = self.decide_uncorrected(sources.clone()).await?;
        let decision = self.apply_confidence_arbitration(decision, &sources);
        Ok(self.apply_user_vocabulary_correction(decision))
    }

    /// Slice B4: flips divergent regions of the selected text to the other
    /// provider's words when BOTH sides' confidence evidence makes the flip
    /// decisive and every meaning-preservation guard holds. Everything else —
    /// single provider, missing evidence, evidence that does not describe its
    /// own text, non-source selections — returns the decision untouched:
    /// byte-identical to the pre-B4 pipeline, with no arbitration diagnostic.
    fn apply_confidence_arbitration(
        &self,
        mut decision: TranscriptDecision,
        sources: &[SourceTranscript],
    ) -> TranscriptDecision {
        // Arbitration only ever rewords a selection that IS one provider's
        // source text. A reconciled, repaired, or reconstructed final is not
        // one provider's words, so there is no "other provider's words" to
        // take and no aligned confidence to vouch for the splice.
        if !matches!(
            decision.selection,
            TranscriptSelection::SourceDeepgram
                | TranscriptSelection::SourceGroq
                | TranscriptSelection::NearIdenticalGroq
        ) {
            return decision;
        }
        // The same sanitization the selection saw, so the texts arbitrated
        // are the texts that were selected from.
        let sanitized: Vec<SourceTranscript> = sanitize_source_transcripts(sources.to_vec())
            .into_iter()
            .filter(|source| !source.text.is_empty())
            .collect();
        let deepgram = sanitized
            .iter()
            .find(|source| source.provider == Provider::Deepgram);
        let groq = sanitized
            .iter()
            .find(|source| source.provider == Provider::Groq);
        let (Some(deepgram), Some(groq)) = (deepgram, groq) else {
            return decision;
        };
        let (incumbent, other) = match decision.selection {
            TranscriptSelection::SourceDeepgram => (deepgram, groq),
            _ => (groq, deepgram),
        };
        // Belt-and-braces: the arbitrated text must BE the incumbent
        // provider's source, or the alignment would describe a text the
        // decision did not deliver.
        if incumbent.text.trim() != decision.transcript.0 {
            return decision;
        }
        // Both sides need their OWN provider's evidence; anything missing
        // keeps the current behavior entirely (no flip, no diagnostic).
        let incumbent_evidence = self
            .word_confidences
            .iter()
            .find(|evidence| evidence.provider == incumbent.provider)
            .map(|evidence| &evidence.words);
        let other_evidence = self
            .word_confidences
            .iter()
            .find(|evidence| evidence.provider == other.provider)
            .map(|evidence| &evidence.words);
        let (Some(incumbent_evidence), Some(other_evidence)) = (incumbent_evidence, other_evidence)
        else {
            return decision;
        };
        if incumbent_evidence.is_empty() || other_evidence.is_empty() {
            return decision;
        }
        let Some(outcome) =
            confidence_arbitration::arbitrate(confidence_arbitration::ArbitrationInput {
                incumbent_text: incumbent.text.trim(),
                incumbent_confidences: incumbent_evidence,
                other_text: other.text.trim(),
                other_confidences: other_evidence,
                sources: &sanitized,
            })
        else {
            return decision;
        };
        decision.confidence_arbitration = Some(ConfidenceArbitrationDiagnostic {
            regions_considered: outcome.regions_considered,
            regions_flipped: outcome.regions_flipped,
            rejections: outcome.rejections,
        });
        if outcome.regions_flipped > 0 {
            decision.transcript = Transcript(outcome.text);
            decision.validation_reason = format!(
                "{}; confidence arbitration flipped {} divergent region(s)",
                decision.validation_reason, outcome.regions_flipped
            );
        }
        decision
    }

    fn apply_user_vocabulary_correction(
        &self,
        mut decision: TranscriptDecision,
    ) -> TranscriptDecision {
        if self.user_vocabulary.is_empty() {
            return decision;
        }
        // The confidence gate applies ONLY when the final Transcript IS the
        // Deepgram source: that is the one text whose words carry Deepgram
        // confidences. Groq-sourced, merged, repaired, and reconstructed
        // finals have no aligned word evidence, so their substitutions apply
        // ungated — the user explicitly asked for this vocabulary.
        let word_confidences: &[(String, f64)] =
            if decision.selection == TranscriptSelection::SourceDeepgram {
                self.word_confidences
                    .iter()
                    .find(|evidence| evidence.provider == Provider::Deepgram)
                    .map(|evidence| evidence.words.as_slice())
                    .unwrap_or(&[])
            } else {
                &[]
            };
        let corrected = vocabulary::apply_user_vocabulary(
            &decision.transcript.0,
            &self.user_vocabulary,
            word_confidences,
        );
        if corrected != decision.transcript.0 {
            decision.transcript = Transcript(corrected);
            decision.validation_reason = format!(
                "{}; user vocabulary corrections applied",
                decision.validation_reason
            );
        }
        decision
    }

    async fn decide_uncorrected(
        &mut self,
        mut sources: Vec<SourceTranscript>,
    ) -> Result<TranscriptDecision, BoundaryError> {
        // Classify and strip ASR outro hallucinations before any selection or
        // model call so silence + "Thank you for watching!" never becomes a
        // Delivery and never spends a reconciliation attempt.
        sources = sanitize_source_transcripts(sources);
        let sanitized_sources = sources.clone();
        sources.retain(|source| !source.text.is_empty());
        if sources.is_empty() {
            let validation_reason =
                "hallucinated suffix; no Source Transcript remains after stripping ASR outros"
                    .to_owned();
            return Err(
                BoundaryError::new(BoundaryKind::Validation, validation_reason.clone())
                    .with_transcript_failure(TranscriptFailureEvidence {
                        validation_reason,
                        fallback_reason: Some("hallucinated suffix".to_owned()),
                        reconciliation_requested: false,
                        recovery_attempted: false,
                        source_selection_diagnostic: source_selection_diagnostic(
                            &sanitized_sources,
                            None,
                            None,
                        ),
                    }),
            );
        }
        sources.sort_by_key(|source| source.provider);
        if let (Some(deepgram), Some(groq)) = (
            sources
                .iter()
                .find(|source| source.provider == Provider::Deepgram),
            sources
                .iter()
                .find(|source| source.provider == Provider::Groq),
        ) {
            let classification = source_pair_classification(&deepgram.text, &groq.text);
            if classification.is_near_identical() {
                let lexically_identical = classification.is_lexically_identical();
                // Word-for-word equal texts differ only in rendering, so the
                // three formatting signals decide them. Texts whose words
                // differ keep the Groq default, always: the difference may be a
                // mishearing or a change of meaning, and nothing available here
                // tells the two apart — English antonyms and negations are
                // minimal edits of the words they invert. A lost formatting
                // improvement is recoverable; text whose meaning is inverted is
                // not, because the user cannot see that it happened.
                let (winner, evidence) = if lexically_identical {
                    near_identical_selection(&deepgram.text, &groq.text, &self.dictionary_terms)
                } else {
                    (
                        GateWinner::Right,
                        "lexically different Source Transcripts; kept the Groq default because a difference in words may be a change of meaning".to_owned(),
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
                    source_selection_diagnostic: source_selection_diagnostic(
                        &sources,
                        Some(selected.provider),
                        Some(classification.confidence()),
                    ),
                    intent_reconstruction: None,
                    confidence_arbitration: None,
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
                        source_selection_diagnostic: source_selection_diagnostic(
                            &sources,
                            Some(winner.provider),
                            Some(gate.confidence),
                        ),
                        intent_reconstruction: None,
                        confidence_arbitration: None,
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
                        return safe_source_fallback(
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
                        return safe_source_fallback(
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
            // Reconcile success requires both a passed safety check and
            // source-derived vocabulary. A model may return short, guardrail-passing
            // meta/refusal text that trips no quality marker yet invents words
            // no provider heard — deliver that as Reconciled and the user is
            // typed a non-transcript. Fall back without Repair: Repair exists
            // for quality failures, not for non-source-derived invent.
            if !is_source_derived(&merge_result.0, &sources) {
                return safe_source_fallback(
                    &sources,
                    "reconciliation produced words absent from every Source Transcript".to_owned(),
                    true,
                    false,
                );
            }
            return Ok(TranscriptDecision {
                transcript: Transcript(merge_result.0.trim().to_owned()),
                selection: TranscriptSelection::Reconciled,
                validation_reason: "Merge Result passed validation".to_owned(),
                fallback_reason: None,
                reconciliation_requested: true,
                recovery_attempted: false,
                source_selection_diagnostic: source_selection_diagnostic(&sources, None, None),
                intent_reconstruction: None,
                confidence_arbitration: None,
            });
        }

        let source = sources
            .first()
            .ok_or_else(|| BoundaryError::new(BoundaryKind::Validation, "no Source Transcript"))?;
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
            source_selection_diagnostic: source_selection_diagnostic(
                &sources,
                Some(source.provider),
                Some(SourceSelectionConfidence::High),
            ),
            intent_reconstruction: None,
            confidence_arbitration: None,
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
                    return safe_source_fallback(
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
                    let _ =
                        tokio::time::timeout(RECONCILIATION_CLEANUP_GRACE, request.as_mut()).await;
                    return safe_source_fallback(
                        sources,
                        "recovery deadline elapsed".to_owned(),
                        reconciliation_requested,
                        true,
                    );
                }
            }
        };
        let failure = quality_failure_reason(&repaired.0, sources);
        if let Some(failure) = &failure
            && !failure.is_contraction()
        {
            return safe_source_fallback(
                sources,
                format!("recovery produced {}", failure.reason()),
                reconciliation_requested,
                true,
            );
        }
        if !is_source_derived(&repaired.0, sources) {
            return safe_source_fallback(
                sources,
                "recovery produced words no Source Transcript contains".to_owned(),
                reconciliation_requested,
                true,
            );
        }
        if let Some(failure) = failure {
            // The repair contracted past the merge floor. It is built out of
            // words the providers heard and otherwise passes guardrails, so it is still
            // the user's speech — but a complete Source Transcript carries more
            // of it, and preferring one is exactly what the floor is for. The
            // fallback follows the spec's contraction rule: the LONGER Source
            // Transcript, the user never receiving less than one provider
            // heard.
            //
            // The floor decides PREFERENCE, not delivery. When the offending
            // text was in both Source Transcripts, neither is safe and this
            // repair is all that is left of the Recording; refusing it there
            // was the round-1 P0. So a failure to find a safe source hands the
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
                    "repaired {reason}; delivered a contracted repair because neither Source Transcript is safe"
                ),
                fallback_reason: Some(failure.reason()),
                reconciliation_requested,
                recovery_attempted: true,
                source_selection_diagnostic: source_selection_diagnostic(sources, None, None),
                intent_reconstruction: None,
                confidence_arbitration: None,
            });
        }
        Ok(TranscriptDecision {
            transcript: Transcript(repaired.0.trim().to_owned()),
            selection: TranscriptSelection::Repaired,
            validation_reason: format!("repaired {reason}"),
            fallback_reason: None,
            reconciliation_requested,
            recovery_attempted: true,
            source_selection_diagnostic: source_selection_diagnostic(sources, None, None),
            intent_reconstruction: None,
            confidence_arbitration: None,
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
    let selected = match safe.as_slice() {
        [] => None,
        [only] => Some((
            *only,
            source_selection_diagnostic(
                sources,
                Some(only.provider),
                Some(SourceSelectionConfidence::High),
            ),
        )),
        [left, right, ..] => {
            let preference = complete_source_preference(left, right);
            Some((preference.source, preference.selection_diagnostic))
        }
    };
    let (source, selection_diagnostic) = selected.ok_or_else(|| {
        source_fallback_refusal(
            sources,
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
        validation_reason: "fuller safe Source Transcript delivered after a rejected contraction"
            .to_owned(),
        fallback_reason: Some(reason),
        reconciliation_requested,
        recovery_attempted,
        source_selection_diagnostic: selection_diagnostic,
        intent_reconstruction: None,
        confidence_arbitration: None,
    })
}

fn safe_source_fallback(
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
        [left, right, ..] => {
            let preference = complete_source_preference(left, right);
            if preference.material {
                return Ok(source_fallback_decision(
                    preference.source,
                    reason,
                    reconciliation_requested,
                    recovery_attempted,
                    preference.diagnostic,
                    preference.selection_diagnostic,
                ));
            }
            let (winner, evidence) = select_better_source(
                &normalized_words(&left.text),
                &normalized_words(&right.text),
            );
            let selected = match winner {
                GateWinner::Left => *left,
                GateWinner::Right => *right,
            };
            return Ok(source_fallback_decision(
                selected,
                reason,
                reconciliation_requested,
                recovery_attempted,
                "safe Source Transcript selected by existing evidence tiers".to_owned(),
                source_selection_diagnostic(
                    sources,
                    Some(selected.provider),
                    Some(evidence.confidence()),
                ),
            ));
        }
    };
    let source = source.ok_or_else(|| {
        source_fallback_refusal(
            sources,
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
        validation_reason: "safe Source Transcript selected by existing evidence tiers".to_owned(),
        fallback_reason: Some(reason),
        reconciliation_requested,
        recovery_attempted,
        source_selection_diagnostic: source_selection_diagnostic(
            sources,
            Some(source.provider),
            Some(SourceSelectionConfidence::High),
        ),
        intent_reconstruction: None,
        confidence_arbitration: None,
    })
}

const MAX_INTENT_RECONSTRUCTION_UTF8_BYTES: usize = 100_000;

fn intent_fallback(
    fallback: Result<TranscriptDecision, BoundaryError>,
    eligibility: IntentReconstructionEligibility,
    outcome: IntentReconstructionOutcome,
    reason: String,
    candidate: Option<String>,
) -> Result<TranscriptDecision, BoundaryError> {
    let mut decision = fallback?;
    decision.validation_reason = reason.clone();
    decision.fallback_reason = Some(reason);
    decision.reconciliation_requested = true;
    decision.intent_reconstruction = Some(IntentReconstructionEvidence {
        eligibility,
        outcome,
        candidate,
    });
    Ok(decision)
}

enum ModelCallFailure {
    Failed(BoundaryError),
    DeadlineElapsed,
}

async fn bounded_model_call(
    request: BoundaryFuture<'_, MergeResult>,
    cancel: Arc<CancelRegistry>,
    deadline: Duration,
) -> Result<MergeResult, ModelCallFailure> {
    tokio::pin!(request);
    match tokio::time::timeout(deadline, request.as_mut()).await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(error)) => Err(ModelCallFailure::Failed(error)),
        Err(_) => {
            cancel.cancel();
            let _ = tokio::time::timeout(RECONCILIATION_CLEANUP_GRACE, request.as_mut()).await;
            Err(ModelCallFailure::DeadlineElapsed)
        }
    }
}

fn source_fallback_decision(
    source: &SourceTranscript,
    reason: String,
    reconciliation_requested: bool,
    recovery_attempted: bool,
    validation_reason: String,
    source_selection_diagnostic: SourceSelectionDiagnostic,
) -> TranscriptDecision {
    TranscriptDecision {
        transcript: Transcript(source.text.trim().to_owned()),
        selection: match source.provider {
            Provider::Deepgram => TranscriptSelection::SourceDeepgram,
            Provider::Groq => TranscriptSelection::SourceGroq,
        },
        validation_reason,
        fallback_reason: Some(reason),
        reconciliation_requested,
        recovery_attempted,
        source_selection_diagnostic,
        intent_reconstruction: None,
        confidence_arbitration: None,
    }
}

/// Provisional boundary pinned by the checked-in fuller/near-equal/repetition
/// regressions. The private controlled corpus needed for calibration is not in
/// this checkout; that calibration remains a release gate rather than a claim
/// encoded here. This is stricter than the catastrophic-fragment floor because
/// completeness is a preference, not a claim that the shorter source is garbage.
const MATERIAL_COMPLETENESS_RATIO: f64 = 0.80;

struct CompleteSourcePreference<'a> {
    source: &'a SourceTranscript,
    material: bool,
    diagnostic: String,
    selection_diagnostic: SourceSelectionDiagnostic,
}

fn complete_source_preference<'a>(
    left: &'a SourceTranscript,
    right: &'a SourceTranscript,
) -> CompleteSourcePreference<'a> {
    let left_words = normalized_words(&left.text);
    let right_words = normalized_words(&right.text);
    let analysis_sources = [(*left).clone(), (*right).clone()];
    let coverage = source_coverage(&analysis_sources);
    let left_discount = coverage[0].repetition_discount;
    let right_discount = coverage[1].repetition_discount;
    let left_coverage = coverage[0].adjusted_coverage;
    let right_coverage = coverage[1].adjusted_coverage;
    let (source, fuller, shorter) = if left_coverage > right_coverage {
        (left, left_coverage, right_coverage)
    } else {
        (right, right_coverage, left_coverage)
    };
    let material = fuller > 0 && (shorter as f64 / fuller as f64) < MATERIAL_COMPLETENESS_RATIO;
    let diagnostic = format!(
        "materially fuller safe Source Transcript selected; raw words Deepgram={} Groq={}; adjusted coverage Deepgram={} Groq={}; repetition discount Deepgram={} Groq={}; selected provider={}; confidence high; safety passed",
        if left.provider == Provider::Deepgram {
            left_words.len()
        } else {
            right_words.len()
        },
        if left.provider == Provider::Groq {
            left_words.len()
        } else {
            right_words.len()
        },
        if left.provider == Provider::Deepgram {
            left_coverage
        } else {
            right_coverage
        },
        if left.provider == Provider::Groq {
            left_coverage
        } else {
            right_coverage
        },
        if left.provider == Provider::Deepgram {
            left_discount
        } else {
            right_discount
        },
        if left.provider == Provider::Groq {
            left_discount
        } else {
            right_discount
        },
        source.provider.cli_label(),
    );
    CompleteSourcePreference {
        source,
        material,
        diagnostic,
        selection_diagnostic: SourceSelectionDiagnostic {
            sources: coverage,
            selected_provider: Some(source.provider),
            confidence: Some(SourceSelectionConfidence::High),
        },
    }
}

fn repetition_discount(candidate: &[String], other: &[String]) -> usize {
    let mut filler_counts: HashMap<&str, usize> = HashMap::new();
    for word in candidate.iter().filter(|word| is_known_filler(word)) {
        *filler_counts.entry(word.as_str()).or_default() += 1;
    }
    let filler_discount: usize = filler_counts
        .values()
        .map(|count| count.saturating_sub(2))
        .sum();

    if surplus_is_self_repetition(candidate, other) {
        let mut budget: HashMap<&str, usize> = HashMap::new();
        for word in other {
            *budget.entry(word.as_str()).or_default() += 1;
        }
        let surplus = candidate
            .iter()
            .filter(|word| match budget.get_mut(word.as_str()) {
                Some(remaining) if *remaining > 0 => {
                    *remaining -= 1;
                    false
                }
                _ => true,
            })
            .count();
        filler_discount.max(surplus)
    } else {
        filler_discount
    }
}

fn is_known_filler(word: &str) -> bool {
    matches!(word, "um" | "uh" | "uhh" | "erm" | "hmm")
}

fn source_coverage(sources: &[SourceTranscript]) -> Vec<SourceCoverageRecord> {
    (0..sources.len())
        .map(|index| {
            let words = normalized_words(&sources[index].text);
            let other_words = sources
                .iter()
                .enumerate()
                .find(|(other_index, _)| *other_index != index)
                .map(|(_, source)| normalized_words(&source.text))
                .unwrap_or_default();
            let discount = repetition_discount(&words, &other_words);
            let safety_passed = !words.is_empty()
                && non_contraction_quality_failure_reason(
                    &sources[index].text,
                    &[SourceTranscript {
                        provider: sources[index].provider,
                        text: sources[index].text.clone(),
                    }],
                )
                .is_none();
            SourceCoverageRecord {
                provider: sources[index].provider,
                raw_words: words.len(),
                adjusted_coverage: words.len().saturating_sub(discount),
                repetition_discount: discount,
                safety_passed,
            }
        })
        .collect()
}

fn source_selection_diagnostic(
    sources: &[SourceTranscript],
    selected_provider: Option<Provider>,
    confidence: Option<SourceSelectionConfidence>,
) -> SourceSelectionDiagnostic {
    SourceSelectionDiagnostic {
        sources: source_coverage(sources),
        selected_provider,
        confidence,
    }
}

/// The Source Transcripts a fallback may deliver: individually safe under
/// the non-contraction guards AND carrying at least one normalised word.
///
/// The wordless exclusion is what keeps every fallback arm inside the
/// divergence gate's guarantees. A source that normalises to zero words
/// ("...", stray punctuation from silence) passes every text-shaped guard,
/// yet no word-level evidence can judge it — the gate never computes garbage
/// verdicts for such a pair, so a wordless side reaching a fallback
/// comparison can win it precisely by being unjudgeable, typing "..." into
/// the user's window as if the dictation succeeded. Pinned by
/// `a_wordless_source_transcript_is_never_delivered_over_heard_words` and
/// `an_unsafe_source_beside_a_wordless_sibling_is_refused_not_replaced_with_dots`.
fn quality_safe_sources<'a>(
    sources: impl IntoIterator<Item = &'a SourceTranscript>,
) -> Vec<&'a SourceTranscript> {
    sources
        .into_iter()
        .filter(|source| {
            !normalized_words(&source.text).is_empty()
                && non_contraction_quality_failure_reason(
                    &source.text,
                    std::slice::from_ref(source),
                )
                .is_none()
        })
        .collect()
}

fn source_fallback_refusal(
    sources: &[SourceTranscript],
    reason: &str,
    reconciliation_requested: bool,
    recovery_attempted: bool,
) -> BoundaryError {
    let validation_reason = format!("{reason}; neither Source Transcript is safe");
    BoundaryError::new(BoundaryKind::Validation, validation_reason.clone()).with_transcript_failure(
        TranscriptFailureEvidence {
            validation_reason,
            fallback_reason: Some(reason.to_owned()),
            reconciliation_requested,
            recovery_attempted,
            source_selection_diagnostic: source_selection_diagnostic(sources, None, None),
        },
    )
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
    confidence: SourceSelectionConfidence,
}

/// English function words plus common spoken fillers, excluded from content
/// density and content-count measurement. A word salad from context-free slices
/// is dominated by these; a real technical dictation is not.
const STOPWORDS: [&str; 94] = [
    "a", "an", "and", "or", "but", "of", "to", "in", "on", "at", "for", "with", "is", "are", "was",
    "were", "be", "been", "being", "am", "the", "this", "that", "these", "those", "it", "its",
    "as", "by", "from", "into", "onto", "over", "under", "out", "up", "down", "off", "so", "then",
    "than", "we", "you", "i", "he", "she", "they", "them", "our", "your", "my", "me", "his", "her",
    "their", "do", "does", "did", "not", "no", "yes", "if", "when", "while", "about", "before",
    "after", "near", "would", "could", "should", "will", "can", "um", "uh", "uhh", "yeah", "like",
    "just", "kind", "sort", "mean", "know", "well", "okay", "ok", "there", "here", "gonna",
    "wanna", "sorta", "kinda", "really", "actually",
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
/// safe-source fallback tie (never to decide gating). It deliberately does NOT
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
    let duplication =
        words.windows(2).filter(|pair| pair[0] == pair[1]).count() as f64 / total as f64;
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

/// The ticket-#201 classification shared by source selection and Intent
/// Reconstruction eligibility. Keeping the near-identical boundary and lexical
/// comparison in one typed result prevents those surfaces from drifting apart.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SourcePairClassification {
    NearIdentical { lexically_identical: bool },
    MaterialDisagreement,
}

impl SourcePairClassification {
    fn is_near_identical(self) -> bool {
        matches!(self, Self::NearIdentical { .. })
    }

    fn is_lexically_identical(self) -> bool {
        matches!(
            self,
            Self::NearIdentical {
                lexically_identical: true
            }
        )
    }

    fn confidence(self) -> SourceSelectionConfidence {
        match self {
            Self::NearIdentical {
                lexically_identical: true,
            } => SourceSelectionConfidence::High,
            Self::NearIdentical {
                lexically_identical: false,
            } => SourceSelectionConfidence::Low,
            Self::MaterialDisagreement => {
                unreachable!("material disagreement has no near-identical confidence")
            }
        }
    }

    fn diagnostic_confidence(self) -> Option<SourceSelectionConfidence> {
        self.is_near_identical().then(|| self.confidence())
    }

    fn intent_eligibility(self) -> Option<IntentReconstructionEligibility> {
        match self {
            Self::NearIdentical {
                lexically_identical: true,
            } => None,
            Self::NearIdentical {
                lexically_identical: false,
            } => Some(IntentReconstructionEligibility::LowConfidenceSelection),
            Self::MaterialDisagreement => {
                Some(IntentReconstructionEligibility::MaterialDisagreement)
            }
        }
    }
}

fn source_pair_classification(left: &str, right: &str) -> SourcePairClassification {
    if source_similarity(left, right) >= 0.85 {
        SourcePairClassification::NearIdentical {
            lexically_identical: normalized_words(left) == normalized_words(right),
        }
    } else {
        SourcePairClassification::MaterialDisagreement
    }
}

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
    fn confidence(self) -> SourceSelectionConfidence {
        if self.is_low_confidence() {
            SourceSelectionConfidence::Low
        } else {
            SourceSelectionConfidence::High
        }
    }
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
        .filter(|occurrences| occurrences.windows(2).any(|pair| pair[1] - pair[0] > 1))
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
/// Transcripts and select the better one (§3.4). The tiers do not share one
/// computation: the wordless guard and the fragment ratio below are counts
/// alone, and only tiers 1 and 3 consult the one-to-one phonetic-tolerant
/// matching of distinct content words (via `select_better_source`). No tier is
/// evidence about MEANING — nothing here, or anywhere in this module, can tell
/// a mishearing from a change of meaning, which is why the near-identical path
/// never lets any of it overturn the Groq default.
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
        if more == 0 {
            // Both wordless: nothing to select and nothing to divide by.
            // (`decide` never sends this pair — two wordless texts are
            // similarity 1.0 and take the near-identical path — the guard
            // keeps this function total over its own inputs.)
            return None;
        }
        // Exactly one side is wordless: punctuation from silence or noise.
        // It is not a transcript — no verdict below can judge a side with no
        // words, and a merge with it is a merge with a stub — so select the
        // only side that heard words before any tier that a wordless side
        // cannot take part in. Even degenerate filler is more of the
        // Recording than nothing. Pinned by
        // `a_wordless_source_transcript_is_never_delivered_over_heard_words`
        // and its companion tests.
        let winner = if left_words.is_empty() {
            GateWinner::Right
        } else {
            GateWinner::Left
        };
        return Some(QualityGate {
            winner,
            confidence: SourceSelectionConfidence::High,
            reason:
                "catastrophically divergent (one Source Transcript is wordless); selected the only Source Transcript with words"
                    .to_owned(),
        });
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
            confidence: SourceSelectionConfidence::High,
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
            confidence: SourceSelectionConfidence::High,
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
                    confidence: evidence.confidence(),
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
        if left_evidence.all_caps {
            " (all-caps)"
        } else {
            ""
        },
        right_evidence.capitalised_sentence_starts,
        right_evidence.sentence_starts,
        if right_evidence.all_caps {
            " (all-caps)"
        } else {
            ""
        },
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
    let alphabetic = text
        .chars()
        .filter(|character| character.is_alphabetic())
        .count();
    let lowercase = text
        .chars()
        .filter(|character| character.is_lowercase())
        .count();
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

/// The archetypal ASR outro hallucinations, learned from captioned video.
/// Matched only when anchored at the final sentence start or the text end —
/// the same words mid-sentence are ordinary dictation.
const HALLUCINATED_SUFFIXES: [&str; 5] = [
    "thank you for watching",
    "thanks for watching",
    "like and subscribe",
    "subtitles by",
    "transcribed by",
];

/// The text's final sentence — everything after the last sentence terminator
/// that ends a word — with leading non-alphanumerics trimmed. A '.' inside a
/// token ("amara.org", "otter.ai") does not end a sentence, so an outro
/// carrying a dotted attribution still reads as one final sentence; a '.'
/// swallowed by a closing quote does not either, which is why callers must
/// not rely on sentence boundaries alone to find a trailing artifact. A text
/// with no terminator is itself the final sentence.
fn final_sentence(text: &str) -> &str {
    let trimmed = text.trim_end_matches(|character: char| !character.is_alphanumeric());
    let start = final_sentence_content_start(trimmed);
    trimmed[start..].trim_start_matches(|character: char| !character.is_alphanumeric())
}

/// Byte offset in `body` where the final sentence's content begins, after any
/// leading non-alphanumerics that follow the preceding terminator. `body` must
/// already have trailing non-alphanumerics trimmed.
fn final_sentence_content_start(body: &str) -> usize {
    let after_terminator = body
        .char_indices()
        .filter(|&(index, character)| {
            matches!(character, '.' | '?' | '!')
                && body[index + character.len_utf8()..]
                    .chars()
                    .next()
                    .is_some_and(char::is_whitespace)
        })
        .map(|(index, character)| index + character.len_utf8())
        .next_back()
        .unwrap_or(0);
    body[after_terminator..]
        .char_indices()
        .find(|(_, character)| character.is_alphanumeric())
        .map(|(index, _)| after_terminator + index)
        .unwrap_or(after_terminator)
}

/// Offset just after the last sentence terminator that introduces the final
/// sentence, or `0` when the whole body is one sentence. Used when stripping
/// a final-sentence outro while keeping the preceding speech and its
/// terminator.
fn final_sentence_boundary_start(body: &str) -> usize {
    body.char_indices()
        .filter(|&(index, character)| {
            matches!(character, '.' | '?' | '!')
                && body[index + character.len_utf8()..]
                    .chars()
                    .next()
                    .is_some_and(char::is_whitespace)
        })
        .map(|(index, character)| index + character.len_utf8())
        .next_back()
        .unwrap_or(0)
}

fn has_anchored_hallucinated_outro(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_lowercase();
    let tail = lower.trim_end_matches(|character: char| !character.is_alphanumeric());
    let outro = final_sentence(&lower);
    HALLUCINATED_SUFFIXES
        .iter()
        .any(|suffix| outro.starts_with(suffix) || tail.ends_with(suffix))
}

/// Remove one anchored hallucinated outro from the tail of `text`.
/// Returns `None` when no anchored outro is present.
fn strip_one_anchored_hallucinated_outro(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let alnum_end = trimmed
        .char_indices()
        .rev()
        .find(|(_, character)| character.is_alphanumeric())
        .map(|(index, character)| index + character.len_utf8())
        .unwrap_or(0);
    let body = &trimmed[..alnum_end];
    if body.is_empty() {
        return None;
    }
    let content_start = final_sentence_content_start(body);
    let final_content = &body[content_start..];
    let final_lower = final_content.to_lowercase();
    if HALLUCINATED_SUFFIXES
        .iter()
        .any(|suffix| final_lower.starts_with(suffix))
    {
        let boundary = final_sentence_boundary_start(body);
        return Some(body[..boundary].trim_end().to_owned());
    }
    let body_lower = body.to_lowercase();
    for suffix in HALLUCINATED_SUFFIXES {
        if !body_lower.ends_with(suffix) {
            continue;
        }
        // Suffixes are ASCII; strip by char count so non-ASCII heads stay intact.
        let suffix_chars = suffix.chars().count();
        let body_chars = body.chars().count();
        if body_chars < suffix_chars {
            continue;
        }
        let keep: String = body.chars().take(body_chars - suffix_chars).collect();
        return Some(keep.trim_end().to_owned());
    }
    None
}

/// Politeness / interjection heads that ride in front of ASR outros and are not
/// standalone speech once the outro is gone. Distinct from `STOPWORDS` so short
/// real dictation like "Done" is not cleared, while "Please" is.
const TRIVIAL_OUTRO_PREFIXES: [&str; 9] = [
    "please", "thanks", "thank", "hi", "hello", "hey", "bye", "alright", "oh",
];

/// True when post-strip remainder is real speech rather than a stopword-only or
/// trivial head left glued to a hallucinated outro.
fn remainder_is_substantial_speech(text: &str) -> bool {
    let words = normalized_words(text);
    if words.is_empty() {
        return false;
    }
    let has_real_content = words
        .iter()
        .any(|word| !is_stopword(word) && !TRIVIAL_OUTRO_PREFIXES.contains(&word.as_str()));
    if has_real_content {
        return true;
    }
    // All stopwords / trivial prefixes: keep multi-word utterances such as
    // "yes i can do that" and clear one- or two-token heads ("ok", "yeah",
    // "please").
    words.len() >= 3
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
            text.match_indices(term)
                .filter_map(move |(start, matched)| {
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
            || selected_spans.iter().any(|(selected_start, selected_end)| {
                start < *selected_end && *selected_start < end
            })
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
/// A candidate with NO content words cannot use that exemption — an empty
/// content set vacuously passing is how "I can't do that." (pure stopwords
/// once "can't" expands to "can not") was once typed as the user's dictation.
/// But a real dictation can be all stopwords too ("Yes, I can do that."), and
/// refusing it would lose a dictation to a guard. So an all-stopword
/// candidate is held to the strictest form of the same question: EVERY word,
/// stopwords included, must be one some provider actually heard. The user's
/// own words survive that; a refusal's vocabulary, against sources that never
/// said "i"/"can"/"do"/"that", does not.
fn is_source_derived(candidate: &str, sources: &[SourceTranscript]) -> bool {
    let source_words: Vec<String> = sources
        .iter()
        .flat_map(|source| normalized_words(&source.text))
        .collect();
    let candidate_words = normalized_words(candidate);
    let content_words: Vec<&String> = candidate_words
        .iter()
        .filter(|word| !is_stopword(word))
        .collect();
    if content_words.is_empty() {
        let heard: HashSet<&str> = source_words.iter().map(String::as_str).collect();
        return !candidate_words.is_empty()
            && candidate_words
                .iter()
                .all(|word| heard.contains(word.as_str()));
    }
    let vocabulary = distinct_content_words(&source_words);
    content_words
        .into_iter()
        .all(|word| vocabulary.contains(word.as_str()))
}

fn quality_failure_reason(candidate: &str, sources: &[SourceTranscript]) -> Option<QualityFailure> {
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
    // All five are appended tail artifacts, so they count when they begin the
    // text's FINAL sentence or end the text. Both anchors are needed: the
    // sentence anchor alone misses a provider that omits punctuation entirely
    // (spec §1 records Groq at zero punctuation, leaving the outro with no
    // sentence of its own) and a period swallowed by a closing quote, while
    // the end anchor alone misses an outro whose attribution or second outro
    // sentence follows it. The anchors stay at the tail deliberately: the
    // same words mid-sentence are ordinary dictation ("...and the recording
    // was transcribed by Whisper."), and a false positive routes real speech
    // into repair — the one path allowed to refuse delivery — so a missed
    // outro (visible junk the user can delete) is the cheaper direction.
    // Each anchored placement and the mid-sentence exemption is pinned by a
    // test. Source selection strips the same anchors via
    // `sanitize_source_transcript_text` before preferring a provider.
    if has_anchored_hallucinated_outro(trimmed) {
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
                0x0600..=0x06ff => 3,                   // Arabic
                0x0900..=0x097f => 4,                   // Devanagari
                0x3040..=0x30ff | 0x3400..=0x9fff => 5, // Japanese/CJK
                _ => 6,
            },
        };
        scripts[index] = true;
    }
    scripts.into_iter().filter(|present| *present).count()
}

/// The normalized word sequence of a text: whitespace tokens stripped to
/// alphanumeric content, case-folded, with contractions expanded. The SAME
/// per-token normalization [`normalize_token`] applies, so provider word
/// streams and texts normalize positionally against each other.
fn normalized_words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .flat_map(normalize_token)
        .filter(|word| !word.is_empty())
        .collect()
}

/// Normalizes ONE whitespace token the way [`normalized_words`] does:
/// punctuation dropped (curly apostrophes folded to straight ones),
/// lowercased, contractions expanded into their parts. A punctuation-only
/// token normalizes to no words.
pub(crate) fn normalize_token(word: &str) -> Vec<String> {
    let word = word
        .chars()
        .filter_map(|character| match character {
            '\u{2019}' => Some('\''),
            character if character.is_alphanumeric() || character == '\'' => Some(character),
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

/// The Recording Deadline clock a capture is enforcing: the instant it began
/// and the Deadline it resolved for itself. Read once at start so the observer
/// path can report headroom against the SAME clock that will stop the
/// Recording — anything stamped later (provider start can block on the
/// keyring) would report headroom the enforcer does not agree with.
///
/// Reporting only. Nothing outside the capture may enforce, shorten, or
/// second-guess this pair.
#[derive(Clone, Copy, Debug)]
pub struct DeadlineClock {
    pub started: Instant,
    pub deadline: Duration,
}

impl DeadlineClock {
    /// Headroom left at `now`, saturating: a Recording already past its
    /// Deadline (the stop is in flight) has none rather than wrapping.
    pub fn remaining(&self, now: Instant) -> Duration {
        self.deadline
            .saturating_sub(now.saturating_duration_since(self.started))
    }
}

pub trait ActiveCapture: Send {
    /// The Deadline clock this capture will enforce. The capture is the only
    /// owner of that truth, so it is the only thing allowed to answer.
    fn deadline_clock(&self) -> DeadlineClock;
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

    /// Word-level confidence evidence the stream retained for its finalized
    /// segments, as `(word, confidence)` pairs in transcript order. Providers
    /// that do not expose word confidences keep the default: none. Slice B2's
    /// user-vocabulary correction gate consumes this signal; since slice B4
    /// the decision pipeline also uses it (per provider) to arbitrate
    /// divergence points between the two Source Transcripts — Deepgram from
    /// its streaming finals, Groq from the verbose_json word timestamps.
    fn word_confidences(&self) -> Vec<(String, f64)> {
        Vec::new()
    }
}

/// Word-level confidence evidence from one Provider's streaming transcript.
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderWordConfidences {
    pub provider: Provider,
    /// `(word, confidence)` pairs in transcript order. A word whose provider
    /// did not report a confidence is carried with `0.0` so the correction
    /// gate treats it as unproven rather than confident.
    pub words: Vec<(String, f64)>,
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
    /// Word-level confidence evidence per provider that retained it (today:
    /// Deepgram's streaming finals). Absent from providers without word
    /// evidence. Consumed by the validation pipeline's user-vocabulary
    /// correction gate.
    pub word_confidences: Vec<ProviderWordConfidences>,
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

        // Word-confidence evidence is pulled BEFORE the deadline path moves the
        // streams into abort: the Deepgram accumulator is settled once its
        // completion future resolved, and Groq's chunk word confidences are
        // stored on its stream by its own completion — a stream that never
        // completed has no Source Transcript for the gate to apply to anyway,
        // and reports none.
        let deepgram_word_confidences = deepgram.word_confidences();
        let groq_word_confidences = groq.word_confidences();

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
                word_confidences: {
                    // One provider-tagged evidence entry per provider that
                    // retained words, in Provider order (Deepgram, Groq).
                    let mut word_confidences = Vec::new();
                    if !deepgram_word_confidences.is_empty() {
                        word_confidences.push(ProviderWordConfidences {
                            provider: Provider::Deepgram,
                            words: deepgram_word_confidences,
                        });
                    }
                    if !groq_word_confidences.is_empty() {
                        word_confidences.push(ProviderWordConfidences {
                            provider: Provider::Groq,
                            words: groq_word_confidences,
                        });
                    }
                    word_confidences
                },
            })
        }
    }
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

pub trait TranscriptValidator: Send {
    fn set_dictionary_terms(&mut self, _dictionary_terms: Vec<String>) {}

    /// Sets the user's personal dictionary terms for the constrained
    /// post-correction pass. Default: no user vocabulary, so corrections are
    /// disabled and pipeline output is byte-identical.
    fn set_user_vocabulary(&mut self, _user_vocabulary: Vec<String>) {}

    /// Sets the word-level confidence evidence for this Recording's provider
    /// transcripts. Default: none, which leaves every user-vocabulary
    /// substitution ungated (the documented asymmetry).
    fn set_word_confidences(&mut self, _word_confidences: Vec<ProviderWordConfidences>) {}

    fn validate(
        &mut self,
        sources: Vec<SourceTranscript>,
    ) -> BoundaryFuture<'_, TranscriptDecision>;

    fn prepare(
        &mut self,
        sources: Vec<SourceTranscript>,
    ) -> BoundaryFuture<'_, PreparedTranscriptDecision> {
        Box::pin(async move {
            self.validate(sources)
                .await
                .map(PreparedTranscriptDecision::Ready)
        })
    }

    fn reconstruct(
        &mut self,
        attempt: IntentReconstructionAttempt,
    ) -> BoundaryFuture<'_, TranscriptDecision> {
        Box::pin(async move { attempt.fallback })
    }
}

impl<M: ReconciliationModel> TranscriptValidator for TranscriptDecisionPipeline<M> {
    fn set_dictionary_terms(&mut self, dictionary_terms: Vec<String>) {
        TranscriptDecisionPipeline::set_dictionary_terms(self, dictionary_terms);
    }

    fn set_user_vocabulary(&mut self, user_vocabulary: Vec<String>) {
        TranscriptDecisionPipeline::set_user_vocabulary(self, user_vocabulary);
    }

    fn set_word_confidences(&mut self, word_confidences: Vec<ProviderWordConfidences>) {
        TranscriptDecisionPipeline::set_word_confidences(self, word_confidences);
    }

    fn validate(
        &mut self,
        sources: Vec<SourceTranscript>,
    ) -> BoundaryFuture<'_, TranscriptDecision> {
        Box::pin(async move { self.decide(sources).await })
    }

    fn prepare(
        &mut self,
        sources: Vec<SourceTranscript>,
    ) -> BoundaryFuture<'_, PreparedTranscriptDecision> {
        Box::pin(async move { TranscriptDecisionPipeline::prepare(self, sources).await })
    }

    fn reconstruct(
        &mut self,
        attempt: IntentReconstructionAttempt,
    ) -> BoundaryFuture<'_, TranscriptDecision> {
        Box::pin(async move { TranscriptDecisionPipeline::reconstruct(self, attempt).await })
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

#[cfg(test)]
mod replay_evidence_additivity_tests {
    use super::*;

    // The replay evidence texts are ADDITIVE: an older daemon's response —
    // with no `source_transcripts` and no `final_transcript` — must keep
    // deserializing, and an evidence without them must serialize without the
    // keys, so pre-change clients and readers stay byte-compatible in both
    // skew directions.
    #[test]
    fn evidence_without_replay_texts_round_trips_without_them() {
        let old_json = r#"{
            "recording_id": 7,
            "correlation_id": "rec-4242-7-1700000000000",
            "stages": ["providers_completed", "validation_completed"],
            "delivery_count": 0,
            "streamed_chunk_count": 0,
            "source_transcript_providers": ["deepgram", "groq"],
            "provider_timings_ms": []
        }"#;
        let evidence: LifecycleEvidence =
            serde_json::from_str(old_json).expect("a pre-replay-texts response must deserialize");
        assert!(evidence.source_transcripts.is_empty());
        assert!(evidence.final_transcript.is_none());
        let reencoded = serde_json::to_value(&evidence).unwrap();
        assert!(
            reencoded.get("source_transcripts").is_none(),
            "empty replay texts must not appear on the wire: {reencoded}"
        );
        assert!(
            reencoded.get("final_transcript").is_none(),
            "an absent final transcript must not appear on the wire: {reencoded}"
        );
    }

    #[test]
    fn a_replay_response_evidence_carries_provider_tagged_texts_and_the_final() {
        let evidence = LifecycleEvidence {
            recording_id: 1,
            correlation_id: "rec-1-1-1".to_owned(),
            stages: vec![LifecycleStage::ProvidersCompleted],
            delivery_count: 0,
            delivery_method: None,
            delivery_fallback_reason: None,
            streamed_chunk_count: 0,
            source_transcript_providers: vec![Provider::Deepgram, Provider::Groq],
            first_chunk_ms: None,
            capture_finalized_ms: None,
            truncated_by: None,
            provider_timings_ms: vec![
                ProviderTiming {
                    provider: Provider::Deepgram,
                    completed_ms: 4,
                },
                ProviderTiming {
                    provider: Provider::Groq,
                    completed_ms: 6,
                },
            ],
            provider_failures: Vec::new(),
            release_to_text_ms: None,
            recording_duration_ms: None,
            stop_to_finalized_ms: None,
            stop_to_delivered_ms: None,
            transcript_selection: Some(TranscriptSelection::SourceDeepgram),
            validation_reason: None,
            fallback_reason: None,
            reconciliation_requested: false,
            recovery_attempted: false,
            source_selection_diagnostic: None,
            intent_reconstruction: None,
            confidence_arbitration: None,
            source_transcripts: vec![
                SourceTranscriptRecord {
                    provider: Provider::Deepgram,
                    text: "Replay this dictation.".to_owned(),
                },
                SourceTranscriptRecord {
                    provider: Provider::Groq,
                    text: "Replay this dictation".to_owned(),
                },
            ],
            final_transcript: Some("Replay this dictation.".to_owned()),
        };
        let encoded = serde_json::to_value(&evidence).unwrap();
        assert_eq!(
            encoded["source_transcripts"]
                .as_array()
                .expect("source transcripts serialize")
                .iter()
                .map(|source| (
                    source["provider"].as_str().unwrap(),
                    source["text"].as_str().unwrap()
                ))
                .collect::<Vec<_>>(),
            vec![
                ("deepgram", "Replay this dictation."),
                ("groq", "Replay this dictation")
            ]
        );
        assert_eq!(encoded["final_transcript"], "Replay this dictation.");
        // And it reads back through the same type, so a --json consumer can
        // round-trip the shape.
        let decoded: LifecycleEvidence =
            serde_json::from_value(encoded).expect("replay evidence round-trips");
        assert_eq!(
            decoded.final_transcript.as_deref(),
            Some("Replay this dictation.")
        );
        assert_eq!(decoded.source_transcripts.len(), 2);
    }
}
