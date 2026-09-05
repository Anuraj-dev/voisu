//! Private host tool: label the newest completed Recording and promote its
//! expiring debug audio into the local controlled corpus.
//!
//! Not packaged. Does not upload, and refuses to write under a git work tree.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use voisu_core::{
    Command, DaemonState, DebugAudioRecord, DiagnosticRecord, LifecycleStage, Provider,
    ProviderTiming, Request, Response, TranscriptSelection, is_secret_env_key, scrub_embedded_urls,
    scrub_secret_values, socket_path, unix_millis_now,
};

const USAGE: &str = "\
mark-last - promote the newest completed Recording into the local corpus

USAGE:
    mark-last good|bad [--note <text>]
    mark-last attach-reference <correlation-id> --file <path>

    cargo run --manifest-path tools/transcript-quality/Cargo.toml --bin mark-last -- \\
      good --note 'dropped the prefix'

Absent from `voisu --help`, packages, and service units. Evidence stays local.

good|bad              Label for the newest completed Recording
--note                Optional free-form note stored with the label
--diagnostics-dir     Default: $XDG_STATE_HOME/voisu/diagnostics
--corpus-dir          Default: $XDG_STATE_HOME/voisu/dev-audio/promoted
attach-reference      Write an adjudicated reference without replacing raw evidence
--help                Print this help
";

const SNAPSHOT_SCHEMA: &str = "voisu-private-promoted-evidence-v1";
const CHECKSUM_FILE: &str = "checksum.sha256";
const AUDIO_FILE: &str = "audio.pcm";
const SNAPSHOT_FILE: &str = "snapshot.json";
const LABEL_FILE: &str = "label.json";
const REFERENCE_FILE: &str = "reference.txt";
const REFERENCE_META_FILE: &str = "reference.json";
const HISTORY_FILE: &str = "history.jsonl";

/// Host default: `~/.local/state/voisu/diagnostics`.
pub fn default_diagnostics_dir() -> Result<PathBuf, String> {
    Ok(voisu_core::state_dir()?.join("diagnostics"))
}

/// Host default: `~/.local/state/voisu/dev-audio/promoted`.
pub fn default_corpus_dir() -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("VOISU_DEV_AUDIO_DIR") {
        let path = PathBuf::from(dir);
        if !path.is_absolute() {
            return Err("VOISU_DEV_AUDIO_DIR must be an absolute path".to_owned());
        }
        return Ok(path);
    }
    Ok(voisu_core::state_dir()?.join("dev-audio").join("promoted"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Label {
    Good,
    Bad,
}

impl Label {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Good => "good",
            Self::Bad => "bad",
        }
    }
}

impl std::fmt::Display for Label {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingActivity {
    Idle,
    Active,
}

pub struct MarkConfig<'a> {
    pub diagnostics_dir: &'a Path,
    pub corpus_dir: &'a Path,
    pub activity: RecordingActivity,
    pub now_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Promotion {
    pub correlation_id: String,
    pub label: Label,
    pub already_present: bool,
    pub entry_dir: PathBuf,
    pub corpus_dir: PathBuf,
    pub checksum: String,
    pub disk_bytes: u64,
    pub recording_count: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LabelRecord {
    pub label: Label,
    pub marked_at_unix_ms: u64,
    pub correlation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReferenceMeta {
    pub kind: String,
    pub attached_at_unix_ms: u64,
    pub correlation_id: String,
}

#[derive(Serialize)]
struct EvidenceSnapshot {
    schema: &'static str,
    correlation_id: String,
    recording_id: u64,
    recorded_at_unix_ms: u64,
    source_transcripts: Vec<SourceSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    final_transcript: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reconstruction_candidate: Option<String>,
    decision: DecisionEvidence,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_id: Option<String>,
    timing: TimingSnapshot,
    stages: Vec<LifecycleStage>,
    audio: AudioSnapshot,
}

#[derive(Serialize)]
struct SourceSnapshot {
    provider: Provider,
    text: String,
}

#[derive(Serialize)]
struct DecisionEvidence {
    #[serde(skip_serializing_if = "Option::is_none")]
    selection: Option<TranscriptSelection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    validation_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fallback_reason: Option<String>,
    reconciliation_requested: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    dpr: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct TimingSnapshot {
    #[serde(skip_serializing_if = "Option::is_none")]
    first_chunk_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    capture_finalized_ms: Option<u64>,
    provider_timings_ms: Vec<ProviderTiming>,
    #[serde(skip_serializing_if = "Option::is_none")]
    release_to_text_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    formatter_latency_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    http_latency_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total_gate_latency_ms: Option<u64>,
}

#[derive(Serialize)]
struct AudioSnapshot {
    source_file_name: String,
    checksum: String,
    bytes: u64,
    captured_at_unix_ms: u64,
    expires_at_unix_ms: u64,
}

/// CLI entry for the private `mark-last` binary.
pub fn run_mark_last(args: impl IntoIterator<Item = impl AsRef<str>>) -> Result<(), String> {
    let parsed = parse_mark_args(args)?;
    if parsed.help {
        print!("{USAGE}");
        return Ok(());
    }
    let diagnostics_dir = match parsed.diagnostics_dir {
        Some(dir) => dir,
        None => default_diagnostics_dir()?,
    };
    let corpus_dir = match parsed.corpus_dir {
        Some(dir) => dir,
        None => default_corpus_dir()?,
    };
    match parsed.command {
        None => Err("missing good|bad or attach-reference\n\n{USAGE}".replace("{USAGE}", USAGE)),
        Some(MarkCommand::Mark { label, note }) => {
            let promotion = mark_last(
                &MarkConfig {
                    diagnostics_dir: &diagnostics_dir,
                    corpus_dir: &corpus_dir,
                    activity: probe_daemon_activity(),
                    now_ms: unix_millis_now(),
                },
                label,
                note,
            )?;
            print!("{}", render_promotion(&promotion));
            Ok(())
        }
        Some(MarkCommand::AttachReference {
            correlation_id,
            file,
        }) => {
            let text = fs::read_to_string(&file)
                .map_err(|err| format!("cannot read reference {}: {err}", file.display()))?;
            attach_reference(&corpus_dir, &correlation_id, &text, unix_millis_now())?;
            println!(
                "attached adjudicated reference for {correlation_id} under {}",
                corpus_dir.display()
            );
            Ok(())
        }
    }
}

pub fn mark_last(
    config: &MarkConfig<'_>,
    label: Label,
    note: Option<String>,
) -> Result<Promotion, String> {
    refuse_git_tree(config.corpus_dir)?;
    if config.activity == RecordingActivity::Active {
        return Err("a Recording is active; wait until it completes".to_owned());
    }

    let records = load_history(config.diagnostics_dir)?;
    let record = newest_completed(&records)
        .ok_or_else(|| "no completed Recording in diagnostic history".to_owned())?;
    let debug_audio = record.debug_audio.as_ref().ok_or_else(|| {
        format!(
            "audio already missing for {}; enable debug capture before the Recording",
            record.correlation_id
        )
    })?;
    let source_audio = audio_path(config.diagnostics_dir, debug_audio)?;
    if !source_audio.is_file() {
        return Err(format!(
            "audio already missing for {} ({})",
            record.correlation_id,
            source_audio.display()
        ));
    }

    let bytes = read_regular_file(&source_audio)?;
    let checksum = sha256_fingerprint(&bytes);
    let entry_dir = config
        .corpus_dir
        .join(sanitize_component(&record.correlation_id));
    refuse_git_tree(&entry_dir)?;
    create_private_dir(config.corpus_dir)?;
    create_private_dir(&entry_dir)?;

    let dest_audio = entry_dir.join(AUDIO_FILE);
    let already_present = dest_audio.is_file();
    if already_present {
        let existing = read_regular_file(&dest_audio)?;
        if sha256_fingerprint(&existing) != checksum {
            return Err(format!(
                "corpus entry for {} already exists with a different checksum; not replacing raw evidence",
                record.correlation_id
            ));
        }
        ensure_private_file(&dest_audio)?;
    } else {
        write_private_new(&dest_audio, &bytes)?;
    }

    let snapshot_path = entry_dir.join(SNAPSHOT_FILE);
    if snapshot_path.is_file() {
        ensure_private_file(&snapshot_path)?;
    } else {
        let snapshot = build_snapshot(record, debug_audio, &checksum, bytes.len() as u64)?;
        let json = serde_json::to_vec_pretty(&snapshot)
            .map_err(|err| format!("cannot serialize snapshot: {err}"))?;
        write_private_new(&snapshot_path, &json)?;
    }

    let checksum_path = entry_dir.join(CHECKSUM_FILE);
    if checksum_path.is_file() {
        ensure_private_file(&checksum_path)?;
    } else {
        let line = format!("{checksum}  {AUDIO_FILE}\n");
        write_private_new(&checksum_path, line.as_bytes())?;
    }

    write_label(
        &entry_dir,
        record,
        label,
        note,
        config.now_ms,
        already_present,
    )?;

    let (disk_bytes, recording_count) = corpus_usage(config.corpus_dir)?;
    Ok(Promotion {
        correlation_id: record.correlation_id.clone(),
        label,
        already_present,
        entry_dir,
        corpus_dir: config.corpus_dir.to_path_buf(),
        checksum,
        disk_bytes,
        recording_count,
    })
}

/// Writes an adjudicated reference beside existing raw evidence. Never replaces
/// `audio.pcm`, `checksum.sha256`, or `snapshot.json`.
pub fn attach_reference(
    corpus_dir: &Path,
    correlation_id: &str,
    text: &str,
    now_ms: u64,
) -> Result<PathBuf, String> {
    refuse_git_tree(corpus_dir)?;
    let entry_dir = corpus_dir.join(sanitize_component(correlation_id));
    refuse_git_tree(&entry_dir)?;
    if !entry_dir.join(SNAPSHOT_FILE).is_file() && !entry_dir.join(AUDIO_FILE).is_file() {
        return Err(format!(
            "no promoted evidence for {correlation_id} under {}",
            corpus_dir.display()
        ));
    }
    let secrets = secret_values_from_env();
    let scrubbed = scrub_text(text, &secrets);
    write_private_replace(&entry_dir.join(REFERENCE_FILE), scrubbed.as_bytes())?;
    let meta = ReferenceMeta {
        kind: "adjudicated".to_owned(),
        attached_at_unix_ms: now_ms,
        correlation_id: correlation_id.to_owned(),
    };
    let json = serde_json::to_vec_pretty(&meta)
        .map_err(|err| format!("cannot serialize reference metadata: {err}"))?;
    write_private_replace(&entry_dir.join(REFERENCE_META_FILE), &json)?;
    Ok(entry_dir)
}

pub fn probe_daemon_activity() -> RecordingActivity {
    match daemon_state() {
        Some(DaemonState::Recording | DaemonState::Processing) => RecordingActivity::Active,
        _ => RecordingActivity::Idle,
    }
}

pub fn render_promotion(promotion: &Promotion) -> String {
    let verb = if promotion.already_present {
        "already marked"
    } else {
        "marked"
    };
    format!(
        "{verb} {} as {}\nchecksum: {}\ncorpus: {}\ndisk: {} ({} Recording{})\n",
        promotion.correlation_id,
        promotion.label,
        promotion.checksum,
        promotion.corpus_dir.display(),
        format_bytes(promotion.disk_bytes),
        promotion.recording_count,
        if promotion.recording_count == 1 {
            ""
        } else {
            "s"
        },
    )
}

pub fn newest_completed(records: &[DiagnosticRecord]) -> Option<&DiagnosticRecord> {
    records.iter().max_by(|left, right| {
        left.recorded_at_unix_ms
            .cmp(&right.recorded_at_unix_ms)
            .then(left.recording_id.cmp(&right.recording_id))
    })
}

struct ParsedMarkArgs {
    command: Option<MarkCommand>,
    diagnostics_dir: Option<PathBuf>,
    corpus_dir: Option<PathBuf>,
    help: bool,
}

enum MarkCommand {
    Mark {
        label: Label,
        note: Option<String>,
    },
    AttachReference {
        correlation_id: String,
        file: PathBuf,
    },
}

fn parse_mark_args(
    args: impl IntoIterator<Item = impl AsRef<str>>,
) -> Result<ParsedMarkArgs, String> {
    let mut command = None;
    let mut diagnostics_dir = None;
    let mut corpus_dir = None;
    let mut note = None;
    let mut help = false;
    let mut positional = Vec::new();
    let mut iter = args.into_iter();
    while let Some(raw) = iter.next() {
        let arg = raw.as_ref();
        match arg {
            "--help" | "-h" => help = true,
            "--note" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "missing value for --note".to_owned())?;
                note = Some(value.as_ref().to_owned());
            }
            "--diagnostics-dir" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "missing value for --diagnostics-dir".to_owned())?;
                diagnostics_dir = Some(PathBuf::from(value.as_ref()));
            }
            "--corpus-dir" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "missing value for --corpus-dir".to_owned())?;
                corpus_dir = Some(PathBuf::from(value.as_ref()));
            }
            "--file" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "missing value for --file".to_owned())?;
                positional.push(format!("--file={}", value.as_ref()));
            }
            other if other.starts_with("--note=") => {
                note = Some(other["--note=".len()..].to_owned());
            }
            other if other.starts_with("--diagnostics-dir=") => {
                diagnostics_dir = Some(PathBuf::from(&other["--diagnostics-dir=".len()..]));
            }
            other if other.starts_with("--corpus-dir=") => {
                corpus_dir = Some(PathBuf::from(&other["--corpus-dir=".len()..]));
            }
            other if other.starts_with("--file=") => {
                positional.push(other.to_owned());
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown argument: {other}\n\n{USAGE}"));
            }
            other => positional.push(other.to_owned()),
        }
    }

    if !help {
        command = Some(parse_command(positional, note)?);
    }
    Ok(ParsedMarkArgs {
        command,
        diagnostics_dir,
        corpus_dir,
        help,
    })
}

fn parse_command(positional: Vec<String>, note: Option<String>) -> Result<MarkCommand, String> {
    let mut extra_file = None;
    let mut words = Vec::new();
    for item in positional {
        if let Some(path) = item.strip_prefix("--file=") {
            extra_file = Some(PathBuf::from(path));
        } else {
            words.push(item);
        }
    }
    match words.first().map(String::as_str) {
        Some("good") | Some("bad") => {
            if words.len() != 1 {
                return Err(format!("unexpected argument: {}\n\n{USAGE}", words[1]));
            }
            if extra_file.is_some() {
                return Err("--file is only valid with attach-reference".to_owned());
            }
            let label = if words[0] == "good" {
                Label::Good
            } else {
                Label::Bad
            };
            Ok(MarkCommand::Mark { label, note })
        }
        Some("attach-reference") => {
            if words.len() != 2 {
                return Err(
                    "attach-reference requires <correlation-id> and --file <path>".to_owned(),
                );
            }
            let file = extra_file.ok_or_else(|| "missing --file <path>".to_owned())?;
            Ok(MarkCommand::AttachReference {
                correlation_id: words[1].clone(),
                file,
            })
        }
        Some(other) => Err(format!("unknown command: {other}\n\n{USAGE}")),
        None => Err(format!("missing good|bad or attach-reference\n\n{USAGE}")),
    }
}

fn load_history(diagnostics_dir: &Path) -> Result<Vec<DiagnosticRecord>, String> {
    let path = diagnostics_dir.join(HISTORY_FILE);
    let metadata = fs::symlink_metadata(&path).map_err(|err| {
        if err.kind() == io::ErrorKind::NotFound {
            format!("no diagnostic history at {}", path.display())
        } else {
            format!("cannot read {}: {err}", path.display())
        }
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "diagnostic history is not a regular file: {}",
            path.display()
        ));
    }
    let bytes = fs::read(&path).map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    Ok(bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_slice(line).ok())
        .collect())
}

fn audio_path(diagnostics_dir: &Path, audio: &DebugAudioRecord) -> Result<PathBuf, String> {
    if !is_safe_file_name(&audio.file_name) {
        return Err(format!(
            "debug audio file name is not a safe basename: {}",
            audio.file_name
        ));
    }
    Ok(diagnostics_dir.join("audio").join(&audio.file_name))
}

fn build_snapshot(
    record: &DiagnosticRecord,
    debug_audio: &DebugAudioRecord,
    checksum: &str,
    bytes: u64,
) -> Result<EvidenceSnapshot, String> {
    let secrets = secret_values_from_env();
    let source_transcripts = record
        .source_transcripts
        .iter()
        .map(|source| SourceSnapshot {
            provider: source.provider,
            text: scrub_text(&source.text, &secrets),
        })
        .collect();
    let final_transcript = record
        .final_transcript
        .as_ref()
        .map(|text| scrub_text(text, &secrets));
    let reconstruction_candidate =
        reconstruction_candidate(record).map(|text| scrub_text(&text, &secrets));
    let model_id = record
        .smart_writing
        .as_ref()
        .and_then(|smart| smart.model_id.clone())
        .map(|id| scrub_text(&id, &secrets));
    let dpr = match record.dpr.as_ref() {
        Some(dpr) => Some(
            serde_json::to_value(dpr)
                .map_err(|err| format!("cannot snapshot decision evidence: {err}"))?,
        ),
        None => None,
    };
    Ok(EvidenceSnapshot {
        schema: SNAPSHOT_SCHEMA,
        correlation_id: record.correlation_id.clone(),
        recording_id: record.recording_id,
        recorded_at_unix_ms: record.recorded_at_unix_ms,
        source_transcripts,
        final_transcript,
        reconstruction_candidate,
        decision: DecisionEvidence {
            selection: record.selection,
            validation_reason: record
                .validation_reason
                .as_ref()
                .map(|text| scrub_text(text, &secrets)),
            fallback_reason: record
                .fallback_reason
                .as_ref()
                .map(|text| scrub_text(text, &secrets)),
            reconciliation_requested: record.reconciliation_requested,
            dpr,
        },
        model_id,
        timing: TimingSnapshot {
            first_chunk_ms: record.first_chunk_ms,
            capture_finalized_ms: record.capture_finalized_ms,
            provider_timings_ms: record.provider_timings_ms.clone(),
            release_to_text_ms: record.release_to_text_ms,
            formatter_latency_ms: record
                .smart_writing
                .as_ref()
                .and_then(|smart| smart.formatter_latency_ms),
            http_latency_ms: record
                .smart_writing
                .as_ref()
                .and_then(|smart| smart.http_latency_ms),
            total_gate_latency_ms: record
                .smart_writing
                .as_ref()
                .and_then(|smart| smart.total_gate_latency_ms),
        },
        stages: record.stages.clone(),
        audio: AudioSnapshot {
            source_file_name: debug_audio.file_name.clone(),
            checksum: checksum.to_owned(),
            bytes,
            captured_at_unix_ms: debug_audio.captured_at_unix_ms,
            expires_at_unix_ms: debug_audio.expires_at_unix_ms,
        },
    })
}

fn reconstruction_candidate(record: &DiagnosticRecord) -> Option<String> {
    let dpr = record.dpr.as_ref()?;
    let value = serde_json::to_value(dpr).ok()?;
    value
        .pointer("/late_evaluation/candidate_text_clamped")
        .and_then(|value| value.as_str())
        .map(str::to_owned)
}

fn write_label(
    entry_dir: &Path,
    record: &DiagnosticRecord,
    label: Label,
    note: Option<String>,
    now_ms: u64,
    already_present: bool,
) -> Result<(), String> {
    let path = entry_dir.join(LABEL_FILE);
    if already_present
        && path.is_file()
        && let Ok(existing) = fs::read_to_string(&path)
        && let Ok(previous) = serde_json::from_str::<LabelRecord>(&existing)
        && previous.label == label
        && previous.note == note
    {
        return Ok(());
    }
    let secrets = secret_values_from_env();
    let record = LabelRecord {
        label,
        marked_at_unix_ms: now_ms,
        correlation_id: record.correlation_id.clone(),
        note: note.map(|text| scrub_text(&text, &secrets)),
    };
    let json = serde_json::to_vec_pretty(&record)
        .map_err(|err| format!("cannot serialize label: {err}"))?;
    write_private_replace(&path, &json)
}

fn corpus_usage(corpus_dir: &Path) -> Result<(u64, usize), String> {
    let mut bytes = 0_u64;
    let mut recordings = 0_usize;
    let entries = match fs::read_dir(corpus_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(err) => {
            return Err(format!(
                "cannot read corpus {}: {err}",
                corpus_dir.display()
            ));
        }
    };
    for entry in entries {
        let entry = entry.map_err(|err| format!("cannot read corpus entry: {err}"))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|err| format!("cannot stat {}: {err}", path.display()))?;
        if file_type.is_dir() {
            if path.join(SNAPSHOT_FILE).is_file() || path.join(AUDIO_FILE).is_file() {
                recordings += 1;
            }
            let (child_bytes, _) = corpus_usage(&path)?;
            bytes = bytes.saturating_add(child_bytes);
        } else if file_type.is_file() {
            bytes = bytes.saturating_add(entry.metadata().map(|meta| meta.len()).unwrap_or(0));
        }
    }
    Ok((bytes, recordings))
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if (bytes as f64) < MIB {
        format!("{:.1} KiB", bytes as f64 / KIB)
    } else {
        format!("{:.1} MiB", bytes as f64 / MIB)
    }
}

fn daemon_state() -> Option<DaemonState> {
    let path = socket_path().ok()?;
    let mut stream = UnixStream::connect(path).ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(1)))
        .ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(1))).ok()?;
    serde_json::to_writer(&mut stream, &Request::new(Command::Status)).ok()?;
    stream.write_all(b"\n").ok()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    let response: Response = serde_json::from_str(&line).ok()?;
    response.state
}

fn refuse_git_tree(path: &Path) -> Result<(), String> {
    let abs = normalize_path(path);
    if let Some(root) = git_root_of(&abs) {
        return Err(format!(
            "refusing to write corpus {} under git work tree {}; use --corpus-dir outside the repository (default ~/.local/state/voisu/dev-audio/promoted)",
            abs.display(),
            root.display()
        ));
    }
    Ok(())
}

fn git_root_of(path: &Path) -> Option<PathBuf> {
    // Resolve symlinks first so `/tmp/link -> repo/tools` still refuses.
    let resolved = resolve_path(path);
    let mut dir = if resolved.is_dir() {
        resolved
    } else {
        match resolved.parent() {
            Some(parent) => parent.to_path_buf(),
            None => resolved,
        }
    };
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn resolve_path(path: &Path) -> PathBuf {
    resolve_path_depth(path, 0)
}

fn resolve_path_depth(path: &Path, depth: usize) -> PathBuf {
    if depth > 8 {
        return normalize_path(path);
    }
    let abs = normalize_path(path);
    if let Ok(canon) = fs::canonicalize(&abs) {
        return canon;
    }
    if let Ok(meta) = fs::symlink_metadata(&abs) {
        if meta.file_type().is_symlink()
            && let Ok(target) = fs::read_link(&abs)
        {
            let joined = if target.is_absolute() {
                normalize_path(&target)
            } else if let Some(parent) = abs.parent() {
                normalize_path(&parent.join(target))
            } else {
                normalize_path(&target)
            };
            if joined != abs {
                return resolve_path_depth(&joined, depth + 1);
            }
        }
        // Exists, but canonicalize failed (permissions, etc.). Do not recurse.
        return abs;
    }
    let mut current = abs;
    let mut missing = Vec::new();
    while !current.as_os_str().is_empty() {
        if fs::symlink_metadata(&current).is_ok() {
            break;
        }
        match current.file_name() {
            Some(name) => missing.push(name.to_os_string()),
            None => break,
        }
        if !current.pop() {
            break;
        }
    }
    let mut resolved = if current.as_os_str().is_empty() {
        normalize_path(path)
    } else {
        resolve_path_depth(&current, depth + 1)
    };
    for name in missing.into_iter().rev() {
        resolved.push(name);
    }
    resolved
}

fn normalize_path(path: &Path) -> PathBuf {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            Err(_) => path.to_path_buf(),
        }
    };
    let mut out = PathBuf::new();
    for component in abs.components() {
        match component {
            std::path::Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            std::path::Component::RootDir => out.push(component.as_os_str()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::Normal(part) => out.push(part),
        }
    }
    out
}

fn create_private_dir(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(format!(
                    "corpus path is not a private directory: {}",
                    path.display()
                ));
            }
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                .map_err(|err| format!("cannot set permissions on {}: {err}", path.display()))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
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

fn read_regular_file(path: &Path) -> Result<Vec<u8>, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!("not a regular file: {}", path.display()));
    }
    fs::read(path).map_err(|err| format!("cannot read {}: {err}", path.display()))
}

fn ensure_private_file(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|err| format!("cannot set permissions on {}: {err}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!("not a regular file: {}", path.display()));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|err| format!("cannot set permissions on {}: {err}", path.display()))
}

fn write_private_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|err| format!("cannot create {}: {err}", path.display()))?;
    finish_private_write(path, &mut file, bytes)
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
    finish_private_write(path, &mut file, bytes)
}

fn finish_private_write(path: &Path, file: &mut File, bytes: &[u8]) -> Result<(), String> {
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|err| format!("cannot write {}: {err}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|err| format!("cannot set permissions on {}: {err}", path.display()))
}

fn sha256_fingerprint(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(7 + 64);
    out.push_str("sha256:");
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn secret_values_from_env() -> Vec<String> {
    std::env::vars()
        .filter(|(key, value)| is_secret_env_key(key) && !value.is_empty())
        .map(|(_, value)| value)
        .collect()
}

fn scrub_text(text: &str, secrets: &[String]) -> String {
    scrub_embedded_urls(&scrub_secret_values(text, secrets))
}

fn is_safe_file_name(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains('/')
        && !value.contains('\\')
        && !value.contains('\0')
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_good_with_note() {
        let parsed = parse_mark_args(["good", "--note", "prefix loss"]).unwrap();
        match parsed.command {
            Some(MarkCommand::Mark { label, note }) => {
                assert_eq!(label, Label::Good);
                assert_eq!(note.as_deref(), Some("prefix loss"));
            }
            _ => panic!("expected mark"),
        }
    }

    #[test]
    fn parse_help() {
        let parsed = parse_mark_args(["--help"]).unwrap();
        assert!(parsed.help);
    }
}
