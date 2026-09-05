//! Audio-adjudicated corpus scoring: per-case WER with the I/D/S breakdown,
//! corpus aggregates, the stable `voisu-private-score-corpus-v1` run JSON, and
//! the daemon replay path (host-only).
//!
//! The pipeline final Transcript comes either from the case's `result.json`
//! sidecar or — with `--replay`, never in CI — from `voisu replay`. A case
//! without either is SKIPped with its reason; nothing is ever faked.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use voisu_core::text_sha256_fingerprint;

use crate::completeness::select_completeness_aware;
use crate::corpus::{CorpusCase, FIXTURE_FILE};
use crate::metrics::{WordError, align_words, detect_critical_errors, detect_section_loss};
use crate::report::ensure_report_path_writable;

pub const RUN_SCHEMA: &str = "voisu-private-score-corpus-v1";

const REPLAY_NO_RESULT: &str = "no result.json; run capture-result or pass --replay";
const REPLAY_GAP: &str = "replay ran but produced no machine-readable transcript (daemon gap; see the README replay section)";
const REPLAY_NO_FIXTURE: &str = "no fixture.pcm in the case directory";
const REPLAY_BINARY_MISSING: &str = "voisu binary not found (pass --voisu or set VOISU_BIN)";
const REPLAY_DAEMON_UNAVAILABLE: &str = "daemon unavailable (start voisu-daemon and retry)";
const REPLAY_FAILED: &str = "replay failed (see stderr)";

/// Host-only replay invocation for cases without a result sidecar.
pub struct ReplayConfig {
    /// `voisu` binary: `--voisu <path>` wins over `$VOISU_BIN`.
    pub voisu_bin: String,
    /// The daemon diagnostics directory; fixtures stage into `<dir>/fixtures`.
    pub diagnostics_dir: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseStatus {
    Scored,
    NoFinal,
    Skipped,
}

/// Stop-anchored telemetry of the captured result, surfaced per case.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TelemetryRow {
    pub telemetry_schema: u32,
    pub recording_duration_ms: Option<u64>,
    pub stop_to_finalized_ms: Option<u64>,
    pub stop_to_delivered_ms: Option<u64>,
}

/// One per-case row of a scored run. Numbers only: transcript text, critical
/// error tokens, and delivery fallback reasons stay in the private sidecar so
/// a run JSON can be diffed or committed safely.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaseRow {
    pub id: String,
    pub tags: Vec<String>,
    pub notes: Option<String>,
    pub status: CaseStatus,
    /// No-final reason or skip reason; null for scored cases.
    pub reason: Option<String>,
    pub wer: Option<WordError>,
    /// Completeness-selected Source Transcript vs the reference (evaluator
    /// heuristic, not product behavior).
    pub source_wer: Option<WordError>,
    pub selected_source: Option<String>,
    /// `delivered` | `not_delivered` | `unknown`.
    pub delivery: String,
    pub delivery_method: Option<String>,
    pub critical_error_count: Option<usize>,
    pub section_loss: Option<bool>,
    pub telemetry: Option<TelemetryRow>,
}

/// Corpus-level one-number-per-change aggregates.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Aggregate {
    pub cases_total: usize,
    pub corpus_wer: Option<f64>,
    pub delivered: usize,
    pub delivery_denominator: usize,
    pub delivery_rate: Option<f64>,
    pub mean_case_wer: Option<f64>,
    pub median_stop_to_delivered_ms: Option<f64>,
    pub no_final: usize,
    pub scored: usize,
    pub skipped: usize,
    pub source_corpus_wer: Option<f64>,
    pub source_mean_case_wer: Option<f64>,
    pub total_deletions: usize,
    pub total_insertions: usize,
    pub total_reference_tokens: usize,
    pub total_substitutions: usize,
}

/// A scored corpus run. Field order is the stable JSON schema; see the README.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScoreRun {
    pub schema: String,
    pub corpus_dir: String,
    pub cases: Vec<CaseRow>,
    pub aggregate: Aggregate,
    pub run_fingerprint: String,
}

/// Scores every loaded case. `replay` enables the host-only daemon path for
/// cases without a result sidecar; every replay outcome is a documented SKIP.
pub fn score_corpus(
    cases: &[CorpusCase],
    corpus_dir: &Path,
    replay: Option<&ReplayConfig>,
) -> Result<ScoreRun, String> {
    let rows: Vec<CaseRow> = cases
        .iter()
        .map(|case| match (&case.result, replay) {
            (Some(result), _) => score_result(case, result),
            (None, Some(config)) => run_replay_case(case, config),
            (None, None) => CaseRow {
                id: case.id.clone(),
                tags: case.tags.clone(),
                notes: case.notes.clone(),
                status: CaseStatus::Skipped,
                reason: Some(REPLAY_NO_RESULT.to_owned()),
                wer: None,
                source_wer: None,
                selected_source: None,
                delivery: "unknown".to_owned(),
                delivery_method: None,
                critical_error_count: None,
                section_loss: None,
                telemetry: None,
            },
        })
        .collect();
    let aggregate = aggregate(&rows);
    let mut run = ScoreRun {
        schema: RUN_SCHEMA.to_owned(),
        corpus_dir: corpus_dir.display().to_string(),
        cases: rows,
        run_fingerprint: String::new(),
        aggregate,
    };
    run.run_fingerprint = fingerprint(&run)?;
    Ok(run)
}

fn score_result(case: &CorpusCase, result: &crate::corpus::CaseResult) -> CaseRow {
    let row = CaseRow {
        id: case.id.clone(),
        tags: case.tags.clone(),
        notes: case.notes.clone(),
        status: CaseStatus::Scored,
        reason: None,
        wer: None,
        source_wer: None,
        selected_source: None,
        delivery: delivery_outcome(result),
        delivery_method: result
            .delivery
            .as_ref()
            .and_then(|delivery| delivery.method.clone()),
        critical_error_count: None,
        section_loss: None,
        telemetry: result.telemetry.as_ref().map(|telemetry| TelemetryRow {
            telemetry_schema: telemetry.telemetry_schema,
            recording_duration_ms: telemetry.recording_duration_ms,
            stop_to_finalized_ms: telemetry.stop_to_finalized_ms,
            stop_to_delivered_ms: telemetry.stop_to_delivered_ms,
        }),
    };
    let Some(final_text) = result
        .final_transcript
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    else {
        let reason = result
            .error
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| "result carries no final transcript".to_owned());
        return CaseRow {
            status: CaseStatus::NoFinal,
            reason: Some(reason),
            ..row
        };
    };
    let wer = align_words(&case.reference, final_text);
    let critical_error_count = detect_critical_errors(&case.reference, final_text).len();
    let section_loss = detect_section_loss(&case.reference, final_text, final_text).any();
    let groq = source_text(result, "groq");
    let deepgram = source_text(result, "deepgram");
    let (source_wer, selected_source) = if groq.is_some() || deepgram.is_some() {
        match select_completeness_aware(groq.as_deref(), deepgram.as_deref()) {
            crate::completeness::CompletenessChoice::Selected { provider, text } => (
                Some(align_words(&case.reference, &text)),
                Some(provider.as_str().to_owned()),
            ),
            crate::completeness::CompletenessChoice::Missing { .. } => (None, None),
        }
    } else {
        (None, None)
    };
    CaseRow {
        wer: Some(wer),
        source_wer,
        selected_source,
        critical_error_count: Some(critical_error_count),
        section_loss: Some(section_loss),
        ..row
    }
}

fn source_text(result: &crate::corpus::CaseResult, provider: &str) -> Option<String> {
    result
        .source_transcripts
        .iter()
        .find(|source| source.provider == provider)
        .map(|source| source.text.clone())
        .filter(|text| !text.trim().is_empty())
}

fn delivery_outcome(result: &crate::corpus::CaseResult) -> String {
    match &result.delivery {
        Some(delivery) if delivery.delivered => "delivered".to_owned(),
        Some(_) => "not_delivered".to_owned(),
        None => "unknown".to_owned(),
    }
}

/// Runs the host-only daemon replay for one case and always SKIPs it: the
/// replay response carries no transcript text, so there is nothing to score
/// and nothing may be faked. The fixture stages as a plain file name inside
/// the daemon's private fixtures directory and is removed afterwards.
fn run_replay_case(case: &CorpusCase, config: &ReplayConfig) -> CaseRow {
    let skip = |reason: &str| CaseRow {
        id: case.id.clone(),
        tags: case.tags.clone(),
        notes: case.notes.clone(),
        status: CaseStatus::Skipped,
        reason: Some(reason.to_owned()),
        wer: None,
        source_wer: None,
        selected_source: None,
        delivery: "unknown".to_owned(),
        delivery_method: None,
        critical_error_count: None,
        section_loss: None,
        telemetry: None,
    };
    let Some(fixture) = &case.fixture else {
        return skip(REPLAY_NO_FIXTURE);
    };
    let staged = match stage_fixture(fixture, &config.diagnostics_dir, &case.id) {
        Ok(staged) => staged,
        Err(err) => return skip(&format!("cannot stage fixture: {err}")),
    };
    let output = Command::new(&config.voisu_bin)
        .arg("replay")
        .arg(&staged.1)
        .output();
    drop(staged); // removes the staged fixture copy
    match output {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => skip(REPLAY_BINARY_MISSING),
        Err(err) => skip(&format!("cannot run voisu: {err}")),
        Ok(output) => {
            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            if output.status.success() {
                eprint!("{combined}");
                skip(REPLAY_GAP)
            } else if output.status.code() == Some(3) || combined.contains("daemon unavailable") {
                eprint!("{combined}");
                skip(REPLAY_DAEMON_UNAVAILABLE)
            } else {
                eprint!("{combined}");
                skip(REPLAY_FAILED)
            }
        }
    }
}

/// Copies the case fixture into `<diagnostics>/fixtures/<case-id>.pcm` (the
/// only place the daemon reads replay fixtures from) with private modes. The
/// staged copy is removed when the guard drops.
struct StagedFixture(PathBuf, String);

impl Drop for StagedFixture {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn stage_fixture(
    fixture: &Path,
    diagnostics_dir: &Path,
    case_id: &str,
) -> Result<StagedFixture, String> {
    let bytes = fs::read(fixture).map_err(|err| format!("cannot read {FIXTURE_FILE}: {err}"))?;
    let fixture_dir = diagnostics_dir.join("fixtures");
    fs::create_dir_all(&fixture_dir)
        .map_err(|err| format!("cannot create {}: {err}", fixture_dir.display()))?;
    let metadata = fs::symlink_metadata(&fixture_dir)
        .map_err(|err| format!("cannot inspect {}: {err}", fixture_dir.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!("{} is not a directory", fixture_dir.display()));
    }
    fs::set_permissions(&fixture_dir, fs::Permissions::from_mode(0o700))
        .map_err(|err| format!("cannot set permissions on {}: {err}", fixture_dir.display()))?;
    let name = format!("{case_id}.pcm");
    let staged_path = fixture_dir.join(&name);
    if fs::symlink_metadata(&staged_path)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(format!(
            "refusing to replace symlink {}",
            staged_path.display()
        ));
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&staged_path)
        .map_err(|err| format!("cannot write {}: {err}", staged_path.display()))?;
    file.write_all(&bytes)
        .map_err(|err| format!("cannot write {}: {err}", staged_path.display()))?;
    Ok(StagedFixture(staged_path, name))
}

/// Corpus aggregates over the per-case rows. Corpus WER is weighted by
/// reference tokens (sum of edits over sum of reference tokens); the mean is
/// the unweighted average of per-case rates. Delivery counts every case whose
/// result carries a known outcome; the median covers cases with a recorded
/// `stop_to_delivered_ms`.
pub fn aggregate(rows: &[CaseRow]) -> Aggregate {
    let mut aggregate = Aggregate {
        cases_total: rows.len(),
        corpus_wer: None,
        delivered: 0,
        delivery_denominator: 0,
        delivery_rate: None,
        mean_case_wer: None,
        median_stop_to_delivered_ms: None,
        no_final: 0,
        scored: 0,
        skipped: 0,
        source_corpus_wer: None,
        source_mean_case_wer: None,
        total_deletions: 0,
        total_insertions: 0,
        total_reference_tokens: 0,
        total_substitutions: 0,
    };
    let mut error_ops = 0usize;
    let mut reference_tokens = 0usize;
    let mut source_error_ops = 0usize;
    let mut source_reference_tokens = 0usize;
    let mut source_scored = 0usize;
    let mut delivered_values = Vec::new();
    for row in rows {
        match row.status {
            CaseStatus::Scored => aggregate.scored += 1,
            CaseStatus::NoFinal => aggregate.no_final += 1,
            CaseStatus::Skipped => aggregate.skipped += 1,
        }
        if let Some(wer) = &row.wer {
            error_ops += wer.insertions + wer.deletions + wer.substitutions;
            reference_tokens += wer.reference_tokens;
        }
        if let Some(wer) = &row.source_wer {
            source_scored += 1;
            source_error_ops += wer.insertions + wer.deletions + wer.substitutions;
            source_reference_tokens += wer.reference_tokens;
        }
        match row.delivery.as_str() {
            "delivered" => {
                aggregate.delivered += 1;
                aggregate.delivery_denominator += 1;
            }
            "not_delivered" => aggregate.delivery_denominator += 1,
            _ => {}
        }
        if let Some(telemetry) = &row.telemetry
            && let Some(value) = telemetry.stop_to_delivered_ms
        {
            delivered_values.push(value);
        }
    }
    if aggregate.scored > 0 {
        aggregate.corpus_wer = Some(if reference_tokens == 0 {
            0.0
        } else {
            error_ops as f64 / reference_tokens as f64
        });
        let rates: Vec<f64> = rows
            .iter()
            .filter_map(|row| row.wer.as_ref().map(|wer| wer.error_rate))
            .collect();
        aggregate.mean_case_wer = Some(rates.iter().sum::<f64>() / rates.len() as f64);
    }
    if source_scored > 0 {
        aggregate.source_corpus_wer = Some(if source_reference_tokens == 0 {
            0.0
        } else {
            source_error_ops as f64 / source_reference_tokens as f64
        });
        let rates: Vec<f64> = rows
            .iter()
            .filter_map(|row| row.source_wer.as_ref().map(|wer| wer.error_rate))
            .collect();
        aggregate.source_mean_case_wer = Some(rates.iter().sum::<f64>() / rates.len() as f64);
    }
    if aggregate.delivery_denominator > 0 {
        aggregate.delivery_rate =
            Some(aggregate.delivered as f64 / aggregate.delivery_denominator as f64);
    }
    if !delivered_values.is_empty() {
        delivered_values.sort_unstable();
        let middle = delivered_values.len() / 2;
        aggregate.median_stop_to_delivered_ms = Some(if delivered_values.len() % 2 == 1 {
            delivered_values[middle] as f64
        } else {
            (delivered_values[middle - 1] + delivered_values[middle]) as f64 / 2.0
        });
    }
    aggregate
}

/// Hashes the run without its environment-specific `corpus_dir`, so identical
/// scoring outcomes fingerprint identically regardless of where the corpus
/// lives.
fn fingerprint(run: &ScoreRun) -> Result<String, String> {
    let mut value =
        serde_json::to_value(run).map_err(|err| format!("cannot serialize run: {err}"))?;
    if let Some(map) = value.as_object_mut() {
        map.remove("corpus_dir");
        map.remove("run_fingerprint");
    }
    let canonical =
        serde_json::to_string(&value).map_err(|err| format!("cannot canonicalize run: {err}"))?;
    Ok(text_sha256_fingerprint(&canonical))
}

/// Renders the per-case table plus the aggregate line.
pub fn render_human(run: &ScoreRun) -> String {
    let width = run
        .cases
        .iter()
        .map(|row| row.id.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let mut out = String::new();
    out.push_str(&format!(
        "{:<width$}  {:>8}  {:>4}  {:>4}  {:>4}  {:>13}  {:>8}  notes\n",
        "case",
        "WER",
        "I",
        "D",
        "S",
        "delivery",
        "src",
        width = width
    ));
    for row in &run.cases {
        let (wer, i, d, s) = match &row.wer {
            Some(wer) => (
                format!("{:.4}", wer.error_rate),
                wer.insertions.to_string(),
                wer.deletions.to_string(),
                wer.substitutions.to_string(),
            ),
            None => (
                "-".to_owned(),
                "-".to_owned(),
                "-".to_owned(),
                "-".to_owned(),
            ),
        };
        let delivery = match (&row.status, row.reason.as_deref()) {
            (CaseStatus::Skipped, Some(reason)) => format!("skip: {reason}"),
            (CaseStatus::NoFinal, Some(reason)) => format!("no_final: {reason}"),
            _ => row.delivery.clone(),
        };
        out.push_str(&format!(
            "{:<width$}  {:>8}  {:>4}  {:>4}  {:>4}  {:>13}  {:>8}  {}\n",
            row.id,
            wer,
            i,
            d,
            s,
            delivery,
            row.selected_source.as_deref().unwrap_or("-"),
            row.notes.as_deref().unwrap_or(""),
            width = width
        ));
    }
    out.push_str(&format!(
        "aggregate: cases={} scored={} no_final={} skipped={} corpus_wer={} mean_case_wer={} source_corpus_wer={} delivery={}/{} ({}) median_stop_to_delivered_ms={} fingerprint={}\n",
        run.aggregate.cases_total,
        run.aggregate.scored,
        run.aggregate.no_final,
        run.aggregate.skipped,
        format_option(run.aggregate.corpus_wer, 4),
        format_option(run.aggregate.mean_case_wer, 4),
        format_option(run.aggregate.source_corpus_wer, 4),
        run.aggregate.delivered,
        run.aggregate.delivery_denominator,
        format_option(run.aggregate.delivery_rate, 3),
        format_option(run.aggregate.median_stop_to_delivered_ms, 0),
        run.run_fingerprint,
    ));
    out
}

fn format_option(value: Option<f64>, places: usize) -> String {
    match value {
        Some(value) => format!("{value:.places$}"),
        None => "n/a".to_owned(),
    }
}

/// Writes the machine-readable run JSON after the same git-path guard as the
/// legacy `--out` report.
pub fn write_run(run: &ScoreRun, path: &Path) -> Result<(), String> {
    ensure_report_path_writable(path)?;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .map_err(|err| format!("cannot create run directory {}: {err}", parent.display()))?;
    }
    let json =
        serde_json::to_string_pretty(run).map_err(|err| format!("cannot serialize run: {err}"))?;
    fs::write(path, json).map_err(|err| format!("cannot write run {}: {err}", path.display()))
}

/// Loads a previously written run JSON for comparison; the schema field must
/// match `RUN_SCHEMA` ("voisu-private-score-corpus-v1").
pub fn load_run(path: &Path) -> Result<ScoreRun, String> {
    let text = fs::read_to_string(path)
        .map_err(|err| format!("cannot read run {}: {err}", path.display()))?;
    let run: ScoreRun =
        serde_json::from_str(&text).map_err(|err| format!("run {}: {err}", path.display()))?;
    if run.schema != RUN_SCHEMA {
        return Err(format!(
            "run {}: schema {:?} is not {RUN_SCHEMA:?}",
            path.display(),
            run.schema
        ));
    }
    Ok(run)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wer(rate: f64, i: usize, d: usize, s: usize, tokens: usize) -> WordError {
        WordError {
            deletions: d,
            error_rate: rate,
            insertions: i,
            reference_tokens: tokens,
            substitutions: s,
        }
    }

    fn row(id: &str, status: CaseStatus, delivery: &str, wer: Option<WordError>) -> CaseRow {
        CaseRow {
            id: id.to_owned(),
            tags: Vec::new(),
            notes: None,
            status,
            reason: None,
            wer,
            source_wer: None,
            selected_source: None,
            delivery: delivery.to_owned(),
            delivery_method: None,
            critical_error_count: None,
            section_loss: None,
            telemetry: None,
        }
    }

    #[test]
    fn aggregate_weights_corpus_wer_and_averages_case_rates() {
        let rows = vec![
            row(
                "a",
                CaseStatus::Scored,
                "delivered",
                Some(wer(0.0, 0, 0, 0, 4)),
            ),
            row(
                "b",
                CaseStatus::Scored,
                "delivered",
                Some(wer(0.5, 0, 1, 1, 4)),
            ),
            row(
                "c",
                CaseStatus::Scored,
                "not_delivered",
                Some(wer(1.0, 1, 0, 0, 2)),
            ),
            row("d", CaseStatus::Skipped, "unknown", None),
        ];
        let aggregate = aggregate(&rows);
        assert_eq!(aggregate.cases_total, 4);
        assert_eq!(aggregate.scored, 3);
        assert_eq!(aggregate.skipped, 1);
        let corpus = aggregate.corpus_wer.expect("corpus wer");
        assert!(
            (corpus - 0.3).abs() < 1e-9,
            "3 edits / 10 tokens, got {corpus}"
        );
        let mean = aggregate.mean_case_wer.expect("mean wer");
        assert!((mean - 0.5).abs() < 1e-9, "mean of 0, 0.5, 1.0, got {mean}");
        assert_eq!(aggregate.delivered, 2);
        assert_eq!(aggregate.delivery_denominator, 3);
        assert!((aggregate.delivery_rate.unwrap() - 2.0 / 3.0).abs() < 1e-9);
        assert!(aggregate.median_stop_to_delivered_ms.is_none());
    }

    #[test]
    fn aggregate_median_is_the_true_middle() {
        let mut odd = row("a", CaseStatus::Scored, "delivered", None);
        odd.telemetry = Some(TelemetryRow {
            telemetry_schema: 2,
            recording_duration_ms: None,
            stop_to_finalized_ms: None,
            stop_to_delivered_ms: Some(100),
        });
        let mut even = odd.clone();
        even.telemetry = Some(TelemetryRow {
            telemetry_schema: 2,
            recording_duration_ms: None,
            stop_to_finalized_ms: None,
            stop_to_delivered_ms: Some(301),
        });
        assert_eq!(
            aggregate(&[odd.clone()]).median_stop_to_delivered_ms,
            Some(100.0)
        );
        assert_eq!(
            aggregate(&[odd, even]).median_stop_to_delivered_ms,
            Some(200.5)
        );
    }

    #[test]
    fn aggregate_with_no_scored_cases_is_all_none() {
        let aggregate = aggregate(&[row("a", CaseStatus::Skipped, "unknown", None)]);
        assert_eq!(aggregate.corpus_wer, None);
        assert_eq!(aggregate.mean_case_wer, None);
        assert_eq!(aggregate.delivery_rate, None);
        assert_eq!(aggregate.scored, 0);
    }

    #[test]
    fn fingerprint_ignores_the_corpus_location() {
        let mut run = ScoreRun {
            schema: RUN_SCHEMA.to_owned(),
            corpus_dir: "/tmp/one".to_owned(),
            cases: Vec::new(),
            aggregate: aggregate(&[]),
            run_fingerprint: String::new(),
        };
        run.run_fingerprint = fingerprint(&run).unwrap();
        let moved = ScoreRun {
            corpus_dir: "/elsewhere".to_owned(),
            ..run.clone()
        };
        assert_eq!(run.run_fingerprint, moved.run_fingerprint);
    }
}
