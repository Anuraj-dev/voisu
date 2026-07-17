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
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{
    BoundaryError, CapturedAudio, DeliveryMethod, LifecycleStage, Provider, ProviderCoordinator,
    ProviderFailure, ProviderTiming, SourceTranscript, TranscriptDecision, TranscriptSelection,
    TranscriptValidator,
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
    pub delivery_method: Option<DeliveryMethod>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_fallback_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_chunk_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_finalized_ms: Option<u64>,
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
            delivery_method: None,
            delivery_fallback_reason: None,
            first_chunk_ms: None,
            capture_finalized_ms: None,
            provider_timings_ms: Vec::new(),
            provider_failures: Vec::new(),
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
    std::env::var(name).ok().and_then(|value| value.parse().ok())
}

/// True when an environment variable name denotes a secret whose value must
/// never appear in a diagnostic export, under any key.
pub fn is_secret_env_key(key: &str) -> bool {
    const MARKERS: [&str; 7] = [
        "API_KEY", "APIKEY", "TOKEN", "SECRET", "PASSWORD", "AUTHORIZATION", "CREDENTIAL",
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
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-' || character == '.')
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

/// Structurally scrubs every `http(s)://` URL embedded in a free-form string of
/// its userinfo credentials and query/fragment secrets, preserving all
/// surrounding text. This defends against secrets that reach a diagnostic
/// through a URL rather than through a secret-named environment variable.
pub fn scrub_embedded_urls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        let next = rest.find("http").filter(|&index| {
            rest[index..].starts_with("http://") || rest[index..].starts_with("https://")
        });
        match next {
            Some(index) => {
                out.push_str(&rest[..index]);
                let tail = &rest[index..];
                // A URL token runs until the first whitespace.
                let end = tail.find(char::is_whitespace).unwrap_or(tail.len());
                out.push_str(&strip_url_secrets(&tail[..end]));
                rest = &tail[end..];
            }
            None => {
                out.push_str(rest);
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
    for failure in &mut record.provider_failures {
        failure.diagnostic = scrub_free_text(&failure.diagnostic, &secrets);
    }
    DiagnosticExport {
        record,
        environment: redacted_environment(vars),
    }
}

/// A bounded, private, on-disk store of correlated diagnostic records. All state
/// lives under one directory the caller has already secured; the store keeps
/// files private (0600) and never leaves the local filesystem. One internal
/// lock serializes every load-prune-persist cycle so concurrent writers (the
/// actor answering history/export while a completed Recording persists its
/// record) can never clobber each other from stale snapshots.
pub struct DiagnosticStore {
    dir: PathBuf,
    policy: RetentionPolicy,
    lock: Mutex<()>,
    temp_counter: AtomicU64,
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
        const TEMP_CREATE_ATTEMPTS: u32 = 32;
        let encoded = serde_json::to_vec(records)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        // A unique, exclusively created temp file per write: O_EXCL refuses to
        // follow a pre-planted symlink at the temp path and the descriptor is
        // created 0600 rather than trusting pre-existing permissions. A name
        // collision (a crash leftover after PID reuse, or a planted file) is
        // NOT fatal: creation retries with a fresh nonce, bounded, so a record
        // is never lost to a stale temp file.
        let (temp, mut file) = 'created: {
            for _ in 0..TEMP_CREATE_ATTEMPTS {
                let temp = self.dir.join(format!(
                    "history.json.tmp.{}.{}",
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

    /// Prunes, removes expired audio, and persists the retained records in
    /// chronological (append) order, then returns them newest first for reading.
    /// Callers must hold the store lock.
    fn prune_and_persist(&self, records: Vec<DiagnosticRecord>) -> io::Result<Vec<DiagnosticRecord>> {
        let outcome = self.policy.prune(records, unix_millis_now());
        self.remove_audio(&outcome.expired_audio);
        self.write_all(&outcome.kept)?;
        let mut newest_first = outcome.kept;
        newest_first.reverse();
        Ok(newest_first)
    }

    /// Appends a completed Recording's record, prunes to the retention policy,
    /// removes any now-expired debug audio, and returns the retained history
    /// (newest first).
    pub fn record(&self, record: DiagnosticRecord) -> io::Result<Vec<DiagnosticRecord>> {
        let _guard = self.lock.lock().expect("diagnostics lock is not poisoned");
        let mut records = self.load_raw();
        records.push(record);
        self.prune_and_persist(records)
    }

    /// Returns the retained history (newest first), pruning stale records and
    /// expired debug audio as a side effect so a reader never sees an entry the
    /// retention policy has already expired.
    pub fn history(&self) -> io::Result<Vec<DiagnosticRecord>> {
        let _guard = self.lock.lock().expect("diagnostics lock is not poisoned");
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
            let _guard = self.lock.lock().expect("diagnostics lock is not poisoned");
            if let Ok(entries) = fs::read_dir(&self.dir) {
                for entry in entries.flatten() {
                    if entry
                        .file_name()
                        .to_str()
                        .is_some_and(|name| name.starts_with("history.json.tmp."))
                    {
                        let _ = fs::remove_file(entry.path());
                    }
                }
            }
        }
        let kept = self.history()?;
        let _guard = self.lock.lock().expect("diagnostics lock is not poisoned");
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
    let decision = validator.validate(completion.sources).await?;
    Ok(ReplayOutcome {
        source_transcripts,
        timings_ms: completion.timings_ms,
        provider_failures,
        decision,
    })
}
