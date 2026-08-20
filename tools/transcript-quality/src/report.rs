use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

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
    pub critical_error_recordings: usize,
    pub mean_word_error: Option<f64>,
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

pub fn write_report(report: &EvaluationReport, path: &Path) -> Result<(), String> {
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
        let mean = match agg.mean_word_error {
            Some(rate) => format!("{rate:.4}"),
            None => "n/a".to_owned(),
        };
        out.push_str(&format!(
            "  {name}: scored={} missing={} mean_word_error={} section_loss={} critical_error_recordings={}\n",
            agg.scored, agg.missing, mean, agg.section_loss_recordings, agg.critical_error_recordings
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
        let mut error_sum = 0.0;
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
                    error_sum += word_error.error_rate;
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
        let mean_word_error = if scored == 0 {
            None
        } else {
            Some(error_sum / scored as f64)
        };
        out.insert(
            name.to_owned(),
            ArmAggregate {
                critical_error_recordings,
                mean_word_error,
                missing,
                scored,
                section_loss_recordings,
            },
        );
    }
    out
}
