use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use transcript_quality::{evaluate_path, ArmResult, EvalConfig, EvaluationReport};

fn fixtures_manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/synthetic.json")
}

fn scratch_dir(label: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "voisu-tq-{}-{}-{label}",
        std::process::id(),
        n
    ));
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn evaluate_synthetic() -> EvaluationReport {
    evaluate_path(&fixtures_manifest(), &EvalConfig::default()).expect("evaluate synthetic")
}

fn recording<'a>(
    report: &'a EvaluationReport,
    id: &str,
) -> &'a transcript_quality::RecordingReport {
    report
        .stable
        .recordings
        .iter()
        .find(|row| row.correlation_id == id)
        .unwrap_or_else(|| panic!("missing Recording {id}"))
}

fn arm<'a>(report: &'a EvaluationReport, id: &str, name: &str) -> &'a ArmResult {
    recording(report, id)
        .arms
        .get(name)
        .unwrap_or_else(|| panic!("{id} missing arm {name}"))
}

#[test]
fn identical_inputs_produce_identical_stable_reports() {
    let first = evaluate_synthetic();
    let second = evaluate_synthetic();
    assert_eq!(first.stable_fingerprint, second.stable_fingerprint);
    let a = serde_json::to_value(&first.stable).unwrap();
    let b = serde_json::to_value(&second.stable).unwrap();
    assert_eq!(a, b);

    let dir = scratch_dir("ident");
    let path_a = dir.join("a.json");
    let path_b = dir.join("b.json");
    transcript_quality::run([
        "--manifest",
        fixtures_manifest().to_str().unwrap(),
        "--out",
        path_a.to_str().unwrap(),
    ])
    .unwrap();
    transcript_quality::run([
        "--manifest",
        fixtures_manifest().to_str().unwrap(),
        "--out",
        path_b.to_str().unwrap(),
    ])
    .unwrap();
    let file_a: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path_a).unwrap()).unwrap();
    let file_b: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path_b).unwrap()).unwrap();
    assert_eq!(file_a["stable"], file_b["stable"]);
    assert_eq!(file_a["stable_fingerprint"], file_b["stable_fingerprint"]);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn missing_reconstruction_is_missing_not_zero_error() {
    let report = evaluate_synthetic();
    for row in &report.stable.recordings {
        let arm = row
            .arms
            .get("intent_reconstruction")
            .expect("intent_reconstruction arm");
        match arm {
            ArmResult::Missing { reason } => {
                assert!(
                    reason.contains("ticket 05"),
                    "{}: unexpected reason {reason}",
                    row.correlation_id
                );
            }
            ArmResult::Scored { word_error, .. } => {
                panic!(
                    "{}: reconstruction must not be scored (wer={})",
                    row.correlation_id, word_error.error_rate
                );
            }
        }
        assert!(arm.scored_error_rate().is_none());
    }
    let agg = report
        .stable
        .aggregates
        .get("intent_reconstruction")
        .expect("reconstruction aggregate");
    assert_eq!(agg.scored, 0);
    assert!(agg.mean_word_error.is_none());
}

#[test]
fn missing_reference_and_missing_source_are_missing() {
    let report = evaluate_synthetic();

    for name in ["completeness_aware_source", "guarded_pipeline"] {
        match arm(&report, "rec-missing-reference", name) {
            ArmResult::Missing { reason } => {
                assert!(
                    reason.contains("reference") || reason.contains("adjudicated"),
                    "{name}: {reason}"
                );
            }
            ArmResult::Scored { word_error, .. } => {
                panic!(
                    "{name} scored missing-reference as wer={}",
                    word_error.error_rate
                )
            }
        }
        match arm(&report, "rec-script-reference", name) {
            ArmResult::Missing { reason } => {
                assert!(
                    reason.contains("script"),
                    "script reference must be missing evidence: {reason}"
                );
            }
            ArmResult::Scored { .. } => panic!("{name} scored a reading script as spoken truth"),
        }
        match arm(&report, "rec-missing-source", name) {
            ArmResult::Missing { reason } => {
                assert!(
                    reason.contains("Source Transcript") || reason.contains("missing"),
                    "{name}: {reason}"
                );
            }
            ArmResult::Scored { word_error, .. } => {
                panic!(
                    "{name} scored missing source as wer={}",
                    word_error.error_rate
                )
            }
        }
    }
}

#[test]
fn completeness_prefers_materially_longer_non_repetitive_sibling() {
    let report = evaluate_synthetic();
    let row = recording(&report, "rec-complete");
    assert_eq!(
        row.evidence.get("selected_source").map(String::as_str),
        Some("groq")
    );
    match arm(&report, "rec-complete", "completeness_aware_source") {
        ArmResult::Scored {
            selected_source,
            hypothesis,
            word_error,
            ..
        } => {
            assert_eq!(selected_source.as_deref(), Some("groq"));
            assert!(
                hypothesis.contains("eval ticket"),
                "short Deepgram fragment must not win: {hypothesis}"
            );
            assert_eq!(word_error.error_rate, 0.0);
        }
        ArmResult::Missing { reason } => panic!("completeness arm missing: {reason}"),
    }
}

#[test]
fn section_loss_when_organized_text_drops_source_prefix() {
    let report = evaluate_synthetic();
    match arm(&report, "rec-section-loss", "guarded_pipeline") {
        ArmResult::Scored {
            section_loss,
            hypothesis,
            ..
        } => {
            assert!(
                section_loss.prefix,
                "organizer dropped the source prefix but section_loss.prefix is false; hypothesis={hypothesis:?} loss={section_loss:?}"
            );
            assert!(
                section_loss.relative_to.iter().any(|item| item == "source"),
                "section loss must cite the source that fed the organizer: {:?}",
                section_loss.relative_to
            );
            assert!(
                !hypothesis
                    .to_ascii_lowercase()
                    .contains("please remember this"),
                "fixture expected the organizer to drop the prefix; hypothesis={hypothesis}"
            );
        }
        ArmResult::Missing { reason } => panic!("pipeline arm missing: {reason}"),
    }
}
