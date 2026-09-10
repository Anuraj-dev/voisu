//! L2 harness integration: fake capture / supervisor / Delivery seams.
//! Local ASR stays unavailable in the product daemon.

use std::time::Duration;

use voisu_app::local_worker::{
    BakeoffCase, CaseKind, CriticalKind, DurationBand, Eligibility, FakeWorker, FeasibilityRunner,
    GateVerdict, RuntimeFamily, Split, WorkerState, WorkerSupervisor, evaluate_report,
    evaluation_order, locked_host_profiles, packaged_unit_restrictions,
    refuse_production_weight_download,
};
use voisu_app::local_worker::{Correlation, TranscribeRequest};

fn correlation() -> Correlation {
    Correlation {
        daemon_nonce: "bakeoff".into(),
        generation: 1,
        request_id: "prep".into(),
        recording_id: "prep".into(),
        model_receipt_hash: "harness-no-weights".into(),
    }
}

#[test]
fn whisper_cpp_is_first_and_ci_never_downloads_weights() {
    let unit = include_str!("../../../packaging/voisu.service");
    let candidates = evaluation_order(&packaged_unit_restrictions(unit));
    assert_eq!(candidates[0].family, RuntimeFamily::WhisperCppProcess);
    assert!(matches!(
        candidates[0].eligibility,
        Eligibility::EvaluateFirst { .. }
    ));
    assert!(refuse_production_weight_download().is_err());
    let catalog = voisu_app::local_model::shipped_catalog();
    assert!(voisu_app::local_model::production_selection(&catalog).is_none());
}

#[test]
fn locked_english_bakeoff_stays_pending_without_a_winner() {
    let speech = BakeoffCase {
        id: "speech-hello".into(),
        kind: CaseKind::Speech,
        split: Split::HeldOut,
        band: DurationBand::OneToTen,
        audio_hash: "hash-speech-hello".into(),
        reference: "hello raja".into(),
        pcm: vec![1, 0, 2, 0],
        speech: Duration::from_millis(800),
        stratum: "pilot-quiet".into(),
        critical: vec![(CriticalKind::Name, "raja".into())],
        scripted_hypothesis: Some("hello raja".into()),
    };
    let negative = BakeoffCase {
        id: "neg-silence".into(),
        kind: CaseKind::Negative,
        split: Split::HeldOut,
        band: DurationBand::OneToTen,
        audio_hash: "hash-neg-silence".into(),
        reference: String::new(),
        pcm: vec![0, 0, 0, 0],
        speech: Duration::from_millis(200),
        stratum: "silence".into(),
        critical: Vec::new(),
        scripted_hypothesis: None,
    };

    let mut speech_runner = FeasibilityRunner::harness(FakeWorker::default());
    speech_runner.prepare(correlation()).unwrap();
    let speech_outcome = speech_runner.run_case(&speech).unwrap();
    assert!(speech_outcome.delivered);
    assert!(speech_outcome.critical_failures.is_empty());
    let timings = speech_outcome.timings.as_ref().unwrap();
    assert!(
        timings
            .stop_to_delivered_ms
            .is_some_and(|ms| ms < timings.recording_duration_ms)
    );

    let mut negative_runner = FeasibilityRunner::harness(FakeWorker::default());
    negative_runner.prepare(correlation()).unwrap();
    let negative_outcome = negative_runner.run_case(&negative).unwrap();
    assert!(!negative_outcome.delivered);

    let report = evaluate_report(
        RuntimeFamily::WhisperCppProcess,
        &[speech, negative],
        vec![speech_outcome, negative_outcome],
        vec![
            "harness smoke with scripted FakeWorker".into(),
            "locked 100+20 English corpus is not in CI".into(),
        ],
        None,
        false,
    );
    assert_eq!(report.verdict, GateVerdict::PendingEvidence);
    assert!(!report.winner_selected);
    assert!(!report.corpus_lock_satisfied);
    assert!(!report.production_weights_downloaded);
    assert_eq!(
        locked_host_profiles()[0].id,
        "fedora-kde-wayland",
        "Fedora KDE Wayland remains the first supported product target"
    );
}

#[test]
fn local_supervisor_starts_absent_and_does_not_admit_capture() {
    let supervisor = WorkerSupervisor::<FakeWorker>::absent();
    assert_eq!(supervisor.state(), WorkerState::Absent);
    let mut supervisor = supervisor;
    let error = supervisor.transcribe(TranscribeRequest {
        correlation: correlation(),
        pcm: vec![1, 0],
    });
    assert!(error.is_err());
}
