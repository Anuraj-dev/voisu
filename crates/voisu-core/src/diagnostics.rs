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
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{
    BoundaryError, CapturedAudio, LifecycleStage, Provider, ProviderCoordinator, ProviderTiming,
    SourceTranscript, TranscriptDecision, TranscriptSelection, TranscriptValidator,
};

/// A stored transcript text is clamped so a bounded history never grows without
/// limit and always fits a single IPC response frame. Dictation transcripts are
/// far shorter than this bound.
pub const MAX_STORED_TEXT: usize = 8 * 1024;

/// The default number of most-recent Recordings retained in local history.
pub const DEFAULT_MAX_RECORDS: usize = 20;
/// The default maximum age of a retained diagnostic record.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(24 * 3600);
/// The default time-to-live for an explicitly captured debug audio file.
pub const DEFAULT_DEBUG_AUDIO_TTL: Duration = Duration::from_secs(3600);

/// The masking placeholder a diagnostic export writes in place of any secret.
pub const REDACTED: &str = "<redacted>";

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

/// The recorded location and expiry of an explicitly captured debug audio file.
/// Its presence is the only way raw audio is retained; without debug capture it
/// is `None`.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct DebugAudioRecord {
    pub path: String,
    pub captured_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
}

impl DebugAudioRecord {
    pub fn is_expired(&self, now_ms: u64) -> bool {
        now_ms >= self.expires_at_unix_ms
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_transcript: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<TranscriptSelection>,
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
    pub first_chunk_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_finalized_ms: Option<u64>,
    #[serde(default)]
    pub provider_timings_ms: Vec<ProviderTiming>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_to_text_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debug_audio: Option<DebugAudioRecord>,
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
            final_transcript: None,
            selection: None,
            validation_reason: None,
            fallback_reason: None,
            reconciliation_requested: false,
            recovery_attempted: false,
            delivery_count: 0,
            first_chunk_ms: None,
            capture_finalized_ms: None,
            provider_timings_ms: Vec::new(),
            release_to_text_ms: None,
            error: None,
            debug_audio: None,
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
                        expired_audio.push(PathBuf::from(audio.path));
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
                    expired_audio.push(PathBuf::from(audio.path));
                }
            }
        }
        for record in &mut kept {
            if let Some(audio) = &record.debug_audio {
                if audio.is_expired(now_ms) {
                    expired_audio.push(PathBuf::from(audio.path.clone()));
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
/// audio paths that are now safe to delete.
pub struct PruneOutcome {
    pub kept: Vec<DiagnosticRecord>,
    pub expired_audio: Vec<PathBuf>,
}

fn env_parse<T: std::str::FromStr>(name: &str) -> Option<T> {
    std::env::var(name).ok().and_then(|value| value.parse().ok())
}

/// True when an environment variable name denotes a secret whose value must be
/// masked in a diagnostic export.
pub fn is_secret_env_key(key: &str) -> bool {
    const MARKERS: [&str; 6] = ["API_KEY", "TOKEN", "SECRET", "PASSWORD", "AUTHORIZATION", "CREDENTIAL"];
    let upper = key.to_ascii_uppercase();
    MARKERS.iter().any(|marker| upper.contains(marker))
}

/// A redacted, self-contained diagnostic export for one Recording. It carries
/// the local record plus only the relevant (`VOISU_`) environment, with every
/// secret value masked. Unrelated environment values are dropped entirely.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DiagnosticExport {
    pub record: DiagnosticRecord,
    pub environment: BTreeMap<String, String>,
}

/// Filters and redacts an environment for export: keeps only `VOISU_` keys
/// (dropping unrelated values) and masks any key that denotes a secret.
pub fn redacted_environment(
    vars: impl IntoIterator<Item = (String, String)>,
) -> BTreeMap<String, String> {
    vars.into_iter()
        .filter(|(key, _)| key.starts_with("VOISU_"))
        .map(|(key, value)| {
            let value = if is_secret_env_key(&key) {
                REDACTED.to_owned()
            } else {
                value
            };
            (key, value)
        })
        .collect()
}

/// Builds a redacted export from a record and the current environment.
pub fn export_record(
    record: DiagnosticRecord,
    vars: impl IntoIterator<Item = (String, String)>,
) -> DiagnosticExport {
    DiagnosticExport {
        record,
        environment: redacted_environment(vars),
    }
}

/// A bounded, private, on-disk store of correlated diagnostic records. All state
/// lives under one directory the caller has already secured; the store keeps
/// files private (0600) and never leaves the local filesystem.
pub struct DiagnosticStore {
    dir: PathBuf,
    policy: RetentionPolicy,
}

impl DiagnosticStore {
    /// Opens (creating if needed) a private diagnostics directory and its audio
    /// subdirectory, both 0700.
    pub fn open(dir: PathBuf, policy: RetentionPolicy) -> io::Result<Self> {
        create_private_dir(&dir)?;
        let store = Self { dir, policy };
        create_private_dir(&store.audio_dir())?;
        Ok(store)
    }

    pub fn audio_dir(&self) -> PathBuf {
        self.dir.join("audio")
    }

    fn history_file(&self) -> PathBuf {
        self.dir.join("history.json")
    }

    fn load_raw(&self) -> Vec<DiagnosticRecord> {
        // A missing or corrupt history file yields an empty history rather than
        // failing the daemon: local diagnostics must never block a Recording.
        match fs::read(self.history_file()) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    fn write_all(&self, records: &[DiagnosticRecord]) -> io::Result<()> {
        let encoded = serde_json::to_vec(records)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let temp = self.dir.join("history.json.tmp");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(&encoded)?;
        file.sync_all()?;
        fs::rename(&temp, self.history_file())
    }

    fn remove_audio(paths: &[PathBuf]) {
        for path in paths {
            let _ = fs::remove_file(path);
        }
    }

    /// Prunes, removes expired audio, and persists the retained records in
    /// chronological (append) order, then returns them newest first for reading.
    fn prune_and_persist(&self, records: Vec<DiagnosticRecord>) -> io::Result<Vec<DiagnosticRecord>> {
        let outcome = self.policy.prune(records, unix_millis_now());
        Self::remove_audio(&outcome.expired_audio);
        self.write_all(&outcome.kept)?;
        let mut newest_first = outcome.kept;
        newest_first.reverse();
        Ok(newest_first)
    }

    /// Appends a completed Recording's record, prunes to the retention policy,
    /// removes any now-expired debug audio, and returns the retained history
    /// (newest first).
    pub fn record(&self, record: DiagnosticRecord) -> io::Result<Vec<DiagnosticRecord>> {
        let mut records = self.load_raw();
        records.push(record);
        self.prune_and_persist(records)
    }

    /// Returns the retained history (newest first), pruning stale records and
    /// expired debug audio as a side effect so a reader never sees an entry the
    /// retention policy has already expired.
    pub fn history(&self) -> io::Result<Vec<DiagnosticRecord>> {
        let records = self.load_raw();
        self.prune_and_persist(records)
    }

    /// Finds one Recording's record by its correlation ID, after pruning.
    pub fn find(&self, correlation_id: &str) -> io::Result<Option<DiagnosticRecord>> {
        Ok(self
            .history()?
            .into_iter()
            .find(|record| record.correlation_id == correlation_id))
    }

    /// Removes expired debug audio and over-retention records. Safe to call on
    /// daemon startup so captures left by a previous run cannot linger.
    pub fn cleanup_expired(&self) -> io::Result<()> {
        self.history().map(|_| ())
    }

    /// Persists an explicit debug audio capture for a correlation ID, returning
    /// its recorded location and expiry. Only called when the user has enabled
    /// debug capture.
    pub fn store_debug_audio(
        &self,
        correlation_id: &str,
        pcm_s16le_mono_16khz: &[u8],
    ) -> io::Result<DebugAudioRecord> {
        let file_name = format!("{}.pcm", sanitize_component(correlation_id));
        let path = self.audio_dir().join(file_name);
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(pcm_s16le_mono_16khz)?;
        file.sync_all()?;
        let now = unix_millis_now();
        let ttl_ms = u64::try_from(self.policy.debug_audio_ttl.as_millis()).unwrap_or(u64::MAX);
        Ok(DebugAudioRecord {
            path: path.to_string_lossy().into_owned(),
            captured_at_unix_ms: now,
            expires_at_unix_ms: now.saturating_add(ttl_ms),
        })
    }
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
    let decision = validator.validate(completion.sources).await?;
    Ok(ReplayOutcome {
        source_transcripts,
        timings_ms: completion.timings_ms,
        decision,
    })
}
