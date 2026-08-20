use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use serde::Serialize;
use voisu_core::text_sha256_fingerprint;

use crate::metrics::{CriticalError, SectionLoss, WordError};

pub const SCHEMA: &str = "voisu-private-transcript-quality-v1";

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArmName {
    CompletenessAwareSource,
    GuardedPipeline,
    IntentReconstruction,
}

impl ArmName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CompletenessAwareSource => "completeness_aware_source",
            Self::GuardedPipeline => "guarded_pipeline",
            Self::IntentReconstruction => "intent_reconstruction",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ArmResult {
    Missing {
        reason: String,
    },
    Scored {
        critical_semantic_errors: Vec<CriticalError>,
        hypothesis: String,
        section_loss: SectionLoss,
        selected_source: Option<String>,
        word_error: WordError,
    },
}

impl ArmResult {
    pub fn is_missing(&self) -> bool {
        matches!(self, Self::Missing { .. })
    }

    pub fn scored_error_rate(&self) -> Option<f64> {
        match self {
            Self::Scored { word_error, .. } => Some(word_error.error_rate),
            Self::Missing { .. } => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RecordingReport {
    pub arms: BTreeMap<String, ArmResult>,
    pub correlation_id: String,
    pub evidence: BTreeMap<String, String>,
    pub speaker: Option<String>,
    pub tags: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ArmAggregate {
    pub corpus_word_error: Option<f64>,
    pub critical_error_recordings: usize,
    pub missing: usize,
    pub scored: usize,
    pub section_loss_recordings: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StableReport {
    pub aggregates: BTreeMap<String, ArmAggregate>,
    pub delivery: BTreeMap<String, String>,
    pub recording_count: usize,
    pub recordings: Vec<RecordingReport>,
    pub schema: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct VolatileReport {
    pub elapsed_ms_by_correlation_id: BTreeMap<String, BTreeMap<String, Option<u64>>>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EvaluationReport {
    pub stable: StableReport,
    pub stable_fingerprint: String,
    pub volatile: VolatileReport,
}

pub fn fingerprint_stable(stable: &StableReport) -> Result<String, String> {
    let value = serde_json::to_value(stable)
        .map_err(|err| format!("cannot serialize stable report: {err}"))?;
    let canonical = serde_json::to_string(&value)
        .map_err(|err| format!("cannot canonicalize stable report: {err}"))?;
    Ok(text_sha256_fingerprint(&canonical))
}

pub fn default_report_path(manifest_path: &Path) -> PathBuf {
    let beside = manifest_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("transcript-quality-report.json");
    if git_root_of(&normalize_path(&beside)).is_none() {
        beside
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("out/transcript-quality-report.json")
    }
}

pub fn ensure_report_path_writable(path: &Path) -> Result<(), String> {
    let abs = normalize_path(path);
    let Some(root) = git_root_of(&abs) else {
        return Ok(());
    };
    if path_is_gitignored(&root, &abs) {
        return Ok(());
    }
    Err(format!(
        "refusing to write report {} under git work tree {}; use --out under an ignored path (tools/transcript-quality/out/) or outside the repository",
        abs.display(),
        root.display()
    ))
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
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

fn git_root_of(path: &Path) -> Option<PathBuf> {
    let mut dir = path.parent().unwrap_or(path).to_path_buf();
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn path_is_gitignored(root: &Path, path: &Path) -> bool {
    match Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["check-ignore", "-q", "--"])
        .arg(path)
        .status()
    {
        Ok(status) if status.success() => true,
        Ok(status) if status.code() == Some(1) => false,
        _ => ignored_by_known_patterns(root, path),
    }
}

fn ignored_by_known_patterns(root: &Path, path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name == "transcript-quality-report.json" {
        return true;
    }
    path.starts_with(root.join("tools/transcript-quality/out"))
}

pub fn write_report(report: &EvaluationReport, path: &Path) -> Result<(), String> {
    ensure_report_path_writable(path)?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|err| {
                format!("cannot create report directory {}: {err}", parent.display())
            })?;
        }
    }
    let json = serde_json::to_string_pretty(report)
        .map_err(|err| format!("cannot serialize report: {err}"))?;
    fs::write(path, json).map_err(|err| format!("cannot write report {}: {err}", path.display()))
}

pub fn render_human(report: &EvaluationReport) -> String {
    let mut out = String::new();
    for recording in &report.stable.recordings {
        out.push_str(&format!("Recording {}\n", recording.correlation_id));
        if let Some(speaker) = &recording.speaker {
            out.push_str(&format!("  speaker: {speaker}\n"));
        }
        if !recording.tags.is_empty() {
            out.push_str(&format!("  tags: {}\n", recording.tags.join(", ")));
        }
        for (key, value) in &recording.evidence {
            out.push_str(&format!("  evidence.{key}: {value}\n"));
        }
        for (name, arm) in &recording.arms {
            out.push_str(&format!("  {name}: {}\n", format_arm(arm)));
        }
        out.push('\n');
    }
    out.push_str("Aggregates\n");
    out.push_str(&format!(
        "  recordings: {}\n",
        report.stable.recording_count
    ));
    for (name, agg) in &report.stable.aggregates {
        let corpus = match agg.corpus_word_error {
            Some(rate) => format!("{rate:.4}"),
            None => "n/a".to_owned(),
        };
        out.push_str(&format!(
            "  {name}: scored={} missing={} corpus_word_error={} section_loss={} critical_error_recordings={}\n",
            agg.scored, agg.missing, corpus, agg.section_loss_recordings, agg.critical_error_recordings
        ));
    }
    if let Some(status) = report.stable.delivery.get("status") {
        out.push_str(&format!("  delivery: {status}\n"));
    }
    out.push_str(&format!(
        "  stable_fingerprint: {}\n",
        report.stable_fingerprint
    ));
    out
}

fn format_arm(arm: &ArmResult) -> String {
    match arm {
        ArmResult::Missing { reason } => format!("missing ({reason})"),
        ArmResult::Scored {
            word_error,
            critical_semantic_errors,
            section_loss,
            selected_source,
            ..
        } => {
            let source = selected_source
                .as_deref()
                .unwrap_or("-");
            format!(
                "scored wer={:.4} I={} D={} S={} critical={} section_loss={} selected={source}",
                word_error.error_rate,
                word_error.insertions,
                word_error.deletions,
                word_error.substitutions,
                critical_semantic_errors.len(),
                section_loss.any()
            )
        }
    }
}

pub fn aggregate(recordings: &[RecordingReport]) -> BTreeMap<String, ArmAggregate> {
    let names = [
        ArmName::CompletenessAwareSource.as_str(),
        ArmName::GuardedPipeline.as_str(),
        ArmName::IntentReconstruction.as_str(),
    ];
    let mut out = BTreeMap::new();
    for name in names {
        let mut scored = 0usize;
        let mut missing = 0usize;
        let mut error_ops = 0usize;
        let mut reference_tokens = 0usize;
        let mut section_loss_recordings = 0usize;
        let mut critical_error_recordings = 0usize;
        for recording in recordings {
            match recording.arms.get(name) {
                Some(ArmResult::Scored {
                    word_error,
                    section_loss,
                    critical_semantic_errors,
                    ..
                }) => {
                    scored += 1;
                    error_ops += word_error.insertions
                        + word_error.deletions
                        + word_error.substitutions;
                    reference_tokens += word_error.reference_tokens;
                    if section_loss.any() {
                        section_loss_recordings += 1;
                    }
                    if !critical_semantic_errors.is_empty() {
                        critical_error_recordings += 1;
                    }
                }
                Some(ArmResult::Missing { .. }) | None => missing += 1,
            }
        }
        let corpus_word_error = if scored == 0 {
            None
        } else if reference_tokens == 0 {
            Some(0.0)
        } else {
            Some(error_ops as f64 / reference_tokens as f64)
        };
        out.insert(
            name.to_owned(),
            ArmAggregate {
                corpus_word_error,
                critical_error_recordings,
                missing,
                scored,
                section_loss_recordings,
            },
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::WordError;

    fn scored(word_error: WordError) -> ArmResult {
        ArmResult::Scored {
            critical_semantic_errors: Vec::new(),
            hypothesis: "h".to_owned(),
            section_loss: SectionLoss {
                body: false,
                prefix: false,
                relative_to: Vec::new(),
            },
            selected_source: None,
            word_error,
        }
    }

    fn recording(id: &str, completeness: ArmResult) -> RecordingReport {
        let mut arms = BTreeMap::new();
        arms.insert(
            ArmName::CompletenessAwareSource.as_str().to_owned(),
            completeness,
        );
        arms.insert(
            ArmName::GuardedPipeline.as_str().to_owned(),
            ArmResult::Missing {
                reason: "unused".to_owned(),
            },
        );
        arms.insert(
            ArmName::IntentReconstruction.as_str().to_owned(),
            ArmResult::Missing {
                reason: "unused".to_owned(),
            },
        );
        RecordingReport {
            arms,
            correlation_id: id.to_owned(),
            evidence: BTreeMap::new(),
            speaker: None,
            tags: Vec::new(),
        }
    }

    #[test]
    fn corpus_word_error_is_weighted_by_reference_tokens() {
        let short = recording(
            "short",
            scored(WordError {
                deletions: 2,
                error_rate: 1.0,
                insertions: 0,
                reference_tokens: 2,
                substitutions: 0,
            }),
        );
        let long = recording(
            "long",
            scored(WordError {
                deletions: 0,
                error_rate: 0.0,
                insertions: 0,
                reference_tokens: 98,
                substitutions: 0,
            }),
        );
        let aggs = aggregate(&[short, long]);
        let completeness = aggs
            .get(ArmName::CompletenessAwareSource.as_str())
            .expect("completeness aggregate");
        let rate = completeness.corpus_word_error.expect("corpus rate");
        assert!(
            (rate - 0.02).abs() < 1e-9,
            "short recording must not outweigh the long one: {rate}"
        );
        assert_eq!(completeness.scored, 2);
    }

    #[test]
    fn default_report_path_uses_crate_out_inside_git_tree() {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/synthetic.json");
        let out = default_report_path(&manifest);
        assert_eq!(
            out,
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("out/transcript-quality-report.json")
        );
        ensure_report_path_writable(&out).expect("crate out/ is gitignored");
    }

    #[test]
    fn default_report_path_sits_beside_manifest_outside_git() {
        let dir = std::env::temp_dir().join(format!(
            "voisu-tq-out-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let manifest = dir.join("manifest.json");
        let out = default_report_path(&manifest);
        assert_eq!(out, dir.join("transcript-quality-report.json"));
        ensure_report_path_writable(&out).expect("outside git is writable");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn tracked_git_path_is_refused() {
        let tracked = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let err = ensure_report_path_writable(&tracked).expect_err("must refuse tracked path");
        assert!(
            err.contains("refusing to write report"),
            "unexpected refuse message: {err}"
        );
    }
}
