//! Private audio-adjudicated eval corpus: directory layout, loader, and
//! per-case result sidecars for `score-corpus`.
//!
//! A corpus carries Raja's raw voice and his adjudicated references. It lives
//! outside the repository (default `$XDG_STATE_HOME/voisu/eval-corpus`) or at
//! a gitignored path; the CLI refuses a tracked git-tree corpus. No audio or
//! real transcript text is ever committed.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use voisu_core::{DeliveryMethod, DiagnosticRecord, Provider};

use crate::report::git_path_is_ignored;

pub const RESULT_SCHEMA: &str = "voisu-private-eval-case-result-v1";
pub const RESULT_FILE: &str = "result.json";
pub const REFERENCE_FILE: &str = "reference.txt";
pub const CASE_META_FILE: &str = "case.json";
pub const FIXTURE_FILE: &str = "fixture.pcm";

/// Mirrors the daemon replay cap (voisu-daemon `MAX_FIXTURE_BYTES`).
const MAX_FIXTURE_BYTES: u64 = 32 * 1024 * 1024;

/// Host default: `$XDG_STATE_HOME/voisu/eval-corpus` — outside the repository.
pub fn default_eval_corpus_dir() -> Result<PathBuf, String> {
    Ok(voisu_core::state_dir()?.join("eval-corpus"))
}

/// Refuses a corpus inside a git work tree unless the path is gitignored:
/// committed corpora would carry private audio and adjudicated references.
pub fn ensure_corpus_path_allowed(path: &Path) -> Result<(), String> {
    if git_path_is_ignored(path)? {
        return Ok(());
    }
    Err(format!(
        "refusing corpus {} inside a git work tree; real corpora carry private audio and references. \
Use a gitignored path (tools/transcript-quality/corpus/ is ignored) or keep the corpus outside the repository (default {})",
        path.display(),
        default_eval_corpus_dir()
            .map(|dir| dir.display().to_string())
            .unwrap_or_else(|_| "~/.local/state/voisu/eval-corpus".to_owned()),
    ))
}

/// Optional per-case metadata (`case.json`).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct CaseMeta {
    /// Must equal the case directory name when present.
    pub id: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub notes: Option<String>,
}

/// Which capture path produced a result sidecar. `replay` is reserved for a
/// later slice: the daemon replay response carries no transcript text today.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResultOrigin {
    History,
    Manual,
    Replay,
}

/// One provider Source Transcript in a result sidecar.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CaseSource {
    /// Lowercase provider name: `groq` or `deepgram`.
    pub provider: String,
    pub text: String,
}

/// Delivery outcome recorded with a result. `fallback_reason` is private to
/// the sidecar and never copied into a scored run JSON.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CaseDelivery {
    pub delivered: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
}

/// Stop-anchored telemetry carried with a result (schema 2 fields).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CaseTelemetry {
    #[serde(default)]
    pub telemetry_schema: u32,
    #[serde(default)]
    pub recording_duration_ms: Option<u64>,
    #[serde(default)]
    pub stop_to_finalized_ms: Option<u64>,
    #[serde(default)]
    pub stop_to_delivered_ms: Option<u64>,
}

/// The captured pipeline result for one case (`result.json` sidecar).
///
/// The stable fields later slices diff against; transcript text stays in the
/// private sidecar and never enters a scored run JSON.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CaseResult {
    pub schema: String,
    pub case_id: String,
    pub origin: ResultOrigin,
    pub captured_at_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    #[serde(default)]
    pub source_transcripts: Vec<CaseSource>,
    #[serde(default)]
    pub final_transcript: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub delivery: Option<CaseDelivery>,
    #[serde(default)]
    pub telemetry: Option<CaseTelemetry>,
}

/// One loaded, validated corpus case.
#[derive(Clone, Debug)]
pub struct CorpusCase {
    pub id: String,
    pub dir: PathBuf,
    pub reference: String,
    pub tags: Vec<String>,
    pub notes: Option<String>,
    pub result: Option<CaseResult>,
    /// Raw s16le/mono/16 kHz PCM fixture for the daemon replay path.
    pub fixture: Option<PathBuf>,
}

/// Loads and validates every case under `dir` (sorted by case id).
///
/// Fail-closed: a missing or empty `reference.txt`, an unsafe case name, or a
/// malformed `case.json` / `result.json` is an error naming the case.
pub fn load_corpus(dir: &Path) -> Result<Vec<CorpusCase>, String> {
    let metadata = fs::symlink_metadata(dir)
        .map_err(|err| format!("cannot read corpus {}: {err}", dir.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!("corpus path is not a directory: {}", dir.display()));
    }
    let mut names = Vec::new();
    for entry in
        fs::read_dir(dir).map_err(|err| format!("cannot read corpus {}: {err}", dir.display()))?
    {
        let entry = entry.map_err(|err| format!("cannot read corpus entry: {err}"))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry
            .file_type()
            .map_err(|err| format!("cannot stat {}: {err}", entry.path().display()))?
            .is_file()
            || name.starts_with('.')
        {
            continue;
        }
        if !is_safe_case_name(&name) {
            return Err(format!(
                "corpus case name {name:?} is not a safe component (use [A-Za-z0-9_-] only) under {}",
                dir.display()
            ));
        }
        names.push(name);
    }
    names.sort();
    names
        .into_iter()
        .map(|name| load_case(dir, &name))
        .collect()
}

fn load_case(corpus_dir: &Path, id: &str) -> Result<CorpusCase, String> {
    let dir = corpus_dir.join(id);
    let reference_path = dir.join(REFERENCE_FILE);
    if fs::symlink_metadata(&reference_path).is_err() {
        return Err(format!(
            "case {id}: missing {REFERENCE_FILE}; the adjudicated ground truth is required"
        ));
    }
    let reference =
        read_regular_text(&reference_path).map_err(|err| format!("case {id}: {err}"))?;
    if reference.trim().is_empty() {
        return Err(format!(
            "case {id}: {} is empty; the adjudicated ground truth is required",
            reference_path.display()
        ));
    }
    let meta = match read_regular_text(&dir.join(CASE_META_FILE)) {
        Ok(text) => {
            let meta: CaseMeta = serde_json::from_str(&text)
                .map_err(|err| format!("case {id}: invalid {CASE_META_FILE}: {err}"))?;
            if let Some(meta_id) = &meta.id
                && meta_id != id
            {
                return Err(format!(
                    "case {id}: {CASE_META_FILE} declares id {meta_id:?}; it must match the directory name"
                ));
            }
            meta
        }
        Err(_) => CaseMeta::default(),
    };
    let result = match read_regular_text(&dir.join(RESULT_FILE)) {
        Ok(text) => Some(parse_case_result(&text, id)?),
        Err(_) => None,
    };
    let fixture_path = dir.join(FIXTURE_FILE);
    let fixture = match fs::symlink_metadata(&fixture_path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(format!(
                    "case {id}: {} is not a regular file",
                    fixture_path.display()
                ));
            }
            if metadata.len() == 0 {
                return Err(format!(
                    "case {id}: {} is empty; a replay fixture is raw s16le/mono/16 kHz PCM",
                    fixture_path.display()
                ));
            }
            if metadata.len() > MAX_FIXTURE_BYTES {
                return Err(format!(
                    "case {id}: {} is larger than the {} byte daemon replay cap",
                    fixture_path.display(),
                    MAX_FIXTURE_BYTES
                ));
            }
            Some(fixture_path)
        }
        Err(_) => None,
    };
    Ok(CorpusCase {
        id: id.to_owned(),
        dir,
        reference,
        tags: meta.tags,
        notes: meta.notes,
        result,
        fixture,
    })
}

/// Parses and validates one result sidecar against its case id.
pub fn parse_case_result(text: &str, case_id: &str) -> Result<CaseResult, String> {
    let mut result: CaseResult = serde_json::from_str(text)
        .map_err(|err| format!("case {case_id}: invalid {RESULT_FILE}: {err}"))?;
    if result.schema != RESULT_SCHEMA {
        return Err(format!(
            "case {case_id}: {RESULT_FILE} schema {:?} is not {RESULT_SCHEMA:?}",
            result.schema
        ));
    }
    if result.case_id != case_id {
        return Err(format!(
            "case {case_id}: {RESULT_FILE} declares case_id {:?}",
            result.case_id
        ));
    }
    for source in &mut result.source_transcripts {
        source.provider = source.provider.trim().to_ascii_lowercase();
        if !matches!(source.provider.as_str(), "groq" | "deepgram") {
            return Err(format!(
                "case {case_id}: {RESULT_FILE} source provider {:?} is not groq or deepgram",
                source.provider
            ));
        }
    }
    Ok(result)
}

/// Reads `history.jsonl` (or a `voisu history --json` array), filters to the
/// requested correlation IDs, and writes one private `result.json` sidecar per
/// record under `<corpus>/<case-id>/`. Raw audio and references are untouched.
pub fn capture_results(
    history_path: &Path,
    corpus_dir: &Path,
    ids: &[String],
    now_ms: u64,
) -> Result<CaptureSummary, String> {
    let records = load_history_records(history_path)?;
    let selected: Vec<&DiagnosticRecord> = if ids.is_empty() {
        records.iter().collect()
    } else {
        ids.iter()
            .map(|id| {
                records
                    .iter()
                    .find(|record| &record.correlation_id == id)
                    .ok_or_else(|| {
                        format!(
                            "no history record for {id:?}; {} usable record(s) were loaded",
                            records.len()
                        )
                    })
            })
            .collect::<Result<Vec<_>, String>>()?
    };
    if selected.is_empty() {
        return Err(format!(
            "no usable records in {}; run dictations first or pass --history",
            history_path.display()
        ));
    }
    let mut summary = CaptureSummary::default();
    for record in selected {
        let case_id = sanitize_component(&record.correlation_id);
        let entry_dir = corpus_dir.join(&case_id);
        create_private_dir(&entry_dir)?;
        let result = case_result_from_record(record, &case_id, now_ms);
        let json = serde_json::to_vec_pretty(&result)
            .map_err(|err| format!("case {case_id}: cannot serialize result: {err}"))?;
        write_private_replace(&entry_dir.join(RESULT_FILE), &json)?;
        if !entry_dir.join(REFERENCE_FILE).is_file() {
            summary
                .warnings
                .push(format!("case {case_id}: no {REFERENCE_FILE} yet; add the adjudicated reference before scoring"));
        }
        summary.written.push(case_id);
    }
    Ok(summary)
}

#[derive(Clone, Debug, Default)]
pub struct CaptureSummary {
    pub written: Vec<String>,
    pub warnings: Vec<String>,
}

impl CaptureSummary {
    pub fn render(&self, corpus_dir: &Path) -> String {
        let mut out = String::new();
        for case in &self.written {
            out.push_str(&format!("wrote {}/{}\n", corpus_dir.display(), case));
        }
        for warning in &self.warnings {
            out.push_str(&format!("warning: {warning}\n"));
        }
        out
    }
}

fn case_result_from_record(record: &DiagnosticRecord, case_id: &str, now_ms: u64) -> CaseResult {
    let final_transcript = record
        .final_transcript
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned);
    let delivered = record.delivery_count > 0 || record.delivery_method.is_some();
    CaseResult {
        schema: RESULT_SCHEMA.to_owned(),
        case_id: case_id.to_owned(),
        origin: ResultOrigin::History,
        captured_at_unix_ms: now_ms,
        correlation_id: Some(record.correlation_id.clone()),
        source_transcripts: record
            .source_transcripts
            .iter()
            .map(|source| CaseSource {
                provider: provider_name(source.provider).to_owned(),
                text: source.text.clone(),
            })
            .collect(),
        final_transcript,
        error: record.error.clone(),
        delivery: Some(CaseDelivery {
            delivered,
            method: record
                .delivery_method
                .map(delivery_method_name)
                .map(str::to_owned),
            fallback_reason: record.delivery_fallback_reason.clone(),
        }),
        telemetry: Some(CaseTelemetry {
            telemetry_schema: record.telemetry_schema,
            recording_duration_ms: record.recording_duration_ms,
            stop_to_finalized_ms: record.stop_to_finalized_ms,
            stop_to_delivered_ms: record.stop_to_delivered_ms,
        }),
    }
}

/// Loads a `history.jsonl` file or a `voisu history --json` array.
fn load_history_records(path: &Path) -> Result<Vec<DiagnosticRecord>, String> {
    let text = read_regular_text(path).map_err(|err| format!("cannot read history: {err}"))?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    if trimmed.starts_with('[') {
        return serde_json::from_str(trimmed).map_err(|err| format!("history JSON array: {err}"));
    }
    let mut records = Vec::new();
    for (idx, line) in trimmed.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let record: DiagnosticRecord = serde_json::from_str(line)
            .map_err(|err| format!("history JSONL line {}: {err}", idx + 1))?;
        records.push(record);
    }
    Ok(records)
}

fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Deepgram => "deepgram",
        Provider::Groq => "groq",
    }
}

fn delivery_method_name(method: DeliveryMethod) -> &'static str {
    match method {
        DeliveryMethod::CompositorSubmitted => "compositor_submitted",
        DeliveryMethod::ClipboardFallback => "clipboard_fallback",
    }
}

fn read_regular_text(path: &Path) -> Result<String, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!("not a regular file: {}", path.display()));
    }
    fs::read_to_string(path).map_err(|err| format!("cannot read {}: {err}", path.display()))
}

fn is_safe_case_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn sanitize_component(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "_".to_owned()
    } else {
        sanitized
    }
}

fn create_private_dir(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(format!(
                    "corpus path is not a directory: {}",
                    path.display()
                ));
            }
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                .map_err(|err| format!("cannot set permissions on {}: {err}", path.display()))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
                && !parent.exists()
            {
                create_private_dir(parent)?;
            }
            DirBuilder::new()
                .mode(0o700)
                .create(path)
                .map_err(|err| format!("cannot create {}: {err}", path.display()))
        }
        Err(err) => Err(format!("cannot create {}: {err}", path.display())),
    }
}

fn write_private_replace(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
    {
        return Err(format!("refusing to follow symlink {}", path.display()));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|err| format!("cannot write {}: {err}", path.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|err| format!("cannot write {}: {err}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|err| format!("cannot set permissions on {}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_names_match_core_serialization() {
        assert_eq!(provider_name(Provider::Groq), "groq");
        assert_eq!(provider_name(Provider::Deepgram), "deepgram");
    }

    #[test]
    fn delivery_method_names_match_core_serialization() {
        assert_eq!(
            delivery_method_name(DeliveryMethod::ClipboardFallback),
            "clipboard_fallback"
        );
        assert_eq!(
            delivery_method_name(DeliveryMethod::CompositorSubmitted),
            "compositor_submitted"
        );
    }

    #[test]
    fn unsafe_case_names_are_rejected() {
        assert!(is_safe_case_name("alpha-01_intro"));
        assert!(!is_safe_case_name("has space"));
        assert!(!is_safe_case_name("../escape"));
        assert!(!is_safe_case_name(""));
    }
}
