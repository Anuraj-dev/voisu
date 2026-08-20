use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use voisu_core::{
    format_validated, organize_local_baseline, LocalBaselineOptions, RenderingPolicy,
    RenderingRoute, WritingMode,
};

use crate::completeness::{select_completeness_aware, CompletenessChoice};
use crate::manifest::{self, EvidencePresence, LoadedRecording};
use crate::metrics::{align_words, detect_critical_errors, detect_section_loss};
use crate::report::{
    aggregate, fingerprint_stable, ArmName, ArmResult, EvaluationReport, RecordingReport,
    StableReport, VolatileReport, SCHEMA,
};

const RECONSTRUCTION_REASON: &str =
    "Intent Reconstruction is not implemented (ticket 05 / #204)";

const DELIVERY_UNIMPLEMENTED: &str =
    "Delivery into a scratch editor is unimplemented in this private tool; evaluation writes report files only and does not type into the focused application";

/// Options for one evaluation run.
#[derive(Clone, Debug, Default)]
pub struct EvalConfig {
    pub deliver_scratch: Option<std::path::PathBuf>,
}

pub fn evaluate_path(
    manifest_path: &Path,
    config: &EvalConfig,
) -> Result<EvaluationReport, String> {
    let recordings = manifest::load_recordings(manifest_path)?;
    evaluate(&recordings, config)
}

pub fn evaluate(
    recordings: &[LoadedRecording],
    config: &EvalConfig,
) -> Result<EvaluationReport, String> {
    let mut reports = Vec::with_capacity(recordings.len());
    let mut latencies = BTreeMap::new();
    for recording in recordings {
        let (report, timing) = evaluate_recording(recording);
        latencies.insert(recording.correlation_id.clone(), timing);
        reports.push(report);
    }
    reports.sort_by(|a, b| a.correlation_id.cmp(&b.correlation_id));
    let mut delivery = BTreeMap::new();
    if config.deliver_scratch.is_some() {
        delivery.insert("requested".to_owned(), "true".to_owned());
        delivery.insert("status".to_owned(), "unimplemented".to_owned());
        delivery.insert("reason".to_owned(), DELIVERY_UNIMPLEMENTED.to_owned());
    } else {
        delivery.insert("requested".to_owned(), "false".to_owned());
        delivery.insert("status".to_owned(), "not_requested".to_owned());
    }
    let aggregates = aggregate(&reports);
    let stable = StableReport {
        aggregates,
        delivery,
        recording_count: reports.len(),
        recordings: reports,
        schema: SCHEMA.to_owned(),
    };
    let stable_fingerprint = fingerprint_stable(&stable)?;
    Ok(EvaluationReport {
        stable,
        stable_fingerprint,
        volatile: VolatileReport {
            elapsed_ms_by_correlation_id: latencies,
        },
    })
}

fn evaluate_recording(
    recording: &LoadedRecording,
) -> (RecordingReport, BTreeMap<String, Option<u64>>) {
    let mut evidence = BTreeMap::new();
    evidence.insert(
        "audio".to_owned(),
        match recording.audio {
            EvidencePresence::NotProvided => "not_provided".to_owned(),
            EvidencePresence::Present => "present".to_owned(),
            EvidencePresence::Missing => "missing".to_owned(),
        },
    );
    evidence.insert(
        "groq".to_owned(),
        if recording.groq.is_some() {
            "present".to_owned()
        } else {
            "missing".to_owned()
        },
    );
    evidence.insert(
        "deepgram".to_owned(),
        if recording.deepgram.is_some() {
            "present".to_owned()
        } else {
            "missing".to_owned()
        },
    );
    evidence.insert(
        "final_transcript".to_owned(),
        if recording.final_transcript.is_some() {
            "present".to_owned()
        } else {
            "missing".to_owned()
        },
    );

    let mut timing: BTreeMap<String, Option<u64>> = BTreeMap::new();
    let source_started = Instant::now();
    let choice =
        select_completeness_aware(recording.groq.as_deref(), recording.deepgram.as_deref());
    timing.insert(
        ArmName::CompletenessAwareSource.as_str().to_owned(),
        Some(elapsed_ms(source_started)),
    );

    let (selected_provider, selected_text, source_missing) = match &choice {
        CompletenessChoice::Selected { provider, text } => {
            evidence.insert("selected_source".to_owned(), provider.as_str().to_owned());
            (Some(provider.as_str().to_owned()), Some(text.clone()), None)
        }
        CompletenessChoice::Missing { reason } => {
            evidence.insert("selected_source".to_owned(), "missing".to_owned());
            (None, None, Some(reason.clone()))
        }
    };

    match &recording.reference {
        Some(_) => evidence.insert("reference".to_owned(), "present".to_owned()),
        None => evidence.insert(
            "reference".to_owned(),
            recording
                .reference_missing_reason
                .clone()
                .unwrap_or_else(|| "missing".to_owned()),
        ),
    };

    let mut arms = BTreeMap::new();
    arms.insert(
        ArmName::CompletenessAwareSource.as_str().to_owned(),
        score_or_missing(
            recording.reference.as_deref(),
            recording.reference_missing_reason.as_deref(),
            source_missing.as_deref(),
            selected_text.as_deref(),
            selected_provider.as_deref(),
            selected_text.as_deref().unwrap_or(""),
        ),
    );

    let (pipeline_arm, pipeline_ms) = match selected_text.as_deref() {
        Some(source) => {
            let started = Instant::now();
            let organized = run_guarded_pipeline(source, recording.rendering_policy);
            let ms = elapsed_ms(started);
            if organized.trim().is_empty() {
                (
                    ArmResult::Missing {
                        reason: "empty Transcript after local organize (Quality Failure)".to_owned(),
                    },
                    Some(ms),
                )
            } else {
                (
                    score_or_missing(
                        recording.reference.as_deref(),
                        recording.reference_missing_reason.as_deref(),
                        None,
                        Some(organized.as_str()),
                        selected_provider.as_deref(),
                        source,
                    ),
                    Some(ms),
                )
            }
        }
        None => (
            ArmResult::Missing {
                reason: source_missing
                    .clone()
                    .unwrap_or_else(|| "no Source Transcript to feed the organizer".to_owned()),
            },
            None,
        ),
    };
    arms.insert(ArmName::GuardedPipeline.as_str().to_owned(), pipeline_arm);
    timing.insert(ArmName::GuardedPipeline.as_str().to_owned(), pipeline_ms);

    arms.insert(
        ArmName::IntentReconstruction.as_str().to_owned(),
        ArmResult::Missing {
            reason: RECONSTRUCTION_REASON.to_owned(),
        },
    );
    timing.insert(ArmName::IntentReconstruction.as_str().to_owned(), None);

    let report = RecordingReport {
        arms,
        correlation_id: recording.correlation_id.clone(),
        evidence,
        speaker: recording.speaker.clone(),
        tags: recording.tags.clone(),
    };
    (report, timing)
}

fn score_or_missing(
    reference: Option<&str>,
    reference_missing_reason: Option<&str>,
    source_missing: Option<&str>,
    hypothesis: Option<&str>,
    selected_source: Option<&str>,
    source_fed: &str,
) -> ArmResult {
    if let Some(reason) = source_missing {
        return ArmResult::Missing {
            reason: reason.to_owned(),
        };
    }
    let Some(reference) = reference else {
        return ArmResult::Missing {
            reason: reference_missing_reason
                .unwrap_or("no audio-adjudicated reference")
                .to_owned(),
        };
    };
    let Some(hypothesis) = hypothesis else {
        return ArmResult::Missing {
            reason: "hypothesis text is missing".to_owned(),
        };
    };
    let word_error = align_words(reference, hypothesis);
    let critical_semantic_errors = detect_critical_errors(reference, hypothesis);
    let section_loss = detect_section_loss(reference, source_fed, hypothesis);
    ArmResult::Scored {
        critical_semantic_errors,
        hypothesis: hypothesis.to_owned(),
        section_loss,
        selected_source: selected_source.map(str::to_owned),
        word_error,
    }
}

fn run_guarded_pipeline(source: &str, policy: RenderingPolicy) -> String {
    let options = LocalBaselineOptions {
        policy,
        route: RenderingRoute::DeterministicLocal,
        timing: None,
    };
    let organized = organize_local_baseline(source, &options);
    format_validated(organized.rendered(), WritingMode::Smart)
        .rendered()
        .to_owned()
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
}
