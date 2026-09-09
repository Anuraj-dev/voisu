//! Local vs Cloud routing: choose the path before constructing dependencies.

mod silent;

use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use voisu_core::{
    AsrMode, DaemonState, DeliveryOutcome, LifecycleEvidence, LifecycleStage, LocalReadiness,
    Response, Transcript, stop_anchored_timings,
};

use crate::config::WritingMode;

use crate::local_model::{ActiveReceipt, ModelStore, models_dir};
use crate::local_recovery::{RecoveryClock, RecoveryError, RecoveryStore};
use crate::local_tail::{
    DeliveryCoordinator, DeliveryPermit, TailError, TerminalDecision, decide_replay,
    decide_transcript,
};
use crate::local_worker::{
    CloudCapabilitySentinel, Correlation, FakeWorker, WorkerOutcome, WorkerState, WorkerSupervisor,
    is_silence_pcm, transcribe_through_supervisor,
};

pub use silent::{CloudFreeProvider, cloud_free_slots};

const LOCAL_UNAVAILABLE: &str = "Local selected; model unavailable";
const LOCAL_NOT_READY: &str = "Local ASR is unavailable; Start refused before capture";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LocalAdmission {
    Ready {
        receipt: ActiveReceipt,
        model_identity: String,
    },
    Unavailable {
        error: String,
    },
}

struct Gate {
    readiness: LocalReadiness,
    receipt: Option<ActiveReceipt>,
    supervisor: WorkerSupervisor<FakeWorker>,
    coordinator: DeliveryCoordinator,
    sentinel: CloudCapabilitySentinel,
    pending_permit: Option<DeliveryPermit>,
    user_terms: Vec<String>,
    /// Supervisor is checked out for inference; refresh must not map the
    /// absent stand-in to Verifying/Loading.
    inference_checked_out: bool,
}

impl Gate {
    fn new() -> Self {
        Self {
            readiness: LocalReadiness::Absent,
            receipt: None,
            supervisor: WorkerSupervisor::absent(),
            coordinator: DeliveryCoordinator::new(),
            sentinel: CloudCapabilitySentinel::new(),
            pending_permit: None,
            user_terms: Vec::new(),
            inference_checked_out: false,
        }
    }

    fn refresh(&mut self) -> LocalReadiness {
        if self.inference_checked_out {
            return self.readiness.clone();
        }
        if test_local_ready() {
            self.ensure_test_worker();
        } else if self.supervisor.state() == WorkerState::Ready && self.receipt.is_some() {
            self.readiness = LocalReadiness::Ready {
                model_identity: self
                    .receipt
                    .as_ref()
                    .map(|receipt| receipt.catalog_id.clone()),
            };
        } else {
            self.load_receipt();
            if self.receipt.is_none() {
                self.readiness = LocalReadiness::Unavailable {
                    error: LOCAL_UNAVAILABLE.to_owned(),
                };
                return self.readiness.clone();
            }
            self.readiness = match self.supervisor.state() {
                WorkerState::Absent | WorkerState::Verifying => LocalReadiness::Verifying,
                WorkerState::Loading => LocalReadiness::Loading,
                WorkerState::Ready => LocalReadiness::Ready {
                    model_identity: self
                        .receipt
                        .as_ref()
                        .map(|receipt| receipt.catalog_id.clone()),
                },
                WorkerState::Busy => LocalReadiness::Busy,
                WorkerState::Stopping => LocalReadiness::Stopping,
                WorkerState::Unavailable => LocalReadiness::Unavailable {
                    error: LOCAL_NOT_READY.to_owned(),
                },
            };
        }
        self.readiness.clone()
    }

    fn load_receipt(&mut self) {
        self.receipt = (|| {
            let dir = models_dir().ok()?;
            let store = ModelStore::open(dir).ok()?;
            store.load_active().ok().flatten()
        })();
    }

    fn ensure_test_worker(&mut self) {
        self.load_receipt();
        if self.receipt.is_none() {
            let receipt = crate::local_model::from_entry(
                crate::local_model::ci_fixture_entry(),
                "l4-test-artifact",
            );
            self.receipt = Some(receipt);
        }
        let hash = self
            .receipt
            .as_ref()
            .map(|receipt| receipt.receipt_hash.clone())
            .unwrap_or_default();
        if self.supervisor.state() != WorkerState::Ready {
            self.attach_ready_worker(hash, &test_local_text());
        }
        self.readiness = LocalReadiness::Ready {
            model_identity: self
                .receipt
                .as_ref()
                .map(|receipt| receipt.catalog_id.clone()),
        };
    }

    fn attach_ready_worker(&mut self, hash: String, text: &str) {
        self.supervisor = WorkerSupervisor::absent();
        let worker = FakeWorker {
            scripted_text: Some(text.to_owned()),
            generation: 1,
            model_receipt_hash: hash.clone(),
            ..FakeWorker::default()
        };
        if self
            .supervisor
            .attach_ready(worker, Instant::now())
            .is_err()
        {
            self.readiness = LocalReadiness::Unavailable {
                error: LOCAL_NOT_READY.to_owned(),
            };
            return;
        }
        let correlation = correlation(&hash, "l4-prepare", "prepare");
        if self
            .supervisor
            .prepare(correlation, Instant::now())
            .is_err()
        {
            self.readiness = LocalReadiness::Unavailable {
                error: LOCAL_NOT_READY.to_owned(),
            };
        }
    }
}

struct InferenceSession {
    receipt_hash: String,
    sentinel: CloudCapabilitySentinel,
    supervisor: WorkerSupervisor<FakeWorker>,
    coordinator: DeliveryCoordinator,
}

fn take_inference_session() -> Result<InferenceSession, LocalError> {
    let mut gate = gate().lock().map_err(|_| LocalError::Unavailable)?;
    if !gate.sentinel.local_path_clean() {
        return Err(LocalError::CloudCapability);
    }
    let receipt = gate.receipt.clone().ok_or(LocalError::Unavailable)?;
    if gate.supervisor.state() != WorkerState::Ready {
        return Err(LocalError::Unavailable);
    }
    let supervisor = std::mem::replace(&mut gate.supervisor, WorkerSupervisor::absent());
    let coordinator = std::mem::replace(&mut gate.coordinator, DeliveryCoordinator::new());
    gate.inference_checked_out = true;
    gate.readiness = LocalReadiness::Busy;
    Ok(InferenceSession {
        receipt_hash: receipt.receipt_hash,
        sentinel: gate.sentinel.clone(),
        supervisor,
        coordinator,
    })
}

fn restore_inference_session(session: InferenceSession) {
    if let Ok(mut gate) = gate().lock() {
        gate.supervisor = session.supervisor;
        gate.coordinator = session.coordinator;
        gate.inference_checked_out = false;
        if gate.supervisor.state() == WorkerState::Ready {
            gate.readiness = LocalReadiness::Ready {
                model_identity: gate
                    .receipt
                    .as_ref()
                    .map(|receipt| receipt.catalog_id.clone()),
            };
        }
    }
}

fn infer_unlocked(
    session: &mut InferenceSession,
    recording_id: &str,
    request_id: &str,
    pcm: Vec<u8>,
) -> Result<WorkerOutcome, LocalError> {
    let correlation = correlation(&session.receipt_hash, recording_id, request_id);
    transcribe_through_supervisor(&mut session.supervisor, correlation, pcm, &session.sentinel)
        .map_err(|error| LocalError::Inference(format!("{error:?}")))
}

fn assert_gate_unlocked() {
    #[cfg(test)]
    {
        assert!(
            gate().try_lock().is_ok(),
            "gate lock must not be held across persist/inference/tail"
        );
    }
}

fn core_writing_mode(mode: WritingMode) -> voisu_core::WritingMode {
    match mode {
        WritingMode::Smart => voisu_core::WritingMode::Smart,
        WritingMode::Literal => voisu_core::WritingMode::Literal,
    }
}

fn correlation(hash: &str, recording_id: &str, request_id: &str) -> Correlation {
    Correlation {
        daemon_nonce: "l4".into(),
        generation: 1,
        request_id: request_id.to_owned(),
        recording_id: recording_id.to_owned(),
        model_receipt_hash: hash.to_owned(),
    }
}

fn gate() -> &'static Mutex<Gate> {
    static GATE: OnceLock<Mutex<Gate>> = OnceLock::new();
    GATE.get_or_init(|| Mutex::new(Gate::new()))
}

#[must_use]
pub fn production_readiness() -> LocalReadiness {
    gate()
        .lock()
        .map(|mut gate| gate.refresh())
        .unwrap_or(LocalReadiness::Unavailable {
            error: LOCAL_UNAVAILABLE.to_owned(),
        })
}

#[must_use]
pub fn admit_local(kind_ready_required: bool) -> LocalAdmission {
    let Ok(mut gate) = gate().lock() else {
        return LocalAdmission::Unavailable {
            error: LOCAL_UNAVAILABLE.to_owned(),
        };
    };
    let readiness = gate.refresh();
    if kind_ready_required && !readiness.admits_capture() {
        return LocalAdmission::Unavailable {
            error: match &readiness {
                LocalReadiness::Unavailable { error } => error.clone(),
                _ => LOCAL_NOT_READY.to_owned(),
            },
        };
    }
    match (gate.receipt.clone(), readiness) {
        (Some(receipt), LocalReadiness::Ready { model_identity }) => LocalAdmission::Ready {
            model_identity: model_identity.unwrap_or_else(|| receipt.catalog_id.clone()),
            receipt,
        },
        _ => LocalAdmission::Unavailable {
            error: LOCAL_NOT_READY.to_owned(),
        },
    }
}

pub fn begin_live(recording_id: &str) {
    begin_live_with_terms(recording_id, Vec::new());
}

pub fn begin_live_with_terms(recording_id: &str, user_terms: Vec<String>) {
    if let Ok(mut gate) = gate().lock() {
        gate.coordinator.begin_live(recording_id);
        gate.user_terms = user_terms;
        gate.pending_permit = None;
        gate.sentinel = CloudCapabilitySentinel::new();
    }
}

pub fn begin_local_session() {
    if let Ok(mut gate) = gate().lock() {
        gate.sentinel = CloudCapabilitySentinel::new();
    }
}

pub fn note_cloud_capability_used() {
    if let Ok(mut gate) = gate().lock() {
        gate.sentinel.construct_cloud_client();
    }
}

pub fn abandon_live(recording_id: &str) {
    if let Ok(mut gate) = gate().lock() {
        gate.coordinator.abandon_live(recording_id);
    }
}

pub fn skip_cloud_doctor_probes() -> bool {
    let Ok(state) = voisu_core::state_dir() else {
        return false;
    };
    matches!(
        crate::asr_mode::load_mode(&crate::config::config_path(), &state),
        Ok((AsrMode::Local, _))
    )
}

/// Finish a Local Recording: optional recovery persist, inference, tail, Delivery.
pub fn complete_recording(
    recording_id: &str,
    pcm: Vec<u8>,
    user_terms: &[String],
    writing_mode: WritingMode,
    recovery_enabled: bool,
    cloud_selected: bool,
) -> Result<LocalCompletion, LocalError> {
    if cloud_selected {
        return Err(LocalError::CloudSelected);
    }
    let mut session = take_inference_session()?;
    assert_gate_unlocked();
    if recovery_enabled {
        let persist = RecoveryStore::open_default(RecoveryClock::system()).and_then(|store| {
            store.persist_before_inference(recording_id, &pcm, &session.receipt_hash)
        });
        if let Err(error) = persist {
            restore_inference_session(session);
            return Err(LocalError::Recovery(error));
        }
    }
    let request_id = format!("{recording_id}-asr");
    let outcome = match infer_unlocked(&mut session, recording_id, &request_id, pcm.clone()) {
        Ok(outcome) => outcome,
        Err(_) if is_silence_pcm(&pcm) => WorkerOutcome::NoText {
            reason: "silence".to_owned(),
            observed_device: "cpu".to_owned(),
        },
        Err(error) => {
            restore_inference_session(session);
            if recovery_enabled {
                let _ = RecoveryStore::open_default(RecoveryClock::system())
                    .and_then(|store| store.mark_failed(recording_id));
            }
            return Err(error);
        }
    };
    let decision = decide_transcript(
        recording_id,
        &pcm,
        outcome,
        user_terms,
        core_writing_mode(writing_mode),
        &mut session.coordinator,
        &session.sentinel,
    );
    restore_inference_session(session);
    Ok(LocalCompletion {
        decision: decision.map_err(LocalError::Tail)?,
        recovery_enabled,
        recording_id: recording_id.to_owned(),
    })
}

pub fn complete_replay(
    recording_id: &str,
    pcm: Vec<u8>,
    user_terms: &[String],
    writing_mode: WritingMode,
    cloud_selected: bool,
    from_recovery: bool,
) -> Result<TerminalDecision, LocalError> {
    if cloud_selected && from_recovery {
        return Err(LocalError::CloudSelected);
    }
    let mut session = take_inference_session()?;
    assert_gate_unlocked();
    let request_id = format!("{recording_id}-replay");
    let outcome = match infer_unlocked(&mut session, recording_id, &request_id, pcm.clone()) {
        Ok(outcome) => outcome,
        Err(error) => {
            restore_inference_session(session);
            return Err(error);
        }
    };
    let decision = decide_replay(
        recording_id,
        &pcm,
        outcome,
        user_terms,
        core_writing_mode(writing_mode),
        &mut session.coordinator,
        &session.sentinel,
    );
    restore_inference_session(session);
    decision.map_err(LocalError::Tail)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LocalStopResult {
    Transcript(String),
    NoText { reason: String },
}

pub fn finish_local_stop(
    recording_id: &str,
    pcm: Vec<u8>,
    writing_mode: WritingMode,
    recovery_enabled: bool,
) -> Result<LocalStopResult, LocalError> {
    let user_terms = gate()
        .lock()
        .map(|gate| gate.user_terms.clone())
        .unwrap_or_default();
    let completion = complete_recording(
        recording_id,
        pcm,
        &user_terms,
        writing_mode,
        recovery_enabled,
        false,
    )?;
    match completion.decision {
        TerminalDecision::NoText { reason } => {
            abandon_live(recording_id);
            Ok(LocalStopResult::NoText { reason })
        }
        TerminalDecision::Transcript(text) => {
            prepare_delivery(recording_id, recovery_enabled)?;
            Ok(LocalStopResult::Transcript(text))
        }
    }
}

pub fn replay_local(
    recording_id: &str,
    fixture_name: &str,
    user_terms: Vec<String>,
    writing_mode: WritingMode,
    load_diagnostic: impl FnOnce(&str) -> Result<Vec<u8>, String>,
) -> Response {
    begin_local_session();
    let from_recovery = crate::local_recovery::is_local_origin(fixture_name);
    let bytes = if from_recovery {
        match crate::local_recovery::read_pcm(fixture_name) {
            Ok(bytes) => bytes,
            Err(error) => {
                return Response::rejected(Some(DaemonState::Idle), error.message());
            }
        }
    } else {
        match load_diagnostic(fixture_name) {
            Ok(bytes) => bytes,
            Err(error) => return Response::rejected(Some(DaemonState::Idle), error),
        }
    };
    match complete_replay(
        recording_id,
        bytes,
        &user_terms,
        writing_mode,
        false,
        from_recovery,
    ) {
        Ok(TerminalDecision::Transcript(text)) => {
            Response::success(DaemonState::Idle, format!("Replay completed: {text}"))
        }
        Ok(TerminalDecision::NoText { reason }) => Response::success(
            DaemonState::Idle,
            format!("Replay completed: no text ({reason})"),
        ),
        Err(error) => Response::rejected(Some(DaemonState::Idle), error.message()),
    }
}

pub fn refuse_cloud_recovery_replay(name: &str, mode: AsrMode) -> Option<String> {
    if mode == AsrMode::Cloud && crate::local_recovery::is_local_origin(name) {
        Some(RecoveryError::CloudSelected.message())
    } else {
        None
    }
}

pub fn prepare_delivery(recording_id: &str, recovery_enabled: bool) -> Result<(), LocalError> {
    let mut gate = gate().lock().map_err(|_| LocalError::Unavailable)?;
    let permit = gate
        .coordinator
        .authorize_live(recording_id)
        .map_err(LocalError::Tail)?;
    gate.pending_permit = Some(permit);
    if recovery_enabled {
        RecoveryStore::open_default(RecoveryClock::system())
            .and_then(|store| store.mark_delivery_started(recording_id))
            .map_err(LocalError::Recovery)?;
    }
    Ok(())
}

pub fn apply_local_delivery_evidence(
    evidence: &mut LifecycleEvidence,
    outcome: DeliveryOutcome,
    started_at: Instant,
    utterance_end: Instant,
    finalized_at: Instant,
) {
    evidence.delivery_count = evidence.delivery_count.saturating_add(1);
    evidence.delivery_method = Some(outcome.method);
    evidence.delivery_fallback_reason = outcome.fallback_reason;
    evidence.release_to_text_ms =
        Some(u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX));
    let delivered_at = Instant::now();
    let timings = stop_anchored_timings(started_at, utterance_end, finalized_at, delivered_at);
    evidence.recording_duration_ms = Some(timings.recording_duration_ms);
    evidence.stop_to_finalized_ms = Some(timings.stop_to_finalized_ms);
    evidence.stop_to_delivered_ms = Some(timings.stop_to_delivered_ms);
    evidence.stages.push(LifecycleStage::DeliveryCompleted);
}

pub fn finish_delivery(
    recording_id: &str,
    text: &str,
    recovery_enabled: bool,
) -> Result<(), LocalError> {
    let mut gate = gate().lock().map_err(|_| LocalError::Unavailable)?;
    let permit = gate
        .pending_permit
        .take()
        .ok_or(LocalError::Tail(TailError::UnauthorizedDelivery))?;
    gate.coordinator
        .confirm_delivery(permit, &Transcript(text.to_owned()))
        .map_err(LocalError::Tail)?;
    if recovery_enabled {
        RecoveryStore::open_default(RecoveryClock::system())
            .and_then(|store| store.mark_delivered(recording_id))
            .map_err(LocalError::Recovery)?;
    }
    Ok(())
}

pub fn authorize_and_confirm_delivery(
    recording_id: &str,
    text: &str,
    recovery_enabled: bool,
) -> Result<(), LocalError> {
    prepare_delivery(recording_id, recovery_enabled)?;
    finish_delivery(recording_id, text, recovery_enabled)
}

pub fn mark_recovery_failed(recording_id: &str, recovery_enabled: bool) {
    if recovery_enabled {
        let _ = RecoveryStore::open_default(RecoveryClock::system())
            .and_then(|store| store.mark_failed(recording_id));
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalCompletion {
    pub decision: TerminalDecision,
    pub recovery_enabled: bool,
    pub recording_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LocalError {
    Unavailable,
    CloudSelected,
    CloudCapability,
    Recovery(RecoveryError),
    Tail(TailError),
    Inference(String),
}

impl LocalError {
    pub fn message(&self) -> String {
        match self {
            Self::Unavailable => LOCAL_NOT_READY.to_owned(),
            Self::CloudSelected => RecoveryError::CloudSelected.message(),
            Self::CloudCapability => "Cloud capability used on the Local path".to_owned(),
            Self::Recovery(error) => {
                if error.is_storage() {
                    format!("recovery-storage: {}", error.message())
                } else {
                    error.message()
                }
            }
            Self::Tail(TailError::CloudCapabilityUsed) => {
                "Cloud capability used on the Local path".to_owned()
            }
            Self::Tail(error) => format!("{error:?}"),
            Self::Inference(message) => message.clone(),
        }
    }
}

pub fn quota_before_local_capture(recovery_enabled: bool) -> Result<(), LocalError> {
    if !recovery_enabled {
        return Ok(());
    }
    RecoveryStore::open_default(RecoveryClock::system())
        .and_then(|store| store.quota_before_capture())
        .map_err(LocalError::Recovery)
}

pub fn sentinel_is_clean() -> bool {
    gate()
        .lock()
        .map(|gate| gate.sentinel.local_path_clean())
        .unwrap_or(false)
}

fn test_local_ready() -> bool {
    std::env::var_os("VOISU_TEST_MODE").is_some()
        && std::env::var_os("VOISU_TEST_LOCAL_READY").is_some()
}

fn test_local_text() -> String {
    std::env::var("VOISU_TEST_LOCAL_TEXT").unwrap_or_else(|_| "hello".to_owned())
}

/// Test-only: attach a Ready worker and verified receipt without env.
#[cfg(test)]
pub fn inject_ready(receipt: ActiveReceipt, text: &str) {
    let mut gate = gate().lock().expect("gate");
    let hash = receipt.receipt_hash.clone();
    gate.receipt = Some(receipt.clone());
    gate.attach_ready_worker(hash, text);
    gate.readiness = LocalReadiness::Ready {
        model_identity: Some(receipt.catalog_id),
    };
    gate.sentinel = CloudCapabilitySentinel::new();
    gate.coordinator = DeliveryCoordinator::new();
    gate.pending_permit = None;
    gate.user_terms = Vec::new();
}

#[cfg(test)]
pub fn reset_gate_for_tests() {
    if let Ok(mut gate) = gate().lock() {
        *gate = Gate::new();
    }
}

#[cfg(test)]
pub fn test_session() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    let guard = LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    reset_gate_for_tests();
    guard
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_receipt() -> ActiveReceipt {
        crate::local_model::from_entry(crate::local_model::ci_fixture_entry(), "l4-artifact")
    }

    #[test]
    fn local_start_requires_receipt_and_ready_worker() {
        let _lock = test_session();
        match admit_local(true) {
            LocalAdmission::Unavailable { error } => {
                assert!(
                    error.contains("unavailable") || error.contains("refused"),
                    "{error}"
                );
            }
            LocalAdmission::Ready { .. } => panic!("empty gate must not admit"),
        }
        inject_ready(fixture_receipt(), "hello world");
        match admit_local(true) {
            LocalAdmission::Ready { .. } => {}
            LocalAdmission::Unavailable { error } => panic!("ready gate must admit: {error}"),
        }
    }

    #[test]
    fn local_path_never_uses_cloud_slots_or_sentinels() {
        let _lock = test_session();
        inject_ready(fixture_receipt(), "hello");
        let (left, right) = cloud_free_slots();
        drop(left);
        drop(right);
        assert!(sentinel_is_clean());
        begin_live("rec-1");
        let completion = complete_recording(
            "rec-1",
            vec![1, 0, 2, 0],
            &[],
            WritingMode::Literal,
            false,
            false,
        )
        .unwrap();
        assert_eq!(
            completion.decision,
            TerminalDecision::Transcript("hello".to_owned())
        );
        authorize_and_confirm_delivery("rec-1", "hello", false).unwrap();
        assert!(sentinel_is_clean());
    }

    #[test]
    fn status_stays_busy_while_inference_is_checked_out() {
        let _lock = test_session();
        inject_ready(fixture_receipt(), "hello");
        let session = take_inference_session().unwrap();
        assert_eq!(production_readiness(), LocalReadiness::Busy);
        restore_inference_session(session);
        assert!(matches!(
            production_readiness(),
            LocalReadiness::Ready { .. }
        ));
    }

    #[test]
    fn silence_is_no_text_and_cloud_selected_recovery_is_refused() {
        let _lock = test_session();
        inject_ready(fixture_receipt(), "hello");
        begin_live("rec-silent");
        let completion = complete_recording(
            "rec-silent",
            vec![0, 0, 0, 0],
            &[],
            WritingMode::Literal,
            false,
            false,
        )
        .unwrap();
        assert!(matches!(
            completion.decision,
            TerminalDecision::NoText { .. }
        ));
        let err = complete_replay("rec-old", vec![1, 0], &[], WritingMode::Literal, true, true)
            .unwrap_err();
        assert!(matches!(err, LocalError::CloudSelected));
    }

    #[test]
    fn clipboard_fallback_is_recorded_on_local_delivery_evidence() {
        let started = Instant::now();
        let utterance_end = started;
        let finalized_at = started;
        let mut evidence = LifecycleEvidence {
            recording_id: 1,
            correlation_id: "rec-1-1-1".into(),
            stages: vec![LifecycleStage::ValidationCompleted],
            delivery_count: 0,
            delivery_method: None,
            delivery_fallback_reason: None,
            streamed_chunk_count: 0,
            source_transcript_providers: Vec::new(),
            first_chunk_ms: None,
            capture_finalized_ms: None,
            truncated_by: None,
            provider_timings_ms: Vec::new(),
            provider_failures: Vec::new(),
            release_to_text_ms: None,
            recording_duration_ms: None,
            stop_to_finalized_ms: None,
            stop_to_delivered_ms: None,
            transcript_selection: None,
            validation_reason: None,
            fallback_reason: None,
            reconciliation_requested: false,
            recovery_attempted: false,
            source_selection_diagnostic: None,
            intent_reconstruction: None,
            confidence_arbitration: None,
            source_transcripts: Vec::new(),
            final_transcript: None,
        };
        apply_local_delivery_evidence(
            &mut evidence,
            DeliveryOutcome::clipboard_fallback("compositor submit failed"),
            started,
            utterance_end,
            finalized_at,
        );
        assert_eq!(evidence.delivery_count, 1);
        assert_eq!(
            evidence.delivery_method,
            Some(voisu_core::DeliveryMethod::ClipboardFallback)
        );
        assert_eq!(
            evidence.delivery_fallback_reason.as_deref(),
            Some("compositor submit failed")
        );
        assert!(evidence.stop_to_delivered_ms.is_some());
        assert_eq!(
            evidence.stages.last(),
            Some(&LifecycleStage::DeliveryCompleted)
        );
    }

    #[test]
    fn transcribe_through_supervisor_rejects_cloud_sentinel() {
        let mut sentinel = CloudCapabilitySentinel::new();
        sentinel.attempt_ip();
        let mut supervisor = crate::local_worker::WorkerSupervisor::<FakeWorker>::absent();
        supervisor
            .attach_ready(FakeWorker::default(), Instant::now())
            .unwrap();
        let corr = crate::local_worker::Correlation {
            daemon_nonce: "n".into(),
            generation: 1,
            request_id: "q".into(),
            recording_id: "rec-1".into(),
            model_receipt_hash: "h".into(),
        };
        assert!(
            transcribe_through_supervisor(&mut supervisor, corr, vec![1, 0], &sentinel).is_err()
        );
    }
}
