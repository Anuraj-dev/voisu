//! Correlated, bounded, local diagnostics for a single Recording.
//!
//! One correlation ID joins every event of a Recording — capture, streamed
//! chunks, provider completion, reconciliation, validation, Delivery, and any
//! error. History is retained locally under a configured retention policy and is
//! never uploaded: no function here performs any network egress. Diagnostic
//! export redacts credentials, authorization headers, secret identifiers, and
//! unrelated environment values. Raw audio is absent from a record unless the
//! user explicitly enables debug capture, and debug audio records its expiry so
//! cleanup can remove expired captures safely.

use std::collections::BTreeMap;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    BoundaryError, CaptureLimit, CapturedAudio, DeliveryMethod, DprDiagnostic, LifecycleStage,
    PreparedTranscriptDecision, Provider, ProviderCoordinator, ProviderFailure, ProviderTiming,
    SourceTranscript, TranscriptDecision, TranscriptSelection, TranscriptValidator,
};

/// A stored transcript text is clamped so a bounded history never grows without
/// limit and always fits a single IPC response frame. Dictation transcripts are
/// far shorter than this bound.
pub const MAX_STORED_TEXT: usize = 8 * 1024;

/// The default number of most-recent Recordings retained in local history.
///
/// Both retention bounds are sized for a week of dictation, and they only work
/// together: at the observed ~36 Recordings a day the age bound prunes first, so
/// a larger count alone would leave the ring plateaued near a single day's
/// worth. Sized so a week at that rate (~252) is bounded but a week at a lighter
/// rate is retained whole.
pub const DEFAULT_MAX_RECORDS: usize = 200;
/// The default maximum age of a retained diagnostic record — one week, so this
/// week's latency can be compared against last week's. Raised in step with
/// [`DEFAULT_MAX_RECORDS`]; see that constant for why neither moves alone.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
/// The default time-to-live for an explicitly captured debug audio file.
pub const DEFAULT_DEBUG_AUDIO_TTL: Duration = Duration::from_secs(3600);

/// The masking placeholder a diagnostic export writes in place of any secret.
pub const REDACTED: &str = "<redacted>";

/// Schema version stamped on every [`SmartWritingDiagnostic`] record.
pub const SMART_WRITING_DIAGNOSTIC_VERSION: u32 = 1;

/// Maximum UTF-8 bytes of Validated-before / Rendered-after text retained in a
/// Smart Writing diagnostic. Clamping is diagnostics-only — Delivery always uses
/// the full text. Spec §10 / constants JSON.
pub const MAX_SMART_WRITING_DIAGNOSTIC_TEXT_UTF8_BYTES: usize = 2048;

/// Maximum structured edit evidence entries retained on one Smart Writing
/// diagnostic. Spec §10 / constants JSON.
pub const MAX_SMART_WRITING_DIAGNOSTIC_EDITS: usize = 32;

/// Maximum UTF-8 bytes of the Minimal Grammar model ID retained in diagnostics.
/// Spec §10 / constants JSON.
pub const MAX_MODEL_ID_UTF8_BYTES: usize = 128;

/// Maximum UTF-8 bytes of any free-form Smart Writing diagnostic string
/// (errors, edit identifiers, rule IDs, edit before/after snippets). Spec §10
/// retains the 128-byte diagnostic limit from #100.
pub const MAX_SMART_WRITING_FREE_TEXT_UTF8_BYTES: usize = 128;

/// Maximum UTF-8 bytes of an edit before/after field in diagnostic evidence.
/// Matches #100 `MAX_GRAMMAR_EDIT_FIELD_UTF8_BYTES`.
pub const MAX_SMART_WRITING_EDIT_FIELD_UTF8_BYTES: usize = 256;

/// The retained history: one JSON record per line, in append order.
///
/// Line-delimited rather than a single JSON array so a completed Recording can
/// append its record without rewriting the ring. At the default retention on a
/// real disk that rewrite costs ~250 ms per dictation, all of it proportional to
/// the retained history rather than to the one record being added. It also
/// bounds crash damage: a torn final line loses one record, where a torn single
/// JSON value is unparseable and loses everything.
const HISTORY_FILE: &str = "history.jsonl";
/// Prefix of the temp files [`DiagnosticStore::write_all`] renames into place.
/// Startup sweeps these, so it has to stay in step with the history file name.
const HISTORY_TEMP_PREFIX: &str = "history.jsonl.tmp.";

/// Milliseconds since the Unix epoch, saturating to 0 before the epoch.
pub fn unix_millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Generates the correlation ID that joins every event of one Recording. It is
/// unique per daemon process (pid plus a monotonic counter) so records from
/// different daemon runs never collide, and it carries the `recording_id` so a
/// user can tie the ID back to the lifecycle they observed.
pub fn correlation_id(recording_id: u64) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "rec-{}-{}-{}",
        std::process::id(),
        recording_id,
        unix_millis_now().wrapping_add(sequence)
    )
}

fn clamp_text(text: String) -> String {
    if text.len() <= MAX_STORED_TEXT {
        return text;
    }
    let mut boundary = MAX_STORED_TEXT;
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let mut clamped = text[..boundary].to_owned();
    clamped.push('…');
    clamped
}

/// Clamps `text` to at most `max_bytes` UTF-8 bytes on a char boundary. Unlike
/// [`clamp_text`], no ellipsis is appended — Smart Writing diagnostic clamps are
/// exact byte budgets that fingerprints of the unclamped source cover for
/// equality.
pub fn clamp_utf8_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut boundary = max_bytes;
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    text[..boundary].to_owned()
}

/// Exact wire length of a text fingerprint: `sha256:` (7) + 64 lowercase hex.
pub const TEXT_SHA256_FINGERPRINT_LEN: usize = 7 + 64;

/// Full SHA-256 fingerprint of `text` as `sha256:` + 64 lowercase hex digits.
/// Used so Validated/Rendered equality remains inspectable after the diagnostic
/// text clamp drops tail bytes.
pub fn text_sha256_fingerprint(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    let mut out = String::with_capacity(TEXT_SHA256_FINGERPRINT_LEN);
    out.push_str("sha256:");
    for byte in digest {
        out.push(HEX_LOWER[(byte >> 4) as usize] as char);
        out.push(HEX_LOWER[(byte & 0x0f) as usize] as char);
    }
    debug_assert!(is_text_sha256_fingerprint(&out));
    out
}

/// Returns true when `value` is exactly `sha256:` followed by 64 lowercase hex
/// digits. Anything else is free-form (possibly secret-bearing or unbounded)
/// and must not be treated as a fingerprint.
pub fn is_text_sha256_fingerprint(value: &str) -> bool {
    if value.len() != TEXT_SHA256_FINGERPRINT_LEN {
        return false;
    }
    let Some(hex) = value.strip_prefix("sha256:") else {
        return false;
    };
    hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// When `value` is not exact `sha256:` + 64 lowercase hex, replaces it with a
/// fingerprint of `source_text` so free-form or secret-bearing mutation cannot
/// leave non-fingerprint content in a fingerprint field.
fn sanitize_text_sha256_fingerprint(value: &mut String, source_text: &str) {
    if !is_text_sha256_fingerprint(value) {
        *value = text_sha256_fingerprint(source_text);
    }
}

const HEX_LOWER: &[u8; 16] = b"0123456789abcdef";

/// A Source Transcript as retained in local history, with its provider so a
/// reader can attribute the text.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct SourceTranscriptRecord {
    pub provider: Provider,
    pub text: String,
}

impl SourceTranscriptRecord {
    pub fn new(source: &SourceTranscript) -> Self {
        Self {
            provider: source.provider,
            text: clamp_text(source.text.clone()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SourceCoverageRecord {
    pub provider: Provider,
    pub raw_words: usize,
    pub adjusted_coverage: usize,
    pub repetition_discount: usize,
    pub safety_passed: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceSelectionConfidence {
    High,
    Low,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SourceSelectionDiagnostic {
    pub sources: Vec<SourceCoverageRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_provider: Option<Provider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<SourceSelectionConfidence>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentReconstructionEligibility {
    MaterialDisagreement,
    LowConfidenceSelection,
    NearIdenticalHighConfidence,
    SingleSource,
    RepairPath,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentReconstructionOutcome {
    Accepted,
    Rejected,
    Failed,
    Deadline,
    Skipped,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct IntentReconstructionDiagnostic {
    pub model: String,
    pub eligibility: IntentReconstructionEligibility,
    pub outcome: IntentReconstructionOutcome,
    pub elapsed_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<String>,
}

impl IntentReconstructionDiagnostic {
    fn normalize(&mut self) {
        self.model = clamp_utf8_bytes(&self.model, MAX_MODEL_ID_UTF8_BYTES);
        self.candidate = self
            .candidate
            .take()
            .map(|candidate| clamp_utf8_bytes(&candidate, MAX_STORED_TEXT));
    }
}

/// The recorded location and expiry of an explicitly captured debug audio file.
/// Its presence is the only way raw audio is retained; without debug capture it
/// is `None`. Only a validated basename is stored — never an arbitrary path — so
/// cleanup can never be steered outside the store's private audio directory by
/// a tampered history file. The expiry is also encoded in the file name itself,
/// so a capture orphaned by a crash before its record persisted still expires.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct DebugAudioRecord {
    pub file_name: String,
    pub captured_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
}

impl DebugAudioRecord {
    pub fn is_expired(&self, now_ms: u64) -> bool {
        now_ms >= self.expires_at_unix_ms
    }
}

/// Writing Mode snapshotted for the Recording that produced this diagnostic.
///
/// Distinct from the app config type so `voisu-core` stays free of config I/O
/// while still recording the exact §10 mode that governed the path.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SmartWritingMode {
    Smart,
    Literal,
}

/// Resolved EnglishEligibility for the Recording — never inferred from words.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnglishEligibilityOutcome {
    Eligible,
    Ineligible,
}

/// Exactly one Smart Writing path outcome per Recording. Spec §10 closed set.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SmartWritingOutcome {
    /// Literal mode, success, no explicit command applied (ordinary identity).
    Literal,
    /// Literal mode, success, at least one explicit formatting command rendered.
    LiteralCommands,
    /// Literal path failed closed to Validated identity (formatter panic/deadline/
    /// oversize/atomic command render failure), whether or not a command was
    /// recognized.
    LiteralFallback,
    /// Smart local Formatting baseline delivered without accepted grammar
    /// (includes command-present Smart runs that skip grammar under §6.1).
    FormattingOnly,
    /// Smart baseline plus accepted grammar edits.
    FormattingAndGrammar,
    /// **Smart only**: failed closed to Validated identity (formatter miss/panic/
    /// oversize). Never used for Literal — use [`Literal`](Self::Literal) or
    /// [`LiteralFallback`](Self::LiteralFallback) instead.
    IdentityFallback,
}

/// Closed rejection / fallback / edit disposition codes for Smart Writing
/// diagnostics. Prefer these over free-form strings. Spec §10.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SmartWritingReasonCode {
    /// Path governed by Literal Writing Mode (no grammar).
    ModeLiteral,
    /// Path governed by Smart Writing Mode.
    ModeSmart,
    /// EnglishEligibility resolved ineligible — grammar not attempted.
    EnglishIneligible,
    /// GrammarCapability was Unavailable before the gate.
    CapabilityUnavailable,
    /// Validated Transcript or other input exceeded a configured bound.
    InputOversize,
    /// Provider response body exceeded a configured bound.
    ResponseOversize,
    /// Grammar HTTP request hit its deadline.
    HttpTimeout,
    /// Grammar HTTP returned a non-success status.
    HttpStatus,
    /// Grammar HTTP transport/connect failure.
    HttpTransport,
    /// Envelope or edit shape was unreadable / malformed.
    Malformed,
    /// Response failed strict schema validation.
    Schema,
    /// Grammar base identity was stale relative to the Validated Transcript.
    Stale,
    /// An edit intersected a protected span.
    ProtectedSpan,
    /// A closed-rule predicate did not match its narrow context.
    RuleContext,
    /// Formatter supplied no unambiguous source anchor for composition.
    Unmappable,
    /// Two grammar edits overlapped or duplicated a source range.
    Overlap,
    /// Local formatter panicked.
    FormatterPanic,
    /// Local formatter exceeded its work deadline.
    FormatterDeadline,
    /// Safety / composition validation panicked.
    SafetyPanic,
    /// Safety / composition exceeded its work deadline.
    SafetyDeadline,
    /// Anchor composition panicked.
    ComposePanic,
    /// Anchor composition exceeded its work deadline.
    ComposeDeadline,
    /// Credential reaper crossed the 2 s watchdog while still non-terminal.
    CleanupOverrun,
    /// Edit accepted and applied to the baseline.
    EditAccepted,
    /// Unknown / out-of-catalog grammar rule ID.
    UnknownRule,
}

/// One structured edit evidence entry retained on a Smart Writing diagnostic.
/// Never carries prompts or raw provider bodies.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SmartWritingEditEvidence {
    /// Bounded diagnostic edit identifier from the provider envelope.
    pub edit_id: String,
    /// Closed grammar rule ID (e.g. `G_DIDNT_APOSTROPHE`).
    pub rule_id: String,
    /// Half-open UTF-8 byte start against the Validated Transcript.
    pub start_utf8: u64,
    /// Half-open UTF-8 byte end against the Validated Transcript.
    pub end_utf8: u64,
    /// Bounded before snippet at the edit range.
    pub before: String,
    /// Bounded after snippet proposed or applied.
    pub after: String,
    /// Acceptance or rejection code for this edit.
    pub code: SmartWritingReasonCode,
}

impl SmartWritingEditEvidence {
    /// Builds edit evidence with clamped free-form fields.
    pub fn new(
        edit_id: impl Into<String>,
        rule_id: impl Into<String>,
        start_utf8: u64,
        end_utf8: u64,
        before: impl AsRef<str>,
        after: impl AsRef<str>,
        code: SmartWritingReasonCode,
    ) -> Self {
        let mut evidence = Self {
            edit_id: edit_id.into(),
            rule_id: rule_id.into(),
            start_utf8,
            end_utf8,
            before: before.as_ref().to_owned(),
            after: after.as_ref().to_owned(),
            code,
        };
        evidence.normalize();
        evidence
    }

    /// Re-applies §10 field clamps. Fields are publicly mutable after
    /// construction, and export scrubbing can expand a string past its budget
    /// when a short secret is replaced with [`REDACTED`]; call this at every
    /// persistence and export boundary so the byte budgets stay invariants.
    pub fn normalize(&mut self) {
        self.edit_id = clamp_utf8_bytes(&self.edit_id, MAX_SMART_WRITING_FREE_TEXT_UTF8_BYTES);
        self.rule_id = clamp_utf8_bytes(&self.rule_id, MAX_SMART_WRITING_FREE_TEXT_UTF8_BYTES);
        self.before = clamp_utf8_bytes(&self.before, MAX_SMART_WRITING_EDIT_FIELD_UTF8_BYTES);
        self.after = clamp_utf8_bytes(&self.after, MAX_SMART_WRITING_EDIT_FIELD_UTF8_BYTES);
    }
}

/// Optional, versioned Smart Writing diagnostic evidence for one Recording.
///
/// Absent on pre-SW records (serde default). Never stores credentials, prompts,
/// raw bodies, app/window/screen/clipboard context, or newly captured audio.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SmartWritingDiagnostic {
    /// Schema version. Current: [`SMART_WRITING_DIAGNOSTIC_VERSION`].
    #[serde(default = "default_smart_writing_diagnostic_version")]
    pub version: u32,
    /// Writing Mode that governed the path for this Recording.
    pub writing_mode: SmartWritingMode,
    /// EnglishEligibility resolution for this Recording.
    pub english_eligibility: EnglishEligibilityOutcome,
    /// Formatter contract ID sealed into the FormattingBaseline.
    pub formatter_contract_id: String,
    /// Bounded Validated-before text (≤ 2,048 UTF-8 bytes). Full equality uses
    /// [`validated_before_sha256`](Self::validated_before_sha256).
    pub validated_before: String,
    /// Full SHA-256 fingerprint of the unclamped Validated Transcript.
    pub validated_before_sha256: String,
    /// Bounded Rendered-after text (≤ 2,048 UTF-8 bytes). Full equality uses
    /// [`rendered_after_sha256`](Self::rendered_after_sha256).
    pub rendered_after: String,
    /// Full SHA-256 fingerprint of the unclamped Rendered Transcript.
    pub rendered_after_sha256: String,
    /// Exactly one path outcome.
    pub outcome: SmartWritingOutcome,
    /// Up to [`MAX_SMART_WRITING_DIAGNOSTIC_EDITS`] structured edit evidence
    /// entries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edits: Vec<SmartWritingEditEvidence>,
    /// Exact Minimal Grammar model ID when a request was considered, clamped to
    /// [`MAX_MODEL_ID_UTF8_BYTES`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    /// Whether a Minimal Grammar HTTP request began.
    #[serde(default)]
    pub request_began: bool,
    /// Local formatter work latency in milliseconds, when measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub formatter_latency_ms: Option<u64>,
    /// Grammar HTTP latency in milliseconds, when a request began.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_latency_ms: Option<u64>,
    /// Safety / composition latency in milliseconds, when measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safety_latency_ms: Option<u64>,
    /// Total final-transform gate latency in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_gate_latency_ms: Option<u64>,
    /// Credential prep latency in milliseconds, when prep ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_prep_latency_ms: Option<u64>,
    /// Whether the 2 s credential reap watchdog was crossed.
    #[serde(default)]
    pub reap_watchdog_crossed: bool,
    /// Closed rejection / fallback reason codes for the Recording path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reason_codes: Vec<SmartWritingReasonCode>,
    /// Optional free-form error, scrubbed and bounded to 128 bytes. Prefer
    /// [`reason_codes`](Self::reason_codes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_form_error: Option<String>,
}

fn default_smart_writing_diagnostic_version() -> u32 {
    SMART_WRITING_DIAGNOSTIC_VERSION
}

impl SmartWritingDiagnostic {
    /// Builds a diagnostic with clamped texts, full fingerprints of the
    /// unclamped sources, and empty optional/list fields ready to fill.
    pub fn new(
        writing_mode: SmartWritingMode,
        english_eligibility: EnglishEligibilityOutcome,
        formatter_contract_id: impl Into<String>,
        validated_before: impl AsRef<str>,
        rendered_after: impl AsRef<str>,
        outcome: SmartWritingOutcome,
    ) -> Self {
        let validated = validated_before.as_ref();
        let rendered = rendered_after.as_ref();
        Self {
            version: SMART_WRITING_DIAGNOSTIC_VERSION,
            writing_mode,
            english_eligibility,
            formatter_contract_id: clamp_utf8_bytes(
                &formatter_contract_id.into(),
                MAX_SMART_WRITING_FREE_TEXT_UTF8_BYTES,
            ),
            validated_before: clamp_utf8_bytes(
                validated,
                MAX_SMART_WRITING_DIAGNOSTIC_TEXT_UTF8_BYTES,
            ),
            validated_before_sha256: text_sha256_fingerprint(validated),
            rendered_after: clamp_utf8_bytes(
                rendered,
                MAX_SMART_WRITING_DIAGNOSTIC_TEXT_UTF8_BYTES,
            ),
            rendered_after_sha256: text_sha256_fingerprint(rendered),
            outcome,
            edits: Vec::new(),
            model_id: None,
            request_began: false,
            formatter_latency_ms: None,
            http_latency_ms: None,
            safety_latency_ms: None,
            total_gate_latency_ms: None,
            credential_prep_latency_ms: None,
            reap_watchdog_crossed: false,
            reason_codes: Vec::new(),
            free_form_error: None,
        }
    }

    /// Sets the model ID, clamping to [`MAX_MODEL_ID_UTF8_BYTES`]. Empty becomes
    /// `None`.
    pub fn set_model_id(&mut self, model_id: impl Into<String>) {
        let clamped = clamp_utf8_bytes(&model_id.into(), MAX_MODEL_ID_UTF8_BYTES);
        self.model_id = if clamped.is_empty() {
            None
        } else {
            Some(clamped)
        };
    }

    /// Replaces edit evidence, retaining at most
    /// [`MAX_SMART_WRITING_DIAGNOSTIC_EDITS`] entries in input order.
    pub fn set_edits(&mut self, edits: Vec<SmartWritingEditEvidence>) {
        self.edits = edits;
        if self.edits.len() > MAX_SMART_WRITING_DIAGNOSTIC_EDITS {
            self.edits.truncate(MAX_SMART_WRITING_DIAGNOSTIC_EDITS);
        }
    }

    /// Sets a free-form error, clamping to
    /// [`MAX_SMART_WRITING_FREE_TEXT_UTF8_BYTES`]. Prefer enums.
    pub fn set_free_form_error(&mut self, error: impl Into<String>) {
        let clamped = clamp_utf8_bytes(&error.into(), MAX_SMART_WRITING_FREE_TEXT_UTF8_BYTES);
        self.free_form_error = if clamped.is_empty() {
            None
        } else {
            Some(clamped)
        };
    }

    /// Re-applies every §10 Smart Writing byte budget, the edit-count cap, and
    /// fingerprint form validation.
    ///
    /// Constructors and setters clamp, but the diagnostic fields stay publicly
    /// mutable (serde + in-process assembly). Export scrubbing can also expand a
    /// field past its budget when a short secret is replaced with
    /// [`REDACTED`]. Persistence and export call this so the bounds remain
    /// invariants of stored and exported records.
    ///
    /// Fingerprints that already match `sha256:` + 64 lowercase hex are kept
    /// (they may digest an unclamped source longer than the clamped text). Any
    /// other value is rejected and regenerated from the clamped text so
    /// free-form or secret-bearing mutation cannot persist or export.
    pub fn normalize(&mut self) {
        self.formatter_contract_id = clamp_utf8_bytes(
            &self.formatter_contract_id,
            MAX_SMART_WRITING_FREE_TEXT_UTF8_BYTES,
        );
        self.validated_before = clamp_utf8_bytes(
            &self.validated_before,
            MAX_SMART_WRITING_DIAGNOSTIC_TEXT_UTF8_BYTES,
        );
        self.rendered_after = clamp_utf8_bytes(
            &self.rendered_after,
            MAX_SMART_WRITING_DIAGNOSTIC_TEXT_UTF8_BYTES,
        );
        // Validate fingerprints after text clamps so invalid values regenerate
        // from the bounded stored text (unclamped source is unavailable here).
        sanitize_text_sha256_fingerprint(&mut self.validated_before_sha256, &self.validated_before);
        sanitize_text_sha256_fingerprint(&mut self.rendered_after_sha256, &self.rendered_after);
        if let Some(model_id) = self.model_id.take() {
            let clamped = clamp_utf8_bytes(&model_id, MAX_MODEL_ID_UTF8_BYTES);
            self.model_id = if clamped.is_empty() {
                None
            } else {
                Some(clamped)
            };
        }
        if let Some(error) = self.free_form_error.take() {
            let clamped = clamp_utf8_bytes(&error, MAX_SMART_WRITING_FREE_TEXT_UTF8_BYTES);
            self.free_form_error = if clamped.is_empty() {
                None
            } else {
                Some(clamped)
            };
        }
        if self.edits.len() > MAX_SMART_WRITING_DIAGNOSTIC_EDITS {
            self.edits.truncate(MAX_SMART_WRITING_DIAGNOSTIC_EDITS);
        }
        for edit in &mut self.edits {
            edit.normalize();
        }
    }
}

/// The correlated local diagnostic evidence of a single Recording.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DiagnosticRecord {
    pub correlation_id: String,
    pub recording_id: u64,
    pub recorded_at_unix_ms: u64,
    #[serde(default)]
    pub stages: Vec<LifecycleStage>,
    #[serde(default)]
    pub streamed_chunk_count: u32,
    #[serde(default)]
    pub source_transcripts: Vec<SourceTranscriptRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_coverage: Vec<SourceCoverageRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_transcript: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<TranscriptSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_provider: Option<Provider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_confidence: Option<SourceSelectionConfidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
    #[serde(default)]
    pub reconciliation_requested: bool,
    #[serde(default)]
    pub recovery_attempted: bool,
    #[serde(default)]
    pub delivery_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_method: Option<DeliveryMethod>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_fallback_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_chunk_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_finalized_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated_by: Option<CaptureLimit>,
    #[serde(default)]
    pub provider_timings_ms: Vec<ProviderTiming>,
    /// Every configured provider that failed or was absent for this Recording,
    /// with its stage and boundary diagnostic. Empty when both providers
    /// contributed a Source Transcript. `voisu history` and `voisu export`
    /// serialize this field, so a missing Source Transcript is never silent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_failures: Vec<ProviderFailure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_to_text_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debug_audio: Option<DebugAudioRecord>,
    /// Optional Smart Writing path diagnostic. Absent on pre-SW records so old
    /// history lines keep deserializing without a schema bump.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smart_writing: Option<SmartWritingDiagnostic>,
    /// Optional Developer Prompt Rendering timeline. Default/release builds
    /// persist only the production shape; the evaluation late-copy field does
    /// not exist unless the crate is compiled with its explicit eval feature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dpr: Option<DprDiagnostic>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent_reconstruction: Option<IntentReconstructionDiagnostic>,
}

impl DiagnosticRecord {
    /// Starts a record for a Recording, stamping the correlation ID and the wall
    /// clock so retention can expire it by age.
    pub fn new(correlation_id: String, recording_id: u64) -> Self {
        Self {
            correlation_id,
            recording_id,
            recorded_at_unix_ms: unix_millis_now(),
            stages: Vec::new(),
            streamed_chunk_count: 0,
            source_transcripts: Vec::new(),
            source_coverage: Vec::new(),
            final_transcript: None,
            selection: None,
            selected_provider: None,
            selection_confidence: None,
            validation_reason: None,
            fallback_reason: None,
            reconciliation_requested: false,
            recovery_attempted: false,
            delivery_count: 0,
            delivery_method: None,
            delivery_fallback_reason: None,
            first_chunk_ms: None,
            capture_finalized_ms: None,
            truncated_by: None,
            provider_timings_ms: Vec::new(),
            provider_failures: Vec::new(),
            release_to_text_ms: None,
            error: None,
            debug_audio: None,
            smart_writing: None,
            dpr: None,
            intent_reconstruction: None,
        }
    }

    pub fn set_final_transcript(&mut self, text: String) {
        self.final_transcript = Some(clamp_text(text));
    }
}

/// The bounded local retention policy for diagnostic history and debug audio.
#[derive(Clone, Copy, Debug)]
pub struct RetentionPolicy {
    pub max_records: usize,
    pub max_age: Duration,
    pub debug_audio_ttl: Duration,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            max_records: DEFAULT_MAX_RECORDS,
            max_age: DEFAULT_MAX_AGE,
            debug_audio_ttl: DEFAULT_DEBUG_AUDIO_TTL,
        }
    }
}

impl RetentionPolicy {
    /// Reads the retention policy from the environment, falling back to defaults.
    /// `VOISU_DIAGNOSTIC_MAX_RECORDS`, `VOISU_DIAGNOSTIC_MAX_AGE_SECS`, and
    /// `VOISU_DEBUG_AUDIO_TTL_SECS` configure retention locally.
    pub fn from_env() -> Self {
        let mut policy = Self::default();
        if let Some(max_records) = env_parse("VOISU_DIAGNOSTIC_MAX_RECORDS") {
            policy.max_records = max_records;
        }
        if let Some(seconds) = env_parse::<u64>("VOISU_DIAGNOSTIC_MAX_AGE_SECS") {
            policy.max_age = Duration::from_secs(seconds);
        }
        if let Some(seconds) = env_parse::<u64>("VOISU_DEBUG_AUDIO_TTL_SECS") {
            policy.debug_audio_ttl = Duration::from_secs(seconds);
        }
        policy
    }

    fn max_age_ms(&self) -> u64 {
        u64::try_from(self.max_age.as_millis()).unwrap_or(u64::MAX)
    }

    /// Prunes a set of records to the policy, preserving the input's
    /// chronological (append) order. Records past the age bound and the oldest
    /// records beyond the count bound are dropped; among retained records, any
    /// debug audio whose expiry has passed is detached. Every dropped or
    /// detached debug audio path is returned so the caller can remove the file
    /// safely. Relying on append order rather than wall-clock ties keeps the
    /// retained set stable across repeated load/prune/store cycles even when
    /// several Recordings share the same millisecond.
    pub fn prune(&self, records: Vec<DiagnosticRecord>, now_ms: u64) -> PruneOutcome {
        let age_floor = now_ms.saturating_sub(self.max_age_ms());
        let mut expired_audio = Vec::new();
        let mut kept: Vec<DiagnosticRecord> = records
            .into_iter()
            .filter_map(|mut record| {
                if record.recorded_at_unix_ms < age_floor {
                    if let Some(audio) = record.debug_audio.take() {
                        expired_audio.push(audio.file_name);
                    }
                    None
                } else {
                    Some(record)
                }
            })
            .collect();
        if kept.len() > self.max_records {
            let overflow = kept.len() - self.max_records;
            for mut record in kept.drain(0..overflow) {
                if let Some(audio) = record.debug_audio.take() {
                    expired_audio.push(audio.file_name);
                }
            }
        }
        for record in &mut kept {
            if let Some(audio) = &record.debug_audio {
                if audio.is_expired(now_ms) {
                    expired_audio.push(audio.file_name.clone());
                    record.debug_audio = None;
                }
            }
        }
        PruneOutcome {
            kept,
            expired_audio,
        }
    }
}

/// The result of pruning: the retained records (newest first) and the debug
/// audio file names that are now safe to delete from the store's audio
/// directory.
pub struct PruneOutcome {
    pub kept: Vec<DiagnosticRecord>,
    pub expired_audio: Vec<String>,
}

fn env_parse<T: std::str::FromStr>(name: &str) -> Option<T> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
}

/// True when an environment variable name denotes a secret whose value must
/// never appear in a diagnostic export, under any key.
pub fn is_secret_env_key(key: &str) -> bool {
    const MARKERS: [&str; 7] = [
        "API_KEY",
        "APIKEY",
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "AUTHORIZATION",
        "CREDENTIAL",
    ];
    let upper = key.to_ascii_uppercase();
    MARKERS.iter().any(|marker| upper.contains(marker))
}

/// The explicit allowlist of environment keys a diagnostic export may carry.
/// Everything else — including unknown `VOISU_*` values, which could hold a
/// secret under an unrecognized name — is omitted entirely. URL values are
/// additionally sanitized of userinfo credentials and query parameters.
pub const EXPORT_ENV_ALLOWLIST: [&str; 11] = [
    "VOISU_GROQ_TRANSCRIPTION_URL",
    "VOISU_DEEPGRAM_TRANSCRIPTION_URL",
    "VOISU_GROQ_RECONCILIATION_URL",
    "VOISU_GROQ_RECONCILIATION_MODEL",
    "VOISU_GROQ_MODEL",
    "VOISU_PIPEWIRE_TARGET",
    "VOISU_RECORDING_DEADLINE_MS",
    "VOISU_DIAGNOSTIC_MAX_RECORDS",
    "VOISU_DIAGNOSTIC_MAX_AGE_SECS",
    "VOISU_DEBUG_AUDIO_TTL_SECS",
    "VOISU_DEBUG_CAPTURE",
];

/// Strips credentials and query/fragment parameters from a URL so an exported
/// endpoint never carries `user:password@` userinfo or `?key=` style secrets.
/// FAIL CLOSED: only well-formed `http://` / `https://` URLs are sanitized and
/// passed through — a scheme-less, malformed, or unrecognized-scheme value is
/// replaced with the redaction mask entirely, because a value we cannot parse
/// is a value whose credential placement we cannot reason about.
pub fn sanitize_url(value: &str) -> String {
    let Some((scheme, rest)) = value.split_once("://") else {
        return REDACTED.to_owned();
    };
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        return REDACTED.to_owned();
    }
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, Some(path)),
        None => (rest, None),
    };
    let host = authority
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(authority);
    if !is_valid_authority_host(host) {
        return REDACTED.to_owned();
    }
    match path {
        Some(path) => format!("{scheme}://{host}/{path}"),
        None => format!("{scheme}://{host}"),
    }
}

/// Strictly validates a sanitized authority: a DNS-safe host name or a
/// bracketed IPv6 literal, optionally followed by `:port` where the port
/// parses as a non-zero u16. Anything else — whitespace, backslashes, stray
/// separators, out-of-range ports — is invalid, and the caller redacts.
fn is_valid_authority_host(host: &str) -> bool {
    if let Some(inner) = host.strip_prefix('[') {
        // IPv6 literal: `[addr]` or `[addr]:port`.
        let Some((address, after)) = inner.split_once(']') else {
            return false;
        };
        // Structural validation, not just a character check: "[deadbeef]" and
        // "[2001:db8::1::2]" are hex-and-colon soup, not IPv6 addresses.
        let address_ok = address.parse::<std::net::Ipv6Addr>().is_ok();
        let port_ok = match after.strip_prefix(':') {
            Some(port) => is_valid_port(port),
            None => after.is_empty(),
        };
        return address_ok && port_ok;
    }
    match host.split_once(':') {
        None => is_dns_safe_name(host),
        Some((name, port)) => is_dns_safe_name(name) && is_valid_port(port),
    }
}

fn is_dns_safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '.'
        })
}

fn is_valid_port(port: &str) -> bool {
    // Digits only (parse::<u16> would tolerate a leading '+'), then 1-65535.
    !port.is_empty()
        && port.bytes().all(|byte| byte.is_ascii_digit())
        && port.parse::<u16>().is_ok_and(|value| value != 0)
}

/// A redacted, self-contained diagnostic export for one Recording. It carries
/// the scrubbed local record plus only an explicit allowlist of configuration
/// environment keys; every secret value is masked and unrelated environment
/// values are dropped entirely.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DiagnosticExport {
    pub record: DiagnosticRecord,
    pub environment: BTreeMap<String, String>,
}

/// Filters an environment for export: only explicitly allowlisted keys survive,
/// URL values are stripped of userinfo and query parameters, and any
/// allowlisted key that nonetheless denotes a secret is masked.
pub fn redacted_environment(
    vars: impl IntoIterator<Item = (String, String)>,
) -> BTreeMap<String, String> {
    vars.into_iter()
        .filter(|(key, _)| EXPORT_ENV_ALLOWLIST.contains(&key.as_str()))
        .map(|(key, value)| {
            let value = if is_secret_env_key(&key) {
                REDACTED.to_owned()
            } else if key.ends_with("_URL") {
                sanitize_url(&value)
            } else {
                value
            };
            (key, value)
        })
        .collect()
}

/// Replaces every occurrence of any known secret value inside a free-form
/// string with the redaction mask. Transcripts can literally contain a spoken
/// or pasted secret, so export scrubs them against the values of every
/// secret-denoting environment variable.
pub fn scrub_secret_values(text: &str, secrets: &[String]) -> String {
    let mut scrubbed = text.to_owned();
    for secret in secrets {
        if !secret.is_empty() {
            scrubbed = scrubbed.replace(secret.as_str(), REDACTED);
        }
    }
    scrubbed
}

/// Strips userinfo credentials and the entire query/fragment from a single
/// `http(s)://` URL token, keeping only scheme, host, and path. A boundary
/// diagnostic can echo a signed provider URL (`https://user:pw@host/listen?token=abc`)
/// whose secret does NOT come from any environment variable, so name-based
/// secret scrubbing never sees it — this removes it structurally instead.
fn strip_url_secrets(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    // Drop query and fragment wholesale: token-bearing parameters live there.
    let core = rest.split(['?', '#']).next().unwrap_or("");
    let (authority, path) = match core.split_once('/') {
        Some((authority, path)) => (authority, Some(path)),
        None => (core, None),
    };
    // Drop any `user:password@` userinfo prefix.
    let host = authority
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(authority);
    match path {
        Some(path) => format!("{scheme}://{host}/{path}"),
        None => format!("{scheme}://{host}"),
    }
}

/// Structurally scrubs every URL embedded in a free-form string of its userinfo
/// credentials and query/fragment secrets, preserving all surrounding text. This
/// defends against secrets that reach a diagnostic through a URL rather than
/// through a secret-named environment variable. It scans EVERY occurrence
/// case-insensitively (so a non-URL `"httpStatus"` never masks a later signed
/// URL, and `HTTPS://` is caught) and covers websocket schemes (`ws`/`wss`),
/// which Deepgram's streaming endpoint uses.
pub fn scrub_embedded_urls(text: &str) -> String {
    const SCHEMES: [&str; 4] = ["http://", "https://", "ws://", "wss://"];
    // A lowercased copy for case-insensitive matching. ASCII lowercasing never
    // changes byte length, and the schemes are ASCII, so offsets map back to the
    // original text at char boundaries.
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    while cursor < text.len() {
        let next = SCHEMES
            .iter()
            .filter_map(|scheme| lower[cursor..].find(scheme).map(|offset| cursor + offset))
            .min();
        match next {
            Some(start) => {
                out.push_str(&text[cursor..start]);
                let tail = &text[start..];
                // A URL token runs until the first whitespace.
                let end = tail.find(char::is_whitespace).unwrap_or(tail.len());
                out.push_str(&strip_url_secrets(&text[start..start + end]));
                cursor = start + end;
            }
            None => {
                out.push_str(&text[cursor..]);
                break;
            }
        }
    }
    out
}

/// Applies both scrubbing passes to a free-form string: known secret VALUES
/// (from secret-named environment variables) and structural URL secrets
/// (userinfo, query/fragment) that no name-based rule would catch.
fn scrub_free_text(text: &str, secrets: &[String]) -> String {
    scrub_embedded_urls(&scrub_secret_values(text, secrets))
}

fn secret_values(vars: &[(String, String)]) -> Vec<String> {
    // Every non-empty secret value is scrubbed: credentials have no minimum
    // length, so even a one-character value must never survive an export.
    vars.iter()
        .filter(|(key, value)| is_secret_env_key(key) && !value.is_empty())
        .map(|(_, value)| value.clone())
        .collect()
}

/// Builds a redacted export from a record and the current environment: the
/// environment is reduced to the explicit allowlist and every free-form string
/// in the record (Source Transcripts, final Transcript, reasons, error) is
/// scrubbed of known secret values.
pub fn export_record(
    record: DiagnosticRecord,
    vars: impl IntoIterator<Item = (String, String)>,
) -> DiagnosticExport {
    let vars: Vec<(String, String)> = vars.into_iter().collect();
    let secrets = secret_values(&vars);
    let mut record = record;
    for source in &mut record.source_transcripts {
        source.text = scrub_free_text(&source.text, &secrets);
    }
    record.final_transcript = record
        .final_transcript
        .map(|text| scrub_free_text(&text, &secrets));
    record.validation_reason = record
        .validation_reason
        .map(|text| scrub_free_text(&text, &secrets));
    record.fallback_reason = record
        .fallback_reason
        .map(|text| scrub_free_text(&text, &secrets));
    record.error = record.error.map(|text| scrub_free_text(&text, &secrets));
    record.delivery_fallback_reason = record
        .delivery_fallback_reason
        .map(|text| scrub_free_text(&text, &secrets));
    for failure in &mut record.provider_failures {
        failure.diagnostic = scrub_free_text(&failure.diagnostic, &secrets);
    }
    if let Some(smart) = record.smart_writing.as_mut() {
        scrub_smart_writing_diagnostic(smart, &secrets);
    }
    if let Some(dpr) = record.dpr.as_mut() {
        #[cfg(feature = "dpr-eval-late-retain")]
        if let Some(late) = dpr.late_evaluation.as_mut() {
            late.candidate_text_clamped = scrub_free_text(&late.candidate_text_clamped, &secrets);
        }
        dpr.normalize();
    }
    if let Some(intent) = record.intent_reconstruction.as_mut() {
        intent.candidate = intent
            .candidate
            .take()
            .map(|candidate| scrub_free_text(&candidate, &secrets));
        intent.normalize();
    }
    DiagnosticExport {
        record,
        environment: redacted_environment(vars),
    }
}

/// Scrubs every free-form string on a Smart Writing diagnostic, then re-clamps
/// §10 budgets and validates fingerprint form via [`SmartWritingDiagnostic::normalize`].
/// Fingerprints are not scrubbed as free text: only the exact `sha256:` + 64
/// lowercase hex form is retained; any other value is regenerated so secrets
/// cannot hide in a fingerprint field. Never introduces credentials, prompts,
/// raw bodies, or app/window context.
///
/// Scrub-then-normalize is required: replacing a short secret with
/// [`REDACTED`] can expand a field past its byte budget, and public mutation can
/// leave oversized text/edits or invalid fingerprints on the in-memory record
/// before export.
fn scrub_smart_writing_diagnostic(smart: &mut SmartWritingDiagnostic, secrets: &[String]) {
    smart.formatter_contract_id = scrub_free_text(&smart.formatter_contract_id, secrets);
    smart.validated_before = scrub_free_text(&smart.validated_before, secrets);
    smart.rendered_after = scrub_free_text(&smart.rendered_after, secrets);
    if let Some(model_id) = smart.model_id.as_mut() {
        *model_id = scrub_free_text(model_id, secrets);
    }
    smart.free_form_error = smart
        .free_form_error
        .as_ref()
        .map(|text| scrub_free_text(text, secrets));
    for edit in &mut smart.edits {
        edit.edit_id = scrub_free_text(&edit.edit_id, secrets);
        edit.rule_id = scrub_free_text(&edit.rule_id, secrets);
        edit.before = scrub_free_text(&edit.before, secrets);
        edit.after = scrub_free_text(&edit.after, secrets);
    }
    // Re-clamp after scrub so redaction expansion cannot violate §10 budgets,
    // and sanitize fingerprints so invalid/secret-bearing values cannot export.
    smart.normalize();
}

/// A bounded, private, on-disk store of correlated diagnostic records. All state
/// lives under one directory the caller has already secured; the store keeps
/// files private (0600) and never leaves the local filesystem. One internal
/// lock serializes every load-prune-persist cycle so concurrent writers (the
/// actor answering history/export while a completed Recording persists its
/// record) can never clobber each other from stale snapshots. Poison is recovered:
/// the mutex guards no in-memory state, and every on-disk rewrite is atomic, so a
/// panic while holding it cannot leave invariants that require abandoning the lock.
pub struct DiagnosticStore {
    dir: PathBuf,
    policy: RetentionPolicy,
    lock: Mutex<()>,
    temp_counter: AtomicU64,
    #[cfg(test)]
    directory_sync_attempts: AtomicU64,
}

impl DiagnosticStore {
    /// Opens (creating if needed) a private diagnostics directory plus its audio
    /// and fixture subdirectories, all 0700 and owned by the current user.
    pub fn open(dir: PathBuf, policy: RetentionPolicy) -> io::Result<Self> {
        create_private_dir(&dir)?;
        let store = Self {
            dir,
            policy,
            lock: Mutex::new(()),
            temp_counter: AtomicU64::new(0),
            #[cfg(test)]
            directory_sync_attempts: AtomicU64::new(0),
        };
        create_private_dir(&store.audio_dir())?;
        create_private_dir(&store.fixture_dir())?;
        Ok(store)
    }

    pub fn audio_dir(&self) -> PathBuf {
        self.dir.join("audio")
    }

    /// The only directory replay may read fixtures from.
    pub fn fixture_dir(&self) -> PathBuf {
        self.dir.join("fixtures")
    }

    fn history_file(&self) -> PathBuf {
        self.dir.join(HISTORY_FILE)
    }

    fn lock_store(&self) -> MutexGuard<'_, ()> {
        self.lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn load_raw(&self) -> io::Result<Vec<DiagnosticRecord>> {
        // Only a missing history is an empty history. Any other open/read error
        // is propagated so no caller can mistake unavailable durable state for
        // an empty snapshot and compact that fiction over the retained log.
        //
        // Line-delimited, so an unreadable line costs only that record. A crash
        // during the append below can leave a torn final line; skipping it keeps
        // every record that was completely written, where a single-value file
        // would have to discard the whole ring.
        let mut file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(self.history_file())
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "diagnostics history is not a regular file",
            ));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .filter_map(|line| serde_json::from_slice(line).ok())
            .collect())
    }

    fn open_append_file(&self) -> io::Result<(File, bool)> {
        let path = self.history_file();
        match OpenOptions::new()
            .read(true)
            .append(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC)
            .open(&path)
        {
            Ok(file) => Ok((file, true)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let file = OpenOptions::new()
                    .read(true)
                    .append(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
                    .open(path)?;
                if !file.metadata()?.is_file() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "diagnostics history is not a regular file",
                    ));
                }
                Ok((file, false))
            }
            Err(error) => Err(error),
        }
    }

    /// Makes a newly-created or atomically replaced directory entry durable.
    ///
    /// The record itself is already safely on disk when this runs. Directory
    /// syncing is therefore best-effort: an unusual filesystem that refuses a
    /// directory fsync must not turn observability into a delivery failure.
    fn sync_directory_best_effort(&self, operation: &str) {
        #[cfg(test)]
        self.directory_sync_attempts.fetch_add(1, Ordering::Relaxed);
        let result = File::open(&self.dir).and_then(|directory| directory.sync_all());
        if let Err(error) = result {
            eprintln!("diagnostics directory sync failed after {operation}: {error}");
        }
    }

    /// Appends one record to the log without rewriting what is already there.
    ///
    /// This is the whole reason the history is line-delimited. Rewriting the
    /// full ring on the completion path of every dictation costs, at the default
    /// retention on a real disk, ~250 ms per Recording — the serialize, the
    /// write and the fsync all scale with the ENTIRE retained history rather
    /// than with the one record being added. An append is bounded by the record.
    fn append(&self, record: &DiagnosticRecord) -> io::Result<()> {
        let mut encoded = serde_json::to_vec(record)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        // `to_vec` escapes any newline inside a string, so a serialized record
        // is always exactly one line and can never split the log.
        debug_assert!(!encoded.contains(&b'\n'));
        encoded.push(b'\n');
        let (mut file, created) = self.open_append_file()?;
        if file.metadata()?.len() != 0 {
            file.seek(SeekFrom::End(-1))?;
            let mut last = [0_u8; 1];
            file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                // A previous ENOSPC/crash may have left a partial JSON tail.
                // Separate it from this complete record so the torn line alone
                // is discarded and the newly accepted Recording stays readable.
                encoded.insert(0, b'\n');
            }
        }
        file.write_all(&encoded)?;
        // `sync_data`, not `sync_all`: the file's size is the only metadata that
        // matters here and a data sync carries it, so this does not pay for an
        // inode flush the history does not need.
        file.sync_data()?;
        if created {
            self.sync_directory_best_effort("history creation");
        }
        Ok(())
    }

    /// Rewrites the whole log atomically. Used only when pruning actually
    /// removed something, so the common append path never pays for it.
    fn write_all(&self, records: &[DiagnosticRecord]) -> io::Result<()> {
        const TEMP_CREATE_ATTEMPTS: u32 = 32;
        let mut encoded = Vec::new();
        for record in records {
            serde_json::to_writer(&mut encoded, record)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            encoded.push(b'\n');
        }
        // A unique, exclusively created temp file per write: O_EXCL refuses to
        // follow a pre-planted symlink at the temp path and the descriptor is
        // created 0600 rather than trusting pre-existing permissions. A name
        // collision (a crash leftover after PID reuse, or a planted file) is
        // NOT fatal: creation retries with a fresh nonce, bounded, so a record
        // is never lost to a stale temp file.
        let (temp, mut file) = 'created: {
            for _ in 0..TEMP_CREATE_ATTEMPTS {
                let temp = self.dir.join(format!(
                    "{HISTORY_TEMP_PREFIX}{}.{}",
                    std::process::id(),
                    self.temp_counter.fetch_add(1, Ordering::Relaxed)
                ));
                match OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&temp)
                {
                    Ok(file) => break 'created (temp, file),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error),
                }
            }
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "cannot create a unique diagnostics temp file",
            ));
        };
        let result = file.write_all(&encoded).and_then(|()| file.sync_all());
        if let Err(error) = result {
            let _ = fs::remove_file(&temp);
            return Err(error);
        }
        let renamed = fs::rename(&temp, self.history_file());
        if renamed.is_err() {
            let _ = fs::remove_file(&temp);
        } else {
            self.sync_directory_best_effort("history replacement");
        }
        renamed
    }

    /// Removes an expired debug audio capture by validated basename only, so a
    /// tampered or corrupt history file can never steer deletion outside the
    /// store's private audio directory.
    fn remove_audio(&self, file_names: &[String]) {
        for file_name in file_names {
            if is_safe_file_name(file_name) {
                let _ = fs::remove_file(self.audio_dir().join(file_name));
            }
        }
    }

    /// Prunes in memory, removes expired debug audio, and returns the retained
    /// records newest first. Writes nothing.
    ///
    /// Pruning on every load is what lets the log carry records the policy has
    /// already expired: they are dropped here, so no reader can ever see one,
    /// and the log only has to be rewritten when it has drifted far enough to be
    /// worth the cost. Callers must hold the store lock.
    fn prune_in_memory(&self, records: Vec<DiagnosticRecord>) -> Vec<DiagnosticRecord> {
        let outcome = self.policy.prune(records, unix_millis_now());
        self.remove_audio(&outcome.expired_audio);
        let mut newest_first = outcome.kept;
        newest_first.reverse();
        newest_first
    }

    /// Rewrites the log to exactly the retained set. Callers must hold the lock.
    fn compact(&self, records: Vec<DiagnosticRecord>) -> io::Result<Vec<DiagnosticRecord>> {
        let outcome = self.policy.prune(records, unix_millis_now());
        self.remove_audio(&outcome.expired_audio);
        self.write_all(&outcome.kept)?;
        let mut newest_first = outcome.kept;
        newest_first.reverse();
        Ok(newest_first)
    }

    /// How far the log may drift past the retained record count, or how many
    /// age-expired records may accumulate, before a write compacts it.
    ///
    /// Without slack there is no amortization at all at the steady state this
    /// retention policy is designed for: once the ring is full, EVERY Recording
    /// pushes the count over the bound, so every Recording would prune one
    /// record and rewrite the whole log — measured at ~250 ms per dictation at
    /// the default retention on a real disk. Compacting once per slack
    /// Recordings instead makes that ~5 ms amortized. The cost is a log bounded
    /// at `max_records + slack` rather than `max_records`; age-expired drift is
    /// rewritten when a subsequent Recording observes more than `slack`
    /// expired entries. Readers are unaffected, because every load prunes
    /// before returning.
    fn compaction_slack(&self) -> usize {
        (self.policy.max_records / 4).max(8)
    }

    fn expired_by_age(&self, records: &[DiagnosticRecord], now_ms: u64) -> usize {
        let age_floor = now_ms.saturating_sub(self.policy.max_age_ms());
        records
            .iter()
            .filter(|record| record.recorded_at_unix_ms < age_floor)
            .count()
    }

    /// Appends a completed Recording's record, prunes to the retention policy,
    /// removes any now-expired debug audio, and returns the retained history
    /// (newest first).
    ///
    /// Smart Writing §10 byte budgets, edit-count caps, and fingerprint form
    /// validation are re-applied before the record is serialized so public field
    /// mutation cannot persist oversized text, model IDs, free-form errors, more
    /// than [`MAX_SMART_WRITING_DIAGNOSTIC_EDITS`] edits, or non-fingerprint
    /// content in the SHA-256 fields.
    pub fn record(&self, mut record: DiagnosticRecord) -> io::Result<Vec<DiagnosticRecord>> {
        if let Some(smart) = record.smart_writing.as_mut() {
            smart.normalize();
        }
        if let Some(dpr) = record.dpr.as_mut() {
            dpr.normalize();
        }
        if let Some(intent) = record.intent_reconstruction.as_mut() {
            intent.normalize();
        }
        let _guard = self.lock_store();
        let mut records = self.load_raw()?;
        let slack = self.compaction_slack();
        let count_drifted = records.len() + 1 > self.policy.max_records + slack;
        // Age drift needs the same amortization as count drift. Without this
        // bounded slack, every first Recording after an age boundary would
        // rewrite the whole log; without any age trigger, low-volume long-lived
        // daemons retain expired transcript text on disk indefinitely.
        let age_drifted = self.expired_by_age(&records, unix_millis_now()) > slack;
        records.push(record);
        if count_drifted || age_drifted {
            return self.compact(records);
        }
        // Appending the record as given, not the pruned copy: the log is allowed
        // to hold more than the policy retains, and the next load prunes it.
        self.append(records.last().expect("the record was just pushed"))?;
        Ok(self.prune_in_memory(records))
    }

    /// Returns the retained history (newest first), pruning stale records and
    /// expired debug audio as a side effect so a reader never sees an entry the
    /// retention policy has already expired.
    ///
    /// Reads do not write. `Command::History` runs this INLINE in the lifecycle
    /// actor, so a concurrent `voisu stop` queues behind it; making it rewrite
    /// and fsync the whole log put ~250 ms at the default retention in front of
    /// that Stop for no gain, since pruning in memory already hides every
    /// expired record.
    pub fn history(&self) -> io::Result<Vec<DiagnosticRecord>> {
        let _guard = self.lock_store();
        let records = self.load_raw()?;
        Ok(self.prune_in_memory(records))
    }

    /// Finds one Recording's record by its correlation ID, after pruning.
    pub fn find(&self, correlation_id: &str) -> io::Result<Option<DiagnosticRecord>> {
        Ok(self
            .history()?
            .into_iter()
            .find(|record| record.correlation_id == correlation_id))
    }

    /// Removes expired debug audio and over-retention records, then sweeps the
    /// audio directory itself: any capture whose filename-encoded expiry has
    /// passed, whose name is unparsable, or that no retained record references
    /// is removed. Run at daemon startup, so a capture orphaned by a crash
    /// before its record persisted can never linger.
    pub fn cleanup_expired(&self) -> io::Result<()> {
        // Purge crash-leftover temp files FIRST, before any history rewrite:
        // enough stale leftovers could otherwise exhaust the bounded create
        // retries and fail the rewrite that was supposed to clean them up.
        {
            let _guard = self.lock_store();
            if let Ok(entries) = fs::read_dir(&self.dir) {
                for entry in entries.flatten() {
                    if entry
                        .file_name()
                        .to_str()
                        .is_some_and(|name| name.starts_with(HISTORY_TEMP_PREFIX))
                    {
                        let _ = fs::remove_file(entry.path());
                    }
                }
            }
        }
        // Startup is the one place a whole-log rewrite is free, so compact here.
        // Runtime never rewrites unless the log has drifted past its slack, so
        // records that aged out between runs would otherwise sit on disk being
        // re-read and re-pruned on every load until enough new Recordings
        // arrived to trigger a compaction.
        let kept = {
            let _guard = self.lock_store();
            let records = self.load_raw()?;
            self.compact(records)?
        };
        let _guard = self.lock_store();
        let referenced: Vec<&str> = kept
            .iter()
            .filter_map(|record| record.debug_audio.as_ref())
            .map(|audio| audio.file_name.as_str())
            .collect();
        let now = unix_millis_now();
        for entry in fs::read_dir(self.audio_dir())? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                let _ = fs::remove_file(entry.path());
                continue;
            };
            let expired = match expiry_from_file_name(name) {
                Some(expires_at_unix_ms) => now >= expires_at_unix_ms,
                None => true,
            };
            if expired || !referenced.contains(&name) {
                let _ = fs::remove_file(entry.path());
            }
        }
        Ok(())
    }

    /// Persists an explicit debug audio capture for a correlation ID, returning
    /// its recorded basename and expiry. The expiry is encoded in the file name
    /// so an orphaned capture still expires, and the file is created exclusively
    /// (never following a pre-planted symlink) with private 0600 permissions.
    /// Only called when the user has enabled debug capture.
    pub fn store_debug_audio(
        &self,
        correlation_id: &str,
        pcm_s16le_mono_16khz: &[u8],
    ) -> io::Result<DebugAudioRecord> {
        let now = unix_millis_now();
        let ttl_ms = u64::try_from(self.policy.debug_audio_ttl.as_millis()).unwrap_or(u64::MAX);
        let expires_at_unix_ms = now.saturating_add(ttl_ms);
        let file_name = format!(
            "{}-exp{}.pcm",
            sanitize_component(correlation_id),
            expires_at_unix_ms
        );
        let path = self.audio_dir().join(&file_name);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        let result = file
            .write_all(pcm_s16le_mono_16khz)
            .and_then(|()| file.sync_all());
        if let Err(error) = result {
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        Ok(DebugAudioRecord {
            file_name,
            captured_at_unix_ms: now,
            expires_at_unix_ms,
        })
    }
}

/// Parses the `-exp<unix-ms>.pcm` suffix a debug audio capture encodes so
/// startup cleanup can expire orphans without a surviving record.
fn expiry_from_file_name(name: &str) -> Option<u64> {
    name.strip_suffix(".pcm")?
        .rsplit_once("-exp")?
        .1
        .parse()
        .ok()
}

/// True for a plain, traversal-free single path component.
fn is_safe_file_name(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains('/')
        && !value.contains('\\')
        && !value.contains('\0')
}

/// Restricts a correlation ID to a safe single path component (defends the audio
/// file name against traversal even though IDs are daemon-generated).
fn sanitize_component(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "diagnostics path is not a private directory",
                ));
            }
            // SAFETY: geteuid has no preconditions and does not mutate memory.
            if metadata.uid() != unsafe { libc::geteuid() } {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "diagnostics directory is not owned by the current user",
                ));
            }
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            DirBuilder::new().mode(0o700).create(path)
        }
        Err(error) => Err(error),
    }
}

/// The outcome of replaying a fixed captured fixture through the provider and
/// validation boundaries.
pub struct ReplayOutcome {
    pub source_transcripts: Vec<SourceTranscript>,
    pub timings_ms: Vec<ProviderTiming>,
    /// Time spent in the validation/reconstruction seam, excluding provider
    /// completion. This mirrors the live Recording's validation-origin clock.
    pub reconstruction_elapsed_ms: u64,
    /// Providers that failed or were absent while replaying the fixture, carried
    /// through so a replay surfaces the same failure visibility as a live
    /// Recording.
    pub provider_failures: Vec<ProviderFailure>,
    pub decision: TranscriptDecision,
}

/// Replays a fixed captured fixture through the provider and validation
/// boundaries without capturing audio again: the coordinator completes both
/// providers on the fixture and the validator produces a decision, exactly as a
/// live Recording would after Stop. No microphone is involved.
pub async fn replay_capture(
    audio: CapturedAudio,
    coordinator: ProviderCoordinator,
    validator: &mut dyn TranscriptValidator,
) -> Result<ReplayOutcome, BoundaryError> {
    let completion = coordinator.complete_with_timings(audio).await?;
    let source_transcripts = completion.sources.clone();
    let provider_failures = completion.provider_failures;
    let prepared = validator.prepare(completion.sources).await?;
    // Keep this origin aligned with the live Recording path: preparation owns
    // source classification and fallback selection, while this clock measures
    // only the bounded reconstruction seam that follows it.
    let reconstruction_started = Instant::now();
    let decision = match prepared {
        PreparedTranscriptDecision::Ready(decision) => decision,
        PreparedTranscriptDecision::Reconstruct(attempt) => validator.reconstruct(attempt).await?,
    };
    Ok(ReplayOutcome {
        source_transcripts,
        timings_ms: completion.timings_ms,
        reconstruction_elapsed_ms: u64::try_from(reconstruction_started.elapsed().as_millis())
            .unwrap_or(u64::MAX),
        provider_failures,
        decision,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    fn store_with_policy(dir: &Path, policy: RetentionPolicy) -> DiagnosticStore {
        DiagnosticStore::open(dir.join("diagnostics"), policy).unwrap()
    }

    #[test]
    fn history_fifo_is_rejected_without_a_blocking_read() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_policy(dir.path(), RetentionPolicy::default());
        let path = store.history_file();
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `path` is a live NUL-terminated pathname and mkfifo does not
        // retain the pointer.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);

        let error = store.history().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            error.to_string(),
            "diagnostics history is not a regular file"
        );
    }

    #[test]
    fn descriptor_based_history_read_still_ignores_only_a_torn_final_line() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_policy(dir.path(), RetentionPolicy::default());
        let retained = DiagnosticRecord::new("retained".to_owned(), 1);
        let mut bytes = serde_json::to_vec(&retained).unwrap();
        bytes.extend_from_slice(b"\n{\"correlation_id\":\"torn");
        fs::write(store.history_file(), bytes).unwrap();

        let history = store.history().unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].correlation_id, "retained");
    }

    #[test]
    fn cleanup_read_failure_never_replaces_the_history_path() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_policy(dir.path(), RetentionPolicy::default());
        let history = store.history_file();
        fs::create_dir(&history).unwrap();
        fs::write(history.join("retained-marker"), b"must survive").unwrap();

        let error = store.cleanup_expired().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let record_error = store
            .record(DiagnosticRecord::new("must-not-rewrite".to_owned(), 2))
            .unwrap_err();
        assert_eq!(record_error.kind(), io::ErrorKind::InvalidData);
        assert!(
            history.is_dir(),
            "failed cleanup must not rename over history"
        );
        assert_eq!(
            fs::read(history.join("retained-marker")).unwrap(),
            b"must survive"
        );
    }

    #[test]
    fn record_compacts_when_expired_age_drift_exceeds_the_bounded_slack() {
        let dir = tempfile::tempdir().unwrap();
        let policy = RetentionPolicy {
            max_records: 100,
            max_age: Duration::from_secs(1),
            debug_audio_ttl: Duration::from_secs(1),
        };
        let store = store_with_policy(dir.path(), policy);
        let expired: Vec<_> = (0..=store.compaction_slack())
            .map(|recording_id| {
                let mut record =
                    DiagnosticRecord::new(format!("expired-{recording_id}"), recording_id as u64);
                record.recorded_at_unix_ms = 0;
                record
            })
            .collect();
        store.write_all(&expired).unwrap();

        let fresh = DiagnosticRecord::new("fresh".to_owned(), 999);
        let history = store.record(fresh).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].correlation_id, "fresh");
        let durable = store.load_raw().unwrap();
        assert_eq!(durable.len(), 1, "expired text must be removed from disk");
        assert_eq!(durable[0].correlation_id, "fresh");
    }

    #[test]
    fn history_creation_and_atomic_replacement_attempt_directory_syncs() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_policy(dir.path(), RetentionPolicy::default());

        store
            .record(DiagnosticRecord::new("created".to_owned(), 1))
            .unwrap();
        assert_eq!(store.directory_sync_attempts.load(Ordering::Relaxed), 1);

        let records = store.load_raw().unwrap();
        store.write_all(&records).unwrap();
        assert_eq!(store.directory_sync_attempts.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn directory_sync_failure_is_non_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_policy(dir.path(), RetentionPolicy::default());
        let moved = dir.path().join("diagnostics-moved");
        fs::rename(&store.dir, &moved).unwrap();

        store.sync_directory_best_effort("test");
        assert_eq!(store.directory_sync_attempts.load(Ordering::Relaxed), 1);
        assert!(
            moved.is_dir(),
            "a failed best-effort sync changes no durable data"
        );
    }

    #[test]
    fn diagnostic_store_recovers_a_poisoned_serialization_lock() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(
            DiagnosticStore::open(dir.path().join("diagnostics"), RetentionPolicy::default())
                .unwrap(),
        );
        let poisoning_store = std::sync::Arc::clone(&store);
        let poisoned = std::thread::spawn(move || {
            let _guard = poisoning_store.lock.lock().unwrap();
            panic!("poison diagnostics serialization lock");
        });
        assert!(poisoned.join().is_err());

        let history = store
            .record(DiagnosticRecord::new("after-poison".to_owned(), 1))
            .expect("a poisoned serialization lock must not block diagnostics");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].correlation_id, "after-poison");
    }
}
