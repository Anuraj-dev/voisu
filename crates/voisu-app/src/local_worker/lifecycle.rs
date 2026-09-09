//! Bounded Local worker lifecycle on the L2 supervisor/protocol/sandbox seams.
//!
//! Production Start/Replay stay gated. A crash cannot move inference into the
//! daemon. Late and duplicate results never become a stale Delivery.

use std::time::Instant;

use voisu_core::Transcript;

use super::protocol::{Correlation, WorkerFrame};
use super::seams::{DeliverySeam, DeliveryToken, FakeDelivery, SeamError};
use super::supervisor::{
    FakeWorker, ReapOutcome, SupervisorError, TranscribeRequest, WorkerChild, WorkerOutcome,
    WorkerState, WorkerSupervisor,
};

#[derive(Clone, Debug)]
struct LiveWork {
    generation: u64,
    request_id: String,
    recording_id: String,
    token: Option<DeliveryToken>,
}

pub struct LocalLifecycle<C, D> {
    supervisor: WorkerSupervisor<C>,
    delivery: D,
    live: Option<LiveWork>,
    daemon_nonce: String,
    generation: u64,
    model_receipt_hash: String,
}

impl LocalLifecycle<FakeWorker, FakeDelivery> {
    #[must_use]
    pub fn harness() -> Self {
        Self::new(FakeDelivery::default())
    }
}

impl<C: WorkerChild, D: DeliverySeam> LocalLifecycle<C, D> {
    #[must_use]
    pub fn new(delivery: D) -> Self {
        Self {
            supervisor: WorkerSupervisor::absent(),
            delivery,
            live: None,
            daemon_nonce: "l3".into(),
            generation: 0,
            model_receipt_hash: String::new(),
        }
    }

    #[must_use]
    pub fn state(&self) -> WorkerState {
        self.supervisor.state()
    }

    #[must_use]
    pub fn delivered(&self) -> Option<&D> {
        Some(&self.delivery)
    }

    pub fn delivery_mut(&mut self) -> &mut D {
        &mut self.delivery
    }

    pub fn pin_model_receipt_hash(&mut self, hash: impl Into<String>) {
        self.model_receipt_hash = hash.into();
    }

    /// Attach a worker that still owes its load proof. This is also the
    /// explicit Setup retry after restart exhaustion, and a daemon restart is
    /// a fresh supervisor: the worker never restarts on its own. Reports
    /// `Loading` until `Prepare` proves first-inference health; transcription
    /// before that is `NotReady`.
    pub fn attach(&mut self, child: C, now: Instant) -> Result<(), SupervisorError> {
        self.supervisor.attach_pending(child, now)?;
        self.generation = self.generation.saturating_add(1);
        Ok(())
    }

    pub fn prepare(&mut self, recording_id: &str, now: Instant) -> Result<(), SupervisorError> {
        let correlation = self.correlation(recording_id, "prepare");
        self.supervisor.prepare(correlation, now)?;
        Ok(())
    }

    pub fn transcribe(
        &mut self,
        recording_id: &str,
        request_id: &str,
        pcm: Vec<u8>,
    ) -> Result<WorkerOutcome, SupervisorError> {
        if self.supervisor.state() != WorkerState::Ready {
            return Err(SupervisorError::NotReady(self.supervisor.state()));
        }
        let correlation = Correlation {
            daemon_nonce: self.daemon_nonce.clone(),
            generation: self.generation,
            request_id: request_id.to_owned(),
            recording_id: recording_id.to_owned(),
            model_receipt_hash: self.model_receipt_hash.clone(),
        };
        self.live = Some(LiveWork {
            generation: self.generation,
            request_id: request_id.to_owned(),
            recording_id: recording_id.to_owned(),
            token: None,
        });
        match self
            .supervisor
            .transcribe(TranscribeRequest { correlation, pcm })
        {
            Ok(outcome) => {
                if let WorkerOutcome::Transcript { text, .. } = &outcome
                    && let Err(error) = self.commit_delivery(recording_id, text)
                {
                    self.invalidate();
                    return Err(SupervisorError::Unavailable(match error {
                        SeamError::DuplicateDelivery | SeamError::StaleToken => {
                            "stale Delivery rejected"
                        }
                        SeamError::Unauthorized => "Delivery unauthorized",
                        SeamError::CloudCapabilityUsed => "Cloud capability used",
                        SeamError::Capture(_) => "capture seam",
                    }));
                }
                Ok(outcome)
            }
            Err(error) => {
                // Retire the generation before control returns: the supervisor
                // already reaped the owned child synchronously, and from here
                // no late frame can correlate.
                self.invalidate();
                Err(error)
            }
        }
    }

    pub fn cancel(&mut self) -> Result<ReapOutcome, SupervisorError> {
        self.invalidate();
        self.supervisor.cancel()
    }

    /// Cloud selection drains and unloads the idle worker: retire the
    /// generation first so no late frame can match afterwards, then detach
    /// the owned child without transcribing. Never drops busy work and never
    /// falls back silently — after this call Local work needs an explicit
    /// Setup retry, and inference reports only the health-observed device.
    pub fn drain_for_cloud_selection(&mut self) -> Result<ReapOutcome, SupervisorError> {
        self.invalidate();
        self.supervisor.unload_idle()
    }

    /// True while the rolling start budget is spent: only this explicit
    /// retry path (after the window slides) or a daemon restart starts again.
    pub fn restart_budget_exhausted(&mut self, now: Instant) -> bool {
        self.supervisor.restart_exhausted(now)
    }

    /// Late worker output after generation invalidation is ignored.
    pub fn inject_late_frame(
        &mut self,
        frame: WorkerFrame,
        recording_id: &str,
    ) -> Result<(), SupervisorError> {
        let Some(live) = &self.live else {
            return Ok(());
        };
        let correlation = match &frame {
            WorkerFrame::Transcript { correlation, .. }
            | WorkerFrame::NoText { correlation, .. }
            | WorkerFrame::Ready { correlation, .. } => correlation,
            WorkerFrame::Error { .. } => return Ok(()),
        };
        if correlation.generation != live.generation
            || correlation.request_id != live.request_id
            || correlation.recording_id != recording_id
        {
            return Ok(());
        }
        Err(SupervisorError::Protocol(
            super::protocol::FrameError::Unsolicited,
        ))
    }

    fn commit_delivery(&mut self, recording_id: &str, text: &str) -> Result<(), SeamError> {
        let live = self.live.as_mut().ok_or(SeamError::Unauthorized)?;
        if live.recording_id != recording_id {
            return Err(SeamError::StaleToken);
        }
        let token = self.delivery.authorize(recording_id)?;
        live.token = Some(token.clone());
        self.delivery.deliver(token, &Transcript(text.to_owned()))?;
        self.live = None;
        Ok(())
    }

    fn invalidate(&mut self) {
        self.live = None;
        self.generation = self.generation.saturating_add(1);
    }

    fn correlation(&self, recording_id: &str, request_id: &str) -> Correlation {
        Correlation {
            daemon_nonce: self.daemon_nonce.clone(),
            generation: self.generation.max(1),
            request_id: request_id.to_owned(),
            recording_id: recording_id.to_owned(),
            model_receipt_hash: self.model_receipt_hash.clone(),
        }
    }
}

/// Production Start and Replay remain refused before capture until measurement
/// selects a catalog entry. A Pilot Candidate alone cannot open this gate.
#[must_use]
pub fn production_local_admission() -> bool {
    crate::local_model::production_selection(&crate::local_model::shipped_catalog()).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_worker::MAX_RESTARTS;
    use std::time::Duration;

    fn fixture_receipt_hash() -> String {
        crate::local_model::from_entry(crate::local_model::ci_fixture_entry(), "l3-artifact")
            .receipt_hash
    }

    fn ready(text: &str) -> LocalLifecycle<FakeWorker, FakeDelivery> {
        let mut life = LocalLifecycle::harness();
        let hash = fixture_receipt_hash();
        life.pin_model_receipt_hash(hash.clone());
        let worker = FakeWorker {
            scripted_text: Some(text.into()),
            generation: 1,
            model_receipt_hash: hash,
            ..FakeWorker::default()
        };
        life.attach(worker, Instant::now()).unwrap();
        if let Some(child) = life.supervisor.child_mut() {
            child.generation = life.generation;
        }
        life.prepare("rec-1", Instant::now()).unwrap();
        life
    }

    #[test]
    fn production_admission_ignores_the_unmeasured_pilot_candidate() {
        assert!(!production_local_admission());
    }

    #[test]
    fn load_abort_does_not_admit_capture_or_delivery() {
        let mut life = LocalLifecycle::harness();
        let hash = fixture_receipt_hash();
        life.pin_model_receipt_hash(hash.clone());
        let worker = FakeWorker {
            scripted_error: Some("load_abort".into()),
            generation: 1,
            model_receipt_hash: hash,
            ..FakeWorker::default()
        };
        life.attach(worker, Instant::now()).unwrap();
        if let Some(child) = life.supervisor.child_mut() {
            child.generation = life.generation;
        }
        let error = life.prepare("rec-1", Instant::now()).unwrap_err();
        assert!(matches!(error, SupervisorError::LoadAborted));
        assert_eq!(life.state(), WorkerState::Unavailable);
        let transcribe = life.transcribe("rec-1", "q", vec![1, 0]).unwrap_err();
        assert!(matches!(transcribe, SupervisorError::NotReady(_)));
        assert!(life.delivery_mut().delivered.is_empty());
    }

    #[test]
    fn crash_is_an_error_not_a_daemon_panic() {
        let mut life = ready("hello");
        if let Some(child) = life.supervisor.child_mut() {
            child.crash = true;
        }
        let error = life.transcribe("rec-1", "q", vec![1, 0]).unwrap_err();
        assert!(matches!(error, SupervisorError::Crashed));
        assert_ne!(life.state(), WorkerState::Busy);
        assert!(life.delivery_mut().delivered.is_empty());
    }

    #[test]
    fn oom_is_an_error_not_success() {
        let mut life = ready("hello");
        if let Some(child) = life.supervisor.child_mut() {
            child.scripted_error = Some("oom".into());
        }
        let error = life.transcribe("rec-1", "q", vec![1, 0]).unwrap_err();
        assert!(matches!(error, SupervisorError::OutOfMemory));
        assert!(life.delivery_mut().delivered.is_empty());
    }

    #[test]
    fn stale_and_duplicate_ids_are_rejected() {
        let mut life = ready("hello");
        life.transcribe("rec-1", "q1", vec![1, 0]).unwrap();
        let dup = life.transcribe("rec-1", "q1", vec![1, 0]).unwrap_err();
        assert!(matches!(dup, SupervisorError::Protocol(_)));
        if let Some(child) = life.supervisor.child_mut() {
            child.generation = 99;
        }
        let stale = life.transcribe("rec-1", "q2", vec![1, 0]).unwrap_err();
        assert!(matches!(stale, SupervisorError::Protocol(_)));
        assert_eq!(life.delivery_mut().delivered.len(), 1);
    }

    #[test]
    fn ignored_cancel_invalidates_generation_before_late_result() {
        let mut life = LocalLifecycle::harness();
        let hash = fixture_receipt_hash();
        life.pin_model_receipt_hash(hash.clone());
        let worker = FakeWorker {
            scripted_text: Some("late".into()),
            generation: 1,
            model_receipt_hash: hash.clone(),
            ignore_cancel: true,
            hold_until_cancel: true,
            ..FakeWorker::default()
        };
        life.attach(worker, Instant::now()).unwrap();
        if let Some(child) = life.supervisor.child_mut() {
            child.generation = life.generation;
        }
        life.prepare("rec-1", Instant::now()).unwrap();
        let inflight = life.transcribe("rec-1", "q", vec![1, 0]).unwrap_err();
        assert!(matches!(inflight, SupervisorError::Busy));
        assert!(life.delivery_mut().delivered.is_empty());
        let outcome = life.cancel().unwrap();
        assert_eq!(outcome, ReapOutcome::Unreaped);
        let late_text = life
            .supervisor
            .child_mut()
            .and_then(|child| child.queued_late.clone())
            .expect("in-flight FakeWorker queued a late Transcript");
        let late = WorkerFrame::Transcript {
            correlation: Correlation {
                daemon_nonce: "l3".into(),
                generation: 1,
                request_id: "q".into(),
                recording_id: "rec-1".into(),
                model_receipt_hash: hash,
            },
            text: late_text,
        };
        assert!(life.inject_late_frame(late, "rec-1").is_ok());
        assert!(life.delivery_mut().delivered.is_empty());
    }

    #[test]
    fn late_result_after_success_does_not_duplicate_delivery() {
        let mut life = ready("hello");
        life.transcribe("rec-1", "q", vec![1, 0]).unwrap();
        let late = WorkerFrame::Transcript {
            correlation: Correlation {
                daemon_nonce: "l3".into(),
                generation: 1,
                request_id: "q".into(),
                recording_id: "rec-1".into(),
                model_receipt_hash: fixture_receipt_hash(),
            },
            text: "late".into(),
        };
        assert!(life.inject_late_frame(late, "rec-1").is_ok());
        assert_eq!(life.delivery_mut().delivered, vec!["hello".to_owned()]);
    }

    #[test]
    fn restart_exhaustion_stays_unavailable() {
        let mut life = LocalLifecycle::harness();
        let now = Instant::now();
        for _ in 0..MAX_RESTARTS {
            life.attach(FakeWorker::default(), now).unwrap();
            let _ = life.cancel();
        }
        let error = life
            .attach(FakeWorker::default(), now + Duration::from_secs(1))
            .unwrap_err();
        assert!(matches!(error, SupervisorError::RestartExhausted));
        assert_ne!(life.state(), WorkerState::Ready);
    }

    #[test]
    fn successful_transcript_is_single_delivery() {
        let mut life = ready("hello raja");
        let outcome = life.transcribe("rec-1", "q", vec![1, 0]).unwrap();
        assert!(matches!(outcome, WorkerOutcome::Transcript { .. }));
        assert_eq!(life.delivery_mut().delivered, vec!["hello raja".to_owned()]);
        let token_again = life.delivery_mut().authorize("rec-1");
        assert_eq!(token_again, Err(SeamError::DuplicateDelivery));
    }

    #[test]
    fn attach_is_loading_until_prepare_proves_health() {
        let mut life = LocalLifecycle::harness();
        life.pin_model_receipt_hash(fixture_receipt_hash());
        life.attach(FakeWorker::default(), Instant::now()).unwrap();
        assert_eq!(life.state(), WorkerState::Loading);
        let error = life.transcribe("rec-1", "q", vec![1, 0]).unwrap_err();
        assert!(matches!(
            error,
            SupervisorError::NotReady(WorkerState::Loading)
        ));
        assert!(life.delivery_mut().delivered.is_empty());
    }

    #[test]
    fn cloud_selection_drains_idle_worker_without_transcribing() {
        let mut life = ready("hello");
        let outcome = life.drain_for_cloud_selection().unwrap();
        assert_eq!(outcome, ReapOutcome::Exited);
        assert_eq!(life.state(), WorkerState::Absent);
        assert!(life.delivery_mut().delivered.is_empty());
        // Local work afterwards needs an explicit Setup retry: attach lands
        // Loading, never straight back in Ready.
        life.attach(FakeWorker::default(), Instant::now()).unwrap();
        assert_eq!(life.state(), WorkerState::Loading);
    }

    #[test]
    fn drain_blocks_on_an_unreaped_child() {
        let mut life = LocalLifecycle::harness();
        let hash = fixture_receipt_hash();
        life.pin_model_receipt_hash(hash.clone());
        let worker = FakeWorker {
            scripted_text: Some("late".into()),
            generation: 1,
            model_receipt_hash: hash,
            ignore_cancel: true,
            hold_until_cancel: true,
            ..FakeWorker::default()
        };
        life.attach(worker, Instant::now()).unwrap();
        if let Some(child) = life.supervisor.child_mut() {
            child.generation = life.generation;
        }
        life.prepare("rec-1", Instant::now()).unwrap();
        let inflight = life.transcribe("rec-1", "q", vec![1, 0]).unwrap_err();
        assert!(matches!(inflight, SupervisorError::Busy));
        let outcome = life.cancel().unwrap();
        assert_eq!(outcome, ReapOutcome::Unreaped);
        assert!(matches!(
            life.drain_for_cloud_selection().unwrap_err(),
            SupervisorError::Unavailable(_)
        ));
        assert!(life.delivery_mut().delivered.is_empty());
    }

    #[test]
    fn exhausted_budget_needs_an_explicit_setup_retry() {
        let mut life = LocalLifecycle::harness();
        let now = Instant::now();
        for _ in 0..MAX_RESTARTS {
            life.attach(FakeWorker::default(), now).unwrap();
            let _ = life.cancel();
        }
        assert!(life.restart_budget_exhausted(now + Duration::from_secs(1)));
        let error = life
            .attach(FakeWorker::default(), now + Duration::from_secs(1))
            .unwrap_err();
        assert!(matches!(error, SupervisorError::RestartExhausted));
        // The window sliding permits a later explicit retry — the Setup
        // retry call — not a background restart.
        let later = now + crate::local_worker::RESTART_WINDOW + Duration::from_secs(1);
        assert!(!life.restart_budget_exhausted(later));
        life.attach(FakeWorker::default(), later).unwrap();
        assert_eq!(life.state(), WorkerState::Loading);
    }

    #[test]
    fn crash_retires_the_generation_before_any_late_frame() {
        let mut life = ready("hello");
        if let Some(child) = life.supervisor.child_mut() {
            child.crash = true;
        }
        let error = life.transcribe("rec-1", "q", vec![1, 0]).unwrap_err();
        assert!(matches!(error, SupervisorError::Crashed));
        let late = WorkerFrame::Transcript {
            correlation: Correlation {
                daemon_nonce: "l3".into(),
                generation: 1,
                request_id: "q".into(),
                recording_id: "rec-1".into(),
                model_receipt_hash: fixture_receipt_hash(),
            },
            text: "late".into(),
        };
        assert!(life.inject_late_frame(late, "rec-1").is_ok());
        assert!(life.delivery_mut().delivered.is_empty());
        let stale = life.transcribe("rec-1", "q2", vec![1, 0]).unwrap_err();
        assert!(matches!(stale, SupervisorError::NotReady(_)));
    }

    #[test]
    fn error_frame_after_success_is_ignored() {
        let mut life = ready("hello");
        life.transcribe("rec-1", "q", vec![1, 0]).unwrap();
        assert!(
            life.inject_late_frame(
                WorkerFrame::Error {
                    code: "native".into(),
                    metadata: serde_json::Map::new(),
                },
                "rec-1"
            )
            .is_ok()
        );
        assert_eq!(life.delivery_mut().delivered, vec!["hello".to_owned()]);
    }
}
