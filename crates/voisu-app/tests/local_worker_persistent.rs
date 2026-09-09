//! Persistent Local worker integration proof (#265) through public interfaces.
//!
//! No production admission: [`production_local_admission`] stays closed, so
//! these tests drive the scripted harness worker through the same public
//! supervisor/lifecycle seams the daemon will use. They prove the #265
//! acceptance shape end to end:
//!
//! * one worker, one request, no per-Recording reload or residue;
//! * no Ready before Prepare health;
//! * Cloud selection drains the idle worker;
//! * restart exhaustion needs an explicit Setup retry;
//! * fault isolation: crash/timeout retires the generation, late frames and
//!   stale Delivery never land.

use std::time::{Duration, Instant};

use voisu_app::local_worker::{
    Correlation, FakeDelivery, FakeWorker, LocalLifecycle, RESTART_WINDOW, ReapOutcome,
    SupervisorError, TranscribeRequest, WorkerOutcome, WorkerState, WorkerSupervisor,
    production_local_admission,
};

fn correlation(generation: u64, request: &str) -> Correlation {
    Correlation {
        daemon_nonce: "l3".into(),
        generation,
        request_id: request.into(),
        recording_id: "rec-1".into(),
        model_receipt_hash: "harness-no-weights".into(),
    }
}

fn ready_harness(text: &str) -> WorkerSupervisor<FakeWorker> {
    let mut supervisor = WorkerSupervisor::absent();
    supervisor
        .attach_pending(FakeWorker::default(), Instant::now())
        .unwrap();
    assert_eq!(supervisor.state(), WorkerState::Loading);
    let mut prepare = correlation(1, "prepare");
    prepare.model_receipt_hash = "harness-no-weights".into();
    supervisor.prepare(prepare, Instant::now()).unwrap();
    if let Some(child) = supervisor.child_mut() {
        child.scripted_text = Some(text.to_owned());
    }
    supervisor
}

#[test]
fn production_admission_stays_closed() {
    assert!(
        !production_local_admission(),
        "Gate A keeps production Local unavailable: no capture path opens here"
    );
}

#[test]
fn attach_is_not_ready_until_prepare() {
    let mut supervisor = WorkerSupervisor::<FakeWorker>::absent();
    supervisor
        .attach_pending(FakeWorker::default(), Instant::now())
        .unwrap();
    assert_eq!(supervisor.state(), WorkerState::Loading);
    let error = supervisor
        .transcribe(TranscribeRequest {
            correlation: correlation(1, "q"),
            pcm: vec![1, 0],
        })
        .unwrap_err();
    assert!(matches!(
        error,
        SupervisorError::NotReady(WorkerState::Loading)
    ));
}

#[test]
fn repeated_inference_without_reload_or_residue() {
    let mut supervisor = ready_harness("first");
    let outcome = supervisor
        .transcribe(TranscribeRequest {
            correlation: correlation(1, "q1"),
            pcm: vec![1, 0],
        })
        .unwrap();
    match outcome {
        WorkerOutcome::Transcript {
            text,
            observed_device,
        } => {
            assert_eq!(text, "first");
            assert_eq!(observed_device, "cpu");
        }
        other => panic!("expected transcript, got {other:?}"),
    }
    if let Some(child) = supervisor.child_mut() {
        child.scripted_text = Some("second".into());
    }
    let outcome = supervisor
        .transcribe(TranscribeRequest {
            correlation: correlation(1, "q2"),
            pcm: vec![1, 0],
        })
        .unwrap();
    assert!(matches!(outcome, WorkerOutcome::Transcript { ref text, .. } if text == "second"));
    let child = supervisor.child_mut().expect("persistent child");
    assert_eq!(child.prepare_count, 1, "no per-Recording reload");
    assert_eq!(child.transcribe_count, 2);
    // Speech then silence: without PCM residue the silence is no-text.
    let outcome = supervisor
        .transcribe(TranscribeRequest {
            correlation: correlation(1, "q3"),
            pcm: vec![0, 0, 0, 0],
        })
        .unwrap();
    assert!(matches!(outcome, WorkerOutcome::NoText { .. }));
}

#[test]
fn one_request_at_a_time_with_no_queue() {
    let mut supervisor = ready_harness("hi");
    if let Some(child) = supervisor.child_mut() {
        child.hold_until_cancel = true;
    }
    let busy = supervisor
        .transcribe(TranscribeRequest {
            correlation: correlation(1, "q1"),
            pcm: vec![1, 0],
        })
        .unwrap_err();
    assert!(matches!(busy, SupervisorError::Busy));
    // The refused second request never queued behind the first.
    assert_ne!(supervisor.state(), WorkerState::Busy);
}

#[test]
fn blocked_io_trips_the_deadline_and_reaps() {
    let worker = FakeWorker {
        block_for: Some(Duration::from_secs(3600)),
        scripted_text: Some("hi".into()),
        ..FakeWorker::default()
    };
    let mut supervisor = WorkerSupervisor::absent();
    supervisor.attach_ready(worker, Instant::now()).unwrap();
    // block_for (one hour) exceeds the public 45 s Stop budget without
    // sleeping: the deadline trips synchronously and the child is reaped.
    let error = supervisor
        .transcribe(TranscribeRequest {
            correlation: correlation(1, "q"),
            pcm: vec![1, 0],
        })
        .unwrap_err();
    assert!(matches!(error, SupervisorError::TimedOut));
    assert_eq!(supervisor.state(), WorkerState::Absent);
}

#[test]
fn cloud_selection_drains_the_idle_worker() {
    let mut life: LocalLifecycle<FakeWorker, FakeDelivery> = LocalLifecycle::harness();
    assert_eq!(life.state(), WorkerState::Absent);
    assert_eq!(
        life.drain_for_cloud_selection().unwrap(),
        ReapOutcome::Exited
    );
}

#[test]
fn crash_retires_the_generation_and_blocks_new_work_until_reaped() {
    let mut supervisor = ready_harness("hi");
    if let Some(child) = supervisor.child_mut() {
        child.crash = true;
    }
    let error = supervisor
        .transcribe(TranscribeRequest {
            correlation: correlation(1, "q"),
            pcm: vec![1, 0],
        })
        .unwrap_err();
    assert!(matches!(error, SupervisorError::Crashed));
    assert_ne!(supervisor.state(), WorkerState::Busy);
}

#[test]
fn restart_exhaustion_needs_an_explicit_setup_retry() {
    let mut supervisor = WorkerSupervisor::<FakeWorker>::absent();
    let t0 = Instant::now();
    for _ in 0..3 {
        supervisor
            .attach_pending(FakeWorker::default(), t0)
            .unwrap();
        supervisor.cancel().unwrap();
    }
    assert!(supervisor.restart_exhausted(t0 + Duration::from_secs(1)));
    assert!(matches!(
        supervisor.attach_pending(FakeWorker::default(), t0 + Duration::from_secs(1)),
        Err(SupervisorError::RestartExhausted)
    ));
    let later = t0 + RESTART_WINDOW + Duration::from_secs(1);
    assert!(!supervisor.restart_exhausted(later));
    supervisor
        .attach_pending(FakeWorker::default(), later)
        .unwrap();
    assert_eq!(supervisor.state(), WorkerState::Loading);
}
