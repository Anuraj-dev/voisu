//! Local vs Cloud routing: choose the path before constructing dependencies.

mod silent;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use voisu_core::{
    AsrMode, DaemonState, DeliveryOutcome, LifecycleEvidence, LifecycleStage, LocalAsrDiagnostic,
    LocalAsrOutcome, LocalReadiness, Response, Transcript, stop_anchored_timings,
};

use crate::config::WritingMode;

use crate::local_model::ActiveReceipt;
#[cfg(not(test))]
use crate::local_model::{
    DevelopmentStore, FileKind, ModelStore, development_local_enabled, development_receipt,
    models_dir, runtime_selection, shipped_catalog,
};
use crate::local_recovery::{RecoveryClock, RecoveryError, RecoveryStore};
use crate::local_tail::{
    DeliveryCoordinator, DeliveryPermit, TailError, TerminalDecision, decide_replay,
    decide_transcript,
};
#[cfg(test)]
use crate::local_worker::FakeWorker;
use crate::local_worker::{
    CloudCapabilitySentinel, Correlation, FakeDelivery, InferenceMetrics, LocalLifecycle,
    WorkerOutcome, WorkerState, WorkerSupervisor, is_silence_pcm, transcribe_through_supervisor,
};
#[cfg(not(test))]
use crate::local_worker::{RuntimeWorker, WhisperCppWorker};

pub use silent::{CloudFreeProvider, cloud_free_slots};

const LOCAL_UNAVAILABLE: &str = "Local selected; model unavailable";
const LOCAL_NOT_READY: &str = "Local ASR is unavailable; Start refused before capture";

#[cfg(test)]
type GateWorker = FakeWorker;
#[cfg(not(test))]
type GateWorker = RuntimeWorker;

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
    published: Arc<Mutex<LocalReadiness>>,
    failed_observation: Option<crate::local_model::consented::ConsentedObservation>,
    receipt: Option<ActiveReceipt>,
    consented_observation: Option<(
        crate::local_model::consented::ConsentedObservation,
        ActiveReceipt,
    )>,
    lifecycle: LocalLifecycle<GateWorker, FakeDelivery>,
    coordinator: DeliveryCoordinator,
    sentinel: CloudCapabilitySentinel,
    published_sentinel: Arc<AtomicBool>,
    /// Pins this worker from admission through capture and inference. Refresh
    /// cannot replace or invalidate it until completion or abandonment.
    admitted_generation: Option<u64>,
    admitted_recording: Option<String>,
    pending_permit: Option<DeliveryPermit>,
    user_terms: Vec<String>,
    /// Supervisor is checked out for inference; refresh must not map the
    /// absent stand-in to Verifying/Loading.
    inference_checked_out: bool,
    runtime: Option<String>,
    requested_device: Option<String>,
    /// Why the last worker install failed to reach Ready (for example the
    /// worker's own `vram_admission` refusal), shown with the not-ready error.
    worker_failure: Option<String>,
    #[cfg(test)]
    test_selected: Option<ActiveReceipt>,
    #[cfg(test)]
    test_consented: Option<(
        crate::local_model::ModelStore,
        crate::local_model::ConsentedCatalogEntry,
    )>,
    #[cfg(test)]
    test_next_worker: Option<FakeWorker>,
    #[cfg(test)]
    launch_count: u32,
    #[cfg(test)]
    verification_count: std::cell::Cell<u32>,
    #[cfg(test)]
    test_before_refresh: Option<Box<dyn FnOnce() + Send>>,
}

impl Gate {
    fn new() -> Self {
        Self {
            readiness: LocalReadiness::Absent,
            published: Arc::new(Mutex::new(LocalReadiness::Unavailable {
                error: LOCAL_UNAVAILABLE.to_owned(),
            })),
            failed_observation: None,
            receipt: None,
            consented_observation: None,
            lifecycle: LocalLifecycle::new(FakeDelivery::default()),
            coordinator: DeliveryCoordinator::new(),
            sentinel: CloudCapabilitySentinel::new(),
            published_sentinel: Arc::new(AtomicBool::new(true)),
            admitted_generation: None,
            admitted_recording: None,
            pending_permit: None,
            user_terms: Vec::new(),
            inference_checked_out: false,
            runtime: None,
            requested_device: None,
            worker_failure: None,
            #[cfg(test)]
            test_selected: None,
            #[cfg(test)]
            test_consented: None,
            #[cfg(test)]
            test_next_worker: None,
            #[cfg(test)]
            launch_count: 0,
            #[cfg(test)]
            verification_count: std::cell::Cell::new(0),
            #[cfg(test)]
            test_before_refresh: None,
        }
    }

    fn publish(&self, readiness: LocalReadiness) {
        *self
            .published
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = readiness;
    }

    fn reset_sentinel(&mut self) {
        self.sentinel = CloudCapabilitySentinel::new();
        self.published_sentinel.store(true, Ordering::Release);
    }

    fn release_admission(&mut self) {
        self.admitted_generation = None;
        self.admitted_recording = None;
        if !self.inference_checked_out {
            self.readiness = if self.lifecycle.state() == WorkerState::Ready {
                LocalReadiness::Ready {
                    model_identity: self.receipt.as_ref().map(local_model_identity),
                }
            } else {
                self.not_ready()
            };
            self.publish(self.readiness.clone());
        }
    }

    fn admit(&mut self, kind_ready_required: bool) -> LocalAdmission {
        let readiness = self.refresh();
        if kind_ready_required && !readiness.admits_capture() {
            return LocalAdmission::Unavailable {
                error: match &readiness {
                    LocalReadiness::Unavailable { error } => error.clone(),
                    _ => LOCAL_NOT_READY.to_owned(),
                },
            };
        }
        match (self.receipt.clone(), readiness) {
            (Some(receipt), LocalReadiness::Ready { model_identity }) => {
                self.admitted_generation = Some(self.lifecycle.generation());
                self.readiness = LocalReadiness::Busy;
                self.publish(self.readiness.clone());
                LocalAdmission::Ready {
                    model_identity: model_identity.unwrap_or_else(|| receipt.catalog_id.clone()),
                    receipt,
                }
            }
            _ => LocalAdmission::Unavailable {
                error: LOCAL_NOT_READY.to_owned(),
            },
        }
    }

    fn refresh_passive(&mut self) -> LocalReadiness {
        if crate::local_model::runtime_selection(&crate::local_model::shipped_catalog()).is_none()
            && !crate::local_model::development_local_enabled()
            && matches!(self.readiness, LocalReadiness::Unavailable { .. })
            && self
                .failed_observation
                .as_ref()
                .is_some_and(|observation| observation.unchanged())
        {
            return self.readiness.clone();
        }
        self.refresh()
    }

    fn refresh(&mut self) -> LocalReadiness {
        self.refresh_with_completion(|| {})
    }

    fn refresh_with_completion(&mut self, after_attempt: impl FnOnce()) -> LocalReadiness {
        let consented_path =
            crate::local_model::runtime_selection(&crate::local_model::shipped_catalog()).is_none()
                && !crate::local_model::development_local_enabled();
        let observation = if consented_path
            && self.admitted_generation.is_none()
            && !self.inference_checked_out
        {
            self.observe_consented()
        } else {
            None
        };
        let readiness = self.refresh_inner();
        after_attempt();
        self.finish_refresh(readiness, consented_path, observation)
    }

    fn finish_refresh(
        &mut self,
        readiness: LocalReadiness,
        consented_path: bool,
        observation: Option<crate::local_model::consented::ConsentedObservation>,
    ) -> LocalReadiness {
        if consented_path && matches!(readiness, LocalReadiness::Unavailable { .. }) {
            // Never associate an old failure with a tree repaired during the
            // attempt. Incomplete preflight observations retain passive retries.
            self.failed_observation = observation.filter(|observation| observation.unchanged());
        } else {
            self.failed_observation = None;
        }
        self.publish(readiness.clone());
        readiness
    }

    fn observe_consented(&self) -> Option<crate::local_model::consented::ConsentedObservation> {
        #[cfg(test)]
        {
            self.test_consented.as_ref().and_then(|(store, entry)| {
                crate::local_model::consented::observe_with_entry(store, entry)
            })
        }
        #[cfg(not(test))]
        {
            let store = ModelStore::open(models_dir().ok()?).ok()?;
            crate::local_model::consented::observe_consented_install(&store)
        }
    }

    fn refresh_inner(&mut self) -> LocalReadiness {
        #[cfg(test)]
        if let Some(before_refresh) = self.test_before_refresh.take() {
            before_refresh();
        }
        if self.admitted_generation.is_some() {
            self.readiness = LocalReadiness::Busy;
            return self.readiness.clone();
        }
        if self.inference_checked_out {
            return self.readiness.clone();
        }
        match self.lifecycle.state() {
            WorkerState::Busy => {
                self.readiness = LocalReadiness::Busy;
                return self.readiness.clone();
            }
            WorkerState::Stopping => {
                self.readiness = LocalReadiness::Stopping;
                return self.readiness.clone();
            }
            _ => {}
        }
        let Some(selected) = self.peek_selected_receipt() else {
            if self.receipt.as_ref().is_some_and(is_consented_receipt) {
                self.lifecycle.mark_unavailable();
            }
            self.readiness = LocalReadiness::Unavailable {
                error: self
                    .worker_failure
                    .clone()
                    .unwrap_or_else(|| LOCAL_UNAVAILABLE.to_owned()),
            };
            return self.readiness.clone();
        };
        let changed = development_model_changed(
            self.receipt
                .as_ref()
                .map(|receipt| receipt.catalog_id.as_str()),
            self.receipt
                .as_ref()
                .map(|receipt| receipt.artifact_id.as_str()),
            &selected.catalog_id,
            &selected.artifact_id,
        );
        let changed = changed
            || self.receipt.as_ref().is_some_and(|receipt| {
                is_consented_receipt(receipt) && receipt.receipt_hash != selected.receipt_hash
            });
        if changed {
            // Keep the previous receipt until the replacement is Ready so a
            // failed drain cannot retarget launch at the new model.
            if self.lifecycle.begin_idle_switch().is_err()
                || !self.lifecycle.can_launch_replacement()
            {
                self.lifecycle.mark_unavailable();
                self.readiness = self.not_ready();
                return self.readiness.clone();
            }
            if !self.install_selected_worker(&selected) {
                self.lifecycle.mark_unavailable();
                self.readiness = self.not_ready();
                return self.readiness.clone();
            }
            self.receipt = Some(selected);
        } else if self.lifecycle.state() == WorkerState::Ready && self.receipt.is_some() {
            self.readiness = LocalReadiness::Ready {
                model_identity: self.receipt.as_ref().map(local_model_identity),
            };
            return self.readiness.clone();
        } else if self.lifecycle.has_child() {
            self.lifecycle.mark_unavailable();
            self.readiness = self.not_ready();
            return self.readiness.clone();
        } else if self.lifecycle.can_launch_replacement() {
            if self.install_selected_worker(&selected) {
                self.receipt = Some(selected);
            } else {
                self.lifecycle.mark_unavailable();
            }
        }
        self.readiness = match self.lifecycle.state() {
            WorkerState::Absent | WorkerState::Verifying => LocalReadiness::Verifying,
            WorkerState::Loading => LocalReadiness::Loading,
            WorkerState::Ready => LocalReadiness::Ready {
                model_identity: self.receipt.as_ref().map(local_model_identity),
            },
            WorkerState::Busy => LocalReadiness::Busy,
            WorkerState::Stopping => LocalReadiness::Stopping,
            WorkerState::Unavailable => self.not_ready(),
        };
        self.readiness.clone()
    }

    fn not_ready(&self) -> LocalReadiness {
        LocalReadiness::Unavailable {
            error: match &self.worker_failure {
                Some(detail) if detail == LOCAL_UNAVAILABLE => LOCAL_UNAVAILABLE.to_owned(),
                Some(detail) => format!("{LOCAL_NOT_READY}: {detail}"),
                None => LOCAL_NOT_READY.to_owned(),
            },
        }
    }

    fn verify_consented(
        &self,
    ) -> Result<crate::local_model::VerifiedConsentedInstall, crate::local_model::ConsentedError>
    {
        #[cfg(test)]
        {
            self.verify_consented_with_progress(|| {})
        }
        #[cfg(not(test))]
        {
            crate::local_worker::verify_default_consented_install()
        }
    }

    fn verify_consented_with_progress(
        &self,
        on_verifying: impl FnOnce(),
    ) -> Result<crate::local_model::VerifiedConsentedInstall, crate::local_model::ConsentedError>
    {
        #[cfg(test)]
        {
            self.verification_count
                .set(self.verification_count.get() + 1);
            let (store, entry) = self
                .test_consented
                .as_ref()
                .ok_or(crate::local_model::ConsentedError::Missing)?;
            crate::local_model::consented::verify_with_entry_notifying(store, entry, on_verifying)
        }
        #[cfg(not(test))]
        {
            use crate::local_model::{ConsentedError, models_dir_from_state};
            let dir =
                models_dir_from_state(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
                    .map_err(ConsentedError::Store)?;
            let store = ModelStore::open(dir).map_err(ConsentedError::Store)?;
            crate::local_model::consented::verify_consented_install_notifying(&store, on_verifying)
        }
    }

    fn peek_selected_receipt(&mut self) -> Option<ActiveReceipt> {
        // Shared production selection: developer pilot excludes consented
        // admission; a verified receipt is the only source of this candidate.
        #[cfg(test)]
        let consider_consented = self.test_consented.is_some();
        #[cfg(not(test))]
        let consider_consented = true;
        if consider_consented
            && crate::local_model::runtime_selection(&crate::local_model::shipped_catalog())
                .is_none()
            && !crate::local_model::development_local_enabled()
        {
            if let Some((observation, receipt)) = self.consented_observation.as_ref()
                && observation.unchanged()
            {
                return Some(receipt.clone());
            }
            self.consented_observation = None;
            if let Some(receipt) = crate::local_worker::select_consented_receipt(false, || {
                let install = self
                    .verify_consented_with_progress(|| self.publish(LocalReadiness::Verifying))
                    .map_err(|error| {
                        self.worker_failure = Some(match error {
                            crate::local_model::ConsentedError::Missing => {
                                LOCAL_UNAVAILABLE.to_owned()
                            }
                            _ => format!("{LOCAL_UNAVAILABLE}: {error:?}"),
                        });
                        error
                    })?;
                self.worker_failure = None;
                let receipt = crate::local_worker::consented_active_receipt(&install);
                let observation =
                    crate::local_model::consented::ConsentedObservation::after_verification(
                        &install,
                    );
                self.consented_observation = Some((observation, receipt));
                Ok(install)
            }) {
                return Some(receipt);
            }
        }
        #[cfg(test)]
        {
            if self.test_consented.is_some() {
                return None;
            }
            self.test_selected.clone().or_else(|| self.receipt.clone())
        }
        #[cfg(not(test))]
        {
            if crate::local_model::development_local_enabled() {
                match DevelopmentStore::open_default() {
                    Ok(store) => match store.selected() {
                        Ok((manifest, handle)) => {
                            return Some(development_receipt(&manifest, &handle.manifest_hash));
                        }
                        Err(crate::local_model::DevelopmentError::MissingSelection) => {}
                        Err(_) => return None,
                    },
                    Err(_) => return None,
                }
            } else if runtime_selection(&shipped_catalog()).is_none() {
                return None;
            }
            (|| {
                let dir = models_dir().ok()?;
                let store = ModelStore::open(dir).ok()?;
                store.load_active().ok().flatten()
            })()
        }
    }

    fn install_selected_worker(&mut self, selected: &ActiveReceipt) -> bool {
        self.worker_failure = None;
        if !self.lifecycle.can_launch_replacement() {
            return false;
        }
        self.publish(LocalReadiness::Loading);
        let Some(worker) = self.spawn_worker(selected) else {
            return false;
        };
        self.lifecycle
            .pin_model_receipt_hash(selected.receipt_hash.clone());
        if self
            .lifecycle
            .attach_replacement(worker, Instant::now())
            .is_err()
        {
            return false;
        }
        #[cfg(test)]
        {
            let generation = self.lifecycle.generation();
            let hash = selected.receipt_hash.clone();
            if let Some(child) = self.lifecycle.child_mut() {
                child.generation = generation;
                child.model_receipt_hash = hash;
            }
        }
        let prepared = self.lifecycle.prepare("l4-prepare", Instant::now());
        if let Err(error) = &prepared {
            self.worker_failure = Some(error.to_string());
        }
        let ready = prepared.is_ok() && self.lifecycle.state() == WorkerState::Ready;
        if ready {
            self.capture_runtime_identity(selected);
        }
        ready
    }

    fn capture_runtime_identity(&mut self, selected: &ActiveReceipt) {
        self.runtime = Some(selected.runtime_abi.clone());
        #[cfg(test)]
        {
            self.requested_device = consented_device(selected)
                .map(str::to_owned)
                .or_else(|| Some("cpu".into()));
        }
        #[cfg(not(test))]
        {
            if development_local_enabled()
                && let Ok((manifest, handle)) =
                    DevelopmentStore::open_default().and_then(|store| store.selected())
                && selected.catalog_id == manifest.model_id
                && selected.artifact_id == handle.manifest_hash
            {
                self.requested_device = Some(manifest.device.clone());
                if let Some(entry) =
                    crate::local_model::development_catalog_entry(&manifest.model_id)
                {
                    self.runtime = Some(entry.runtime.as_str().to_owned());
                }
            } else if is_consented_receipt(selected) {
                self.requested_device = consented_device(selected).map(str::to_owned);
                self.runtime = Some(
                    if self.requested_device.as_deref() == Some("cpu") {
                        "native"
                    } else {
                        "CTranslate2"
                    }
                    .into(),
                );
            } else {
                self.requested_device = Some("cpu".into());
            }
        }
    }

    fn spawn_worker(&mut self, selected: &ActiveReceipt) -> Option<GateWorker> {
        if !self.lifecycle.can_launch_replacement() {
            return None;
        }
        if !crate::local_model::development_local_enabled() && is_consented_receipt(selected) {
            // This exact re-verification and receipt comparison also runs in
            // tests. Only process creation is replaced by the scripted worker.
            let install = match self.verify_consented() {
                Ok(install)
                    if crate::local_worker::consented_active_receipt(&install) == *selected =>
                {
                    install
                }
                _ => {
                    self.worker_failure = Some(LOCAL_UNAVAILABLE.into());
                    return None;
                }
            };
            #[cfg(not(test))]
            {
                return match crate::local_worker::launch_consented_worker(install) {
                    Ok(worker) => Some(RuntimeWorker::Consented(Box::new(worker))),
                    Err(error) => {
                        self.worker_failure = Some(match error {
                            crate::local_model::ConsentedError::Health(detail) => detail,
                            crate::local_model::ConsentedError::GpuBusy => {
                                crate::local_worker::GPU_BUSY.to_owned()
                            }
                            error => format!("{error:?}"),
                        });
                        None
                    }
                };
            }
            #[cfg(test)]
            drop(install);
        }
        #[cfg(test)]
        {
            let _ = selected;
            self.launch_count = self.launch_count.saturating_add(1);
            self.test_next_worker.take()
        }
        #[cfg(not(test))]
        {
            if development_local_enabled()
                && let Ok((manifest, handle)) =
                    DevelopmentStore::open_default().and_then(|store| store.selected())
                && selected.catalog_id == manifest.model_id
                && selected.artifact_id == handle.manifest_hash
            {
                return match crate::local_worker::DevWorker::launch(
                    manifest,
                    handle.manifest_hash,
                    &handle.path,
                ) {
                    Ok(worker) => Some(RuntimeWorker::Development(Box::new(worker))),
                    Err(error) => {
                        self.worker_failure = Some(error.to_string());
                        None
                    }
                };
            }
            let catalog = shipped_catalog();
            let entry = catalog.entries.iter().find(|entry| {
                entry.id == selected.catalog_id && entry.revision == selected.catalog_revision
            })?;
            if !entry.production_weights {
                return None;
            }
            if runtime_selection(&catalog).map(|chosen| chosen.id) != Some(entry.id) {
                return None;
            }
            if entry.runtime_abi.protocol != crate::local_worker::PROTOCOL_VERSION
                || selected.runtime_abi != entry.runtime_abi.abi_id
                || selected.protocol != entry.runtime_abi.protocol
            {
                return None;
            }
            for file in entry.files {
                match selected.files.get(file.name) {
                    Some(pin) if pin.sha256_hex == file.sha256_hex && pin.bytes == file.bytes => {}
                    _ => return None,
                }
            }
            let weights = entry
                .files
                .iter()
                .find(|file| file.kind == FileKind::Weights)
                .map(|file| file.name)?;
            let artifact = (|| {
                let dir = models_dir().ok()?;
                let store = ModelStore::open(dir).ok()?;
                Some(
                    store
                        .root()
                        .join("artifacts")
                        .join(entry.id)
                        .join(entry.revision)
                        .join(&selected.artifact_id),
                )
            })()?;
            if !artifact.join(weights).is_file() {
                return None;
            }
            WhisperCppWorker::from_artifact(&artifact, weights, &selected.receipt_hash)
                .ok()
                .map(RuntimeWorker::Whisper)
        }
    }

    #[cfg(test)]
    fn attach_ready_worker(&mut self, hash: String, text: &str) {
        let mut supervisor = WorkerSupervisor::absent();
        let worker = FakeWorker {
            scripted_text: Some(text.to_owned()),
            generation: 1,
            model_receipt_hash: hash.clone(),
            ..FakeWorker::default()
        };
        if supervisor.attach_ready(worker, Instant::now()).is_err() {
            self.readiness = LocalReadiness::Unavailable {
                error: LOCAL_NOT_READY.to_owned(),
            };
            return;
        }
        let correlation = correlation(&hash, "l4-prepare", "prepare");
        if supervisor.prepare(correlation, Instant::now()).is_err() {
            self.readiness = LocalReadiness::Unavailable {
                error: LOCAL_NOT_READY.to_owned(),
            };
            return;
        }
        self.lifecycle.restore_supervisor(supervisor);
        self.lifecycle.pin_model_receipt_hash(hash);
        self.runtime = self
            .receipt
            .as_ref()
            .map(|receipt| receipt.runtime_abi.clone());
        self.requested_device = Some("cpu".into());
    }
}

struct InferenceSession {
    receipt_hash: String,
    catalog_id: String,
    language: String,
    runtime: Option<String>,
    requested_device: Option<String>,
    sentinel: CloudCapabilitySentinel,
    supervisor: WorkerSupervisor<GateWorker>,
    coordinator: DeliveryCoordinator,
}

fn take_inference_session() -> Result<InferenceSession, LocalError> {
    let mut gate = gate().lock().map_err(|_| LocalError::Unavailable)?;
    if !gate.sentinel.local_path_clean() {
        gate.release_admission();
        return Err(LocalError::CloudCapability);
    }
    let Some(receipt) = gate.receipt.clone() else {
        gate.release_admission();
        return Err(LocalError::Unavailable);
    };
    if gate.lifecycle.state() != WorkerState::Ready {
        gate.release_admission();
        return Err(LocalError::Unavailable);
    }
    if gate
        .admitted_generation
        .is_some_and(|generation| generation != gate.lifecycle.generation())
    {
        gate.release_admission();
        return Err(LocalError::Unavailable);
    }
    let supervisor = gate.lifecycle.take_supervisor();
    let coordinator = std::mem::replace(&mut gate.coordinator, DeliveryCoordinator::new());
    gate.inference_checked_out = true;
    gate.readiness = LocalReadiness::Busy;
    gate.publish(gate.readiness.clone());
    Ok(InferenceSession {
        receipt_hash: receipt.receipt_hash,
        catalog_id: receipt.catalog_id,
        language: receipt.language,
        runtime: gate.runtime.clone(),
        requested_device: gate.requested_device.clone(),
        sentinel: gate.sentinel.clone(),
        supervisor,
        coordinator,
    })
}

fn restore_inference_session(session: InferenceSession) {
    if let Ok(mut gate) = gate().lock() {
        gate.lifecycle.restore_supervisor(session.supervisor);
        gate.coordinator = session.coordinator;
        gate.inference_checked_out = false;
        gate.release_admission();
    }
}

fn silence_outcome(observed_device: &str) -> Result<WorkerOutcome, LocalError> {
    if observed_device.is_empty() || observed_device == "unknown" {
        return Err(LocalError::Inference(
            "silence inference lacked an observed device".into(),
        ));
    }
    Ok(WorkerOutcome::NoText {
        reason: "silence".to_owned(),
        observed_device: observed_device.to_owned(),
        metrics: crate::local_worker::InferenceMetrics::default(),
    })
}

fn infer_unlocked(
    session: &mut InferenceSession,
    recording_id: &str,
    request_id: &str,
    pcm: Vec<u8>,
) -> Result<WorkerOutcome, LocalError> {
    let correlation = correlation(&session.receipt_hash, recording_id, request_id);
    transcribe_through_supervisor(&mut session.supervisor, correlation, pcm, &session.sentinel)
        .map_err(|error| LocalError::Inference(error.to_string()))
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

fn development_model_changed(
    previous_id: Option<&str>,
    previous_hash: Option<&str>,
    selected_id: &str,
    selected_hash: &str,
) -> bool {
    match (previous_id, previous_hash) {
        (Some(id), Some(hash)) => id != selected_id || hash != selected_hash,
        _ => false,
    }
}

fn local_model_identity(receipt: &ActiveReceipt) -> String {
    if is_consented_receipt(receipt) {
        format!(
            "{} (Consented Install, {})",
            receipt.catalog_id,
            consented_device(receipt).unwrap_or("unknown")
        )
    } else {
        receipt.catalog_id.clone()
    }
}

// This in-memory identity is built only from a freshly verified receipt.
fn consented_device(receipt: &ActiveReceipt) -> Option<&str> {
    if !is_consented_receipt(receipt) {
        return None;
    }
    receipt
        .runtime_abi
        .rsplit_once('@')
        .map(|(_, device)| device)
        .filter(|device| matches!(*device, "cpu" | "cuda:0"))
}

fn is_consented_receipt(receipt: &ActiveReceipt) -> bool {
    receipt.runtime_abi.starts_with("voisu-consented:")
}

fn gate() -> &'static Mutex<Gate> {
    static GATE: OnceLock<Mutex<Gate>> = OnceLock::new();
    GATE.get_or_init(|| {
        let mut gate = Gate::new();
        gate.published = published_readiness().clone();
        gate.published_sentinel = published_sentinel().clone();
        Mutex::new(gate)
    })
}

fn published_readiness() -> &'static Arc<Mutex<LocalReadiness>> {
    static PUBLISHED: OnceLock<Arc<Mutex<LocalReadiness>>> = OnceLock::new();
    PUBLISHED.get_or_init(|| {
        Arc::new(Mutex::new(LocalReadiness::Unavailable {
            error: LOCAL_UNAVAILABLE.to_owned(),
        }))
    })
}

fn published_snapshot(published: &Mutex<LocalReadiness>) -> LocalReadiness {
    published
        .lock()
        .map(|readiness| readiness.clone())
        .unwrap_or_else(|_| refresh_failed_readiness())
}

fn published_sentinel() -> &'static Arc<AtomicBool> {
    static CLEAN: OnceLock<Arc<AtomicBool>> = OnceLock::new();
    CLEAN.get_or_init(|| Arc::new(AtomicBool::new(true)))
}

fn refresh_failed_readiness() -> LocalReadiness {
    LocalReadiness::Unavailable {
        error: "Local readiness failed; restart daemon to recover".to_owned(),
    }
}

fn publish_poisoned_gate(gate: std::sync::PoisonError<std::sync::MutexGuard<'_, Gate>>) {
    // Keep the mutex poisoned: worker ownership after a panic is uncertain.
    // Admission remains fail-closed until daemon restart; only publish failure.
    let mut gate = gate.into_inner();
    gate.readiness = refresh_failed_readiness();
    gate.publish(gate.readiness.clone());
}

fn schedule_readiness_refresh(
    gate: &'static Mutex<Gate>,
    refreshing: &'static AtomicBool,
) -> Option<std::thread::JoinHandle<()>> {
    if refreshing
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return None;
    }
    // Polling never waits for model hashing or worker startup. The same gate
    // owns refresh, admission and inference, so a pending refresh cannot launch
    // a replacement underneath an admitted worker.
    let result = std::thread::Builder::new()
        .name("local-readiness".into())
        .spawn(move || {
            struct Reset(&'static AtomicBool);
            impl Drop for Reset {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::Release);
                }
            }
            let _reset = Reset(refreshing);
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match gate.lock() {
                    Ok(mut gate) => {
                        gate.refresh_passive();
                    }
                    Err(poison) => publish_poisoned_gate(poison),
                }));
            if result.is_err() {
                match gate.lock() {
                    Err(poison) => publish_poisoned_gate(poison),
                    Ok(gate) => gate.publish(refresh_failed_readiness()),
                }
            }
        });
    match result {
        Ok(handle) => Some(handle),
        Err(_) => {
            refreshing.store(false, Ordering::Release);
            None
        }
    }
}

fn readiness_snapshot() -> LocalReadiness {
    published_snapshot(published_readiness())
}

#[must_use]
pub fn runtime_readiness() -> LocalReadiness {
    #[cfg(test)]
    {
        // Unit tests have no production worker. A published Verifying snapshot
        // is still the status answer, so a gate held by verification cannot
        // look like a confirmed failure.
        let readiness = readiness_snapshot();
        if matches!(readiness, LocalReadiness::Verifying) {
            return readiness;
        }
        LocalReadiness::Unavailable {
            error: LOCAL_UNAVAILABLE.to_owned(),
        }
    }
    #[cfg(not(test))]
    {
        static REFRESHING: AtomicBool = AtomicBool::new(false);
        let _ = schedule_readiness_refresh(gate(), &REFRESHING);
        readiness_snapshot()
    }
}

/// Capture admission runs on the admission queue, never on the status path.
/// Unlike a polling snapshot it must wait for and revalidate the selected model.
pub fn admission_readiness() -> LocalReadiness {
    #[cfg(test)]
    {
        runtime_readiness()
    }
    #[cfg(not(test))]
    {
        match gate().lock() {
            Ok(mut gate) => gate.refresh(),
            Err(poison) => {
                publish_poisoned_gate(poison);
                refresh_failed_readiness()
            }
        }
    }
}

#[must_use]
pub fn admit_local(kind_ready_required: bool) -> LocalAdmission {
    match gate().lock() {
        Ok(mut gate) => gate.admit(kind_ready_required),
        Err(poison) => {
            publish_poisoned_gate(poison);
            LocalAdmission::Unavailable {
                error: LOCAL_NOT_READY.to_owned(),
            }
        }
    }
}

pub fn begin_live(recording_id: &str) {
    begin_live_with_terms(recording_id, Vec::new());
}

pub fn begin_live_with_terms(recording_id: &str, user_terms: Vec<String>) {
    if let Ok(mut gate) = gate().lock() {
        gate.coordinator.begin_live(recording_id);
        gate.admitted_recording = Some(recording_id.to_owned());
        gate.user_terms = user_terms;
        gate.pending_permit = None;
        gate.reset_sentinel();
    }
}

pub fn begin_local_session() {
    if let Ok(mut gate) = gate().lock() {
        gate.reset_sentinel();
    }
}

pub fn note_cloud_capability_used() {
    published_sentinel().store(false, Ordering::Release);
    if let Ok(mut gate) = gate().lock() {
        gate.sentinel.construct_cloud_client();
        gate.published_sentinel.store(false, Ordering::Release);
    }
}

pub fn abandon_live(recording_id: &str) {
    if let Ok(mut gate) = gate().lock() {
        gate.coordinator.abandon_live(recording_id);
        if gate
            .admitted_recording
            .as_deref()
            .is_none_or(|current| current == recording_id)
        {
            gate.release_admission();
        }
    }
}

/// The worker reservation made by a Local capture admission, held until the
/// admitted work takes it over. Dropping it on any exit before then (shutdown,
/// a cancelled or superseded admission, a quota refusal) releases the worker so
/// later admissions are not refused until restart.
#[must_use = "dropping an unstarted admission releases the reserved worker"]
pub struct UnstartedAdmission {
    armed: bool,
}

impl UnstartedAdmission {
    /// Claims the reservation of a successful [`admit_local`]; `local` is false
    /// for a Cloud admission, which reserved nothing.
    pub fn claim(local: bool) -> Self {
        Self { armed: local }
    }

    /// Capture begins: a Local Recording now holds the reservation until
    /// completion or [`abandon_live`]. A Cloud claim begins nothing.
    pub fn begin_live_with_terms(mut self, recording_id: &str, user_terms: Vec<String>) {
        if std::mem::take(&mut self.armed) {
            begin_live_with_terms(recording_id, user_terms);
        }
    }

    /// Hands the reservation to work that releases it itself (Local replay).
    pub fn hand_off(mut self) {
        self.armed = false;
    }
}

impl Drop for UnstartedAdmission {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(mut gate) = gate().lock()
            // Only an admission no Recording or inference has taken over; a
            // begun Recording keeps its worker until it completes or abandons.
            && gate.admitted_generation.is_some()
            && gate.admitted_recording.is_none()
            && !gate.inference_checked_out
        {
            gate.release_admission();
        }
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
        Err(_) if is_silence_pcm(&pcm) => {
            match silence_outcome(session.supervisor.observed_device()) {
                Ok(outcome) => outcome,
                Err(silence_error) => {
                    restore_inference_session(session);
                    if recovery_enabled {
                        let _ = RecoveryStore::open_default(RecoveryClock::system())
                            .and_then(|store| store.mark_failed(recording_id));
                    }
                    return Err(silence_error);
                }
            }
        }
        Err(error) => {
            restore_inference_session(session);
            if recovery_enabled {
                let _ = RecoveryStore::open_default(RecoveryClock::system())
                    .and_then(|store| store.mark_failed(recording_id));
            }
            return Err(error);
        }
    };
    let local_asr = local_asr_from_session(&session, &outcome);
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
        local_asr,
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
    Transcript {
        text: String,
        local_asr: LocalAsrDiagnostic,
    },
    NoText {
        reason: String,
        local_asr: LocalAsrDiagnostic,
    },
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
            Ok(LocalStopResult::NoText {
                reason,
                local_asr: completion.local_asr,
            })
        }
        TerminalDecision::Transcript(text) => {
            prepare_delivery(recording_id, recovery_enabled)?;
            Ok(LocalStopResult::Transcript {
                text,
                local_asr: completion.local_asr,
            })
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
                abandon_live(recording_id);
                return Response::rejected(Some(DaemonState::Idle), error.message());
            }
        }
    } else {
        match load_diagnostic(fixture_name) {
            Ok(bytes) => bytes,
            Err(error) => {
                abandon_live(recording_id);
                return Response::rejected(Some(DaemonState::Idle), error);
            }
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
    pub local_asr: LocalAsrDiagnostic,
}

/// Identity-only snapshot when inference never produced an outcome (capture
/// abort, supervisor error). Callers still persist a Local ASR record so the
/// selected model is visible in history.
pub fn current_local_asr_error(reason: &str) -> Option<LocalAsrDiagnostic> {
    let gate = gate().lock().ok()?;
    let receipt = gate.receipt.as_ref()?;
    let mut diagnostic = LocalAsrDiagnostic {
        model_id: receipt.catalog_id.clone(),
        outcome: LocalAsrOutcome::Error,
        runtime: gate.runtime.clone(),
        backend: None,
        language: nonempty_owned(&receipt.language),
        requested_device: gate.requested_device.clone(),
        observed_device: None,
        resident: None,
        manifest_hash: nonempty_owned(&receipt.receipt_hash),
        reason: nonempty_owned(reason),
        inference_ms: None,
        peak_host_ram_bytes: None,
        peak_gpu_memory_bytes: None,
    };
    diagnostic.normalize();
    Some(diagnostic)
}

fn local_asr_from_session(
    session: &InferenceSession,
    outcome: &WorkerOutcome,
) -> LocalAsrDiagnostic {
    let (kind, observed, reason, metrics) = match outcome {
        WorkerOutcome::Transcript {
            observed_device,
            metrics,
            ..
        } => (
            LocalAsrOutcome::Transcript,
            observed_device.as_str(),
            None,
            metrics.clone(),
        ),
        WorkerOutcome::NoText {
            reason,
            observed_device,
            metrics,
        } => (
            LocalAsrOutcome::NoText,
            observed_device.as_str(),
            Some(reason.clone()),
            metrics.clone(),
        ),
    };
    let InferenceMetrics {
        backend,
        resident,
        inference_ms,
        peak_host_ram_bytes,
        peak_gpu_memory_bytes,
        ..
    } = metrics;
    let mut diagnostic = LocalAsrDiagnostic {
        model_id: session.catalog_id.clone(),
        outcome: kind,
        runtime: session.runtime.clone(),
        backend,
        language: nonempty_owned(&session.language),
        requested_device: session.requested_device.clone(),
        observed_device: nonempty_owned(observed),
        resident,
        manifest_hash: nonempty_owned(&session.receipt_hash),
        reason,
        inference_ms,
        peak_host_ram_bytes,
        peak_gpu_memory_bytes,
    };
    diagnostic.normalize();
    diagnostic
}

fn nonempty_owned(value: &str) -> Option<String> {
    if value.is_empty() || value == "unknown" {
        None
    } else {
        Some(value.to_owned())
    }
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
    let result = RecoveryStore::open_default(RecoveryClock::system())
        .and_then(|store| store.quota_before_capture())
        .map_err(LocalError::Recovery);
    if result.is_err()
        && let Ok(mut gate) = gate().lock()
    {
        gate.release_admission();
    }
    result
}

pub fn sentinel_is_clean() -> bool {
    published_sentinel().load(Ordering::Acquire)
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
    gate.reset_sentinel();
    gate.coordinator = DeliveryCoordinator::new();
    gate.pending_permit = None;
    gate.user_terms = Vec::new();
}

#[cfg(test)]
pub fn reset_gate_for_tests() {
    if let Ok(mut gate) = gate().lock() {
        *gate = Gate::new();
        gate.published = published_readiness().clone();
        gate.published_sentinel = published_sentinel().clone();
        gate.reset_sentinel();
        gate.publish(LocalReadiness::Absent);
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

    fn consented_gate() -> (tempfile::TempDir, Gate, ActiveReceipt) {
        let state = tempfile::tempdir().unwrap();
        let store = crate::local_model::ModelStore::open(state.path().join("models")).unwrap();
        crate::local_model::consented::install_tests::install(&store);
        let entry = crate::local_model::consented::install_tests::entry();
        let install = crate::local_model::consented::verify_with_entry(&store, &entry).unwrap();
        let receipt = crate::local_worker::consented_active_receipt(&install);
        let mut gate = Gate::new();
        gate.test_consented = Some((store, entry));
        gate.test_next_worker = Some(FakeWorker {
            model_receipt_hash: receipt.receipt_hash.clone(),
            observed_device: "cuda:0".into(),
            ..Default::default()
        });
        (state, gate, receipt)
    }

    #[test]
    fn status_and_doctor_report_device_from_verified_consented_receipt() {
        for device in ["cpu", "cuda:0"] {
            let temp = tempfile::tempdir().unwrap();
            let store = crate::local_model::ModelStore::open(temp.path().join("models")).unwrap();
            let mut entry = crate::local_model::consented::install_tests::entry();
            entry.device = device;
            let receipt =
                crate::local_model::consented::install_tests::install_fixture_entry(&store, &entry);
            let install = crate::local_model::consented::verify_with_entry(&store, &entry).unwrap();
            let active = crate::local_worker::consented_active_receipt(&install);
            let identity = local_model_identity(&active);
            assert_eq!(consented_device(&active), Some(receipt.device.as_str()));
            let status = voisu_core::AsrModeStatus {
                pending: Some(AsrMode::Local),
                local_readiness: LocalReadiness::Ready {
                    model_identity: Some(identity),
                },
                active: None,
                revision: None,
                admission_error: None,
                audio_retention: None,
                local_path_clean: Some(true),
            };
            assert!(
                crate::local_status::format_cli_status("status", Some(&status)).contains(device)
            );
            assert!(
                crate::local_doctor::rows_from_status(&status)
                    .iter()
                    .any(|row| row.value.contains(device))
            );
        }
    }

    #[test]
    fn missing_consented_install_never_publishes_verifying() {
        let state = tempfile::tempdir().unwrap();
        let store = crate::local_model::ModelStore::open(state.path().join("models")).unwrap();
        let mut gate = Gate::new();
        gate.test_consented = Some((store, crate::local_model::consented::install_tests::entry()));
        let unavailable = LocalReadiness::Unavailable {
            error: LOCAL_UNAVAILABLE.to_owned(),
        };
        assert_eq!(published_snapshot(&gate.published), unavailable);
        assert!(gate.peek_selected_receipt().is_none());
        assert_eq!(published_snapshot(&gate.published), unavailable);
        assert_eq!(gate.refresh(), unavailable);
        assert!(!gate.readiness.admits_capture());
        assert_eq!(gate.launch_count, 0);
    }

    #[test]
    fn inadmissible_consented_install_never_publishes_verifying() {
        for missing_install in [false, true] {
            let (_state, mut gate, _) = consented_gate();
            let install = gate.verify_consented().unwrap();
            let root = install.layout().root.clone();
            drop(install);
            let (store, _) = gate.test_consented.as_ref().unwrap();
            if missing_install {
                std::fs::remove_dir_all(root).unwrap();
            } else {
                std::fs::write(
                    store.root().join("private/consented.receipt.json"),
                    b"invalid",
                )
                .unwrap();
            }
            let unavailable = published_snapshot(&gate.published);
            assert!(gate.peek_selected_receipt().is_none());
            assert_eq!(published_snapshot(&gate.published), unavailable);
            assert!(matches!(
                gate.refresh(),
                LocalReadiness::Unavailable { error } if error.starts_with(LOCAL_UNAVAILABLE)
            ));
            assert_eq!(gate.launch_count, 0);
        }
    }

    #[test]
    fn consented_verification_publishes_before_file_hashing() {
        let (_state, owned, _) = consented_gate();
        let install = owned.verify_consented().unwrap();
        let weights = install.layout().weights.join("model.bin");
        let bytes = std::fs::read(&weights).unwrap();
        drop(install);
        let published = owned.published.clone();
        let gate = Mutex::new(owned);
        let (stage_tx, stage_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let verifier = std::thread::spawn(move || {
            let held = gate.lock().unwrap();
            held.verify_consented_with_progress(|| {
                held.publish(LocalReadiness::Verifying);
                stage_tx.send(()).unwrap();
                resume_rx.recv().unwrap();
            })
        });
        stage_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("an installed model must enter verification");
        let during_verification = published_snapshot(&published);
        // A mutation after the notification must still be caught by hashing.
        std::fs::write(weights, vec![b'x'; bytes.len()]).unwrap();
        resume_tx.send(()).unwrap();
        let result = verifier.join().unwrap();
        assert_eq!(during_verification, LocalReadiness::Verifying);
        assert!(matches!(
            result,
            Err(crate::local_model::ConsentedError::Digest(_))
        ));
    }

    #[test]
    fn queued_refresh_without_an_install_stays_unavailable() {
        let owned = Gate::new();
        let published = owned.published.clone();
        let gate: &'static Mutex<Gate> = Box::leak(Box::new(Mutex::new(owned)));
        let refreshing = Box::leak(Box::new(AtomicBool::new(false)));
        let held = gate.lock().unwrap();
        let worker = schedule_readiness_refresh(gate, refreshing).unwrap();
        let unavailable = LocalReadiness::Unavailable {
            error: LOCAL_UNAVAILABLE.to_owned(),
        };
        let queued = published_snapshot(&published);
        drop(held);
        worker.join().unwrap();
        assert_eq!(queued, unavailable);
        assert_eq!(published_snapshot(&published), unavailable);
    }

    #[test]
    fn failure_suppression_is_not_retained_outside_consented_path() {
        let (_state, mut gate, _) = consented_gate();
        let install = gate.verify_consented().unwrap();
        std::fs::write(install.layout().weights.join("model.bin"), b"bad weight").unwrap();
        drop(install);
        let failure = gate.refresh();
        assert!(gate.failed_observation.is_some());
        gate.finish_refresh(failure, false, None);
        assert!(gate.failed_observation.is_none());
        let verifications = gate.verification_count.get();
        gate.refresh_passive();
        assert!(
            gate.verification_count.get() > verifications,
            "non-consented failures must retain passive retries"
        );
    }

    #[test]
    fn published_ready_does_not_flicker_during_passive_refresh() {
        let (_state, mut owned, _) = consented_gate();
        let ready = owned.refresh();
        assert!(matches!(ready, LocalReadiness::Ready { .. }));
        let published = owned.published.clone();
        let gate: &'static Mutex<Gate> = Box::leak(Box::new(Mutex::new(owned)));
        let refreshing = Box::leak(Box::new(AtomicBool::new(false)));
        let held = gate.lock().unwrap();
        let worker = schedule_readiness_refresh(gate, refreshing).unwrap();
        assert_eq!(published_snapshot(&published), ready);
        drop(held);
        worker.join().unwrap();
        assert_eq!(published_snapshot(&published), ready);
    }

    #[test]
    fn confirmed_verification_failure_stays_visible_until_tree_changes() {
        let (_state, mut gate, _) = consented_gate();
        let install = gate.verify_consented().unwrap();
        let path = install.layout().weights.join("model.bin");
        let original = std::fs::read(&path).unwrap();
        std::fs::write(&path, vec![b'x'; original.len()]).unwrap();
        drop(install);
        let failure = gate.refresh();
        assert!(
            matches!(&failure, LocalReadiness::Unavailable { error } if error.contains("Digest"))
        );
        let verifications = gate.verification_count.get();
        for _ in 0..3 {
            assert_eq!(gate.refresh_passive(), failure);
            assert_eq!(published_snapshot(&gate.published), failure);
        }
        assert_eq!(gate.verification_count.get(), verifications);
        std::fs::write(&path, original).unwrap();
        assert!(matches!(
            gate.refresh_passive(),
            LocalReadiness::Ready { .. }
        ));
    }

    #[test]
    fn missing_tree_recovers_without_rewriting_receipt() {
        let (_state, mut gate, _) = consented_gate();
        let install = gate.verify_consented().unwrap();
        let root = install.layout().root.clone();
        drop(install);
        let saved = root.with_extension("saved");
        std::fs::rename(&root, &saved).unwrap();
        assert!(matches!(gate.refresh(), LocalReadiness::Unavailable { .. }));
        std::fs::rename(&saved, &root).unwrap();
        assert!(matches!(
            gate.refresh_passive(),
            LocalReadiness::Ready { .. }
        ));
    }

    #[test]
    fn repair_before_failure_publication_is_not_cached() {
        let (_state, mut gate, _) = consented_gate();
        let install = gate.verify_consented().unwrap();
        let path = install.layout().weights.join("model.bin");
        let original = std::fs::read(&path).unwrap();
        drop(install);
        std::fs::write(&path, vec![b'x'; original.len()]).unwrap();
        let failure = gate.refresh_with_completion(|| std::fs::write(&path, original).unwrap());
        assert!(
            matches!(&failure, LocalReadiness::Unavailable { error } if error.contains("Digest"))
        );
        assert!(gate.failed_observation.is_none());
        assert!(matches!(
            gate.refresh_passive(),
            LocalReadiness::Ready { .. }
        ));
    }

    #[test]
    fn admission_reserves_worker_across_queued_refresh_until_abandoned() {
        let _session = test_session();
        inject_ready(fixture_receipt(), "original");
        assert!(matches!(admit_local(true), LocalAdmission::Ready { .. }));
        let admitted_generation = gate().lock().unwrap().lifecycle.generation();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let refresh_barrier = barrier.clone();
        let refresher = std::thread::spawn(move || {
            refresh_barrier.wait();
            gate().lock().unwrap().refresh_passive()
        });
        {
            let mut held = gate().lock().unwrap();
            held.test_selected = Some(parakeet_receipt());
            held.test_next_worker = Some(FakeWorker::default());
        }
        barrier.wait();
        assert_eq!(refresher.join().unwrap(), LocalReadiness::Busy);
        begin_live("reserved");
        abandon_live("stale-recording");
        {
            let mut held = gate().lock().unwrap();
            assert_eq!(held.refresh(), LocalReadiness::Busy);
            assert_eq!(held.lifecycle.generation(), admitted_generation);
            assert_eq!(held.receipt.as_ref().unwrap(), &fixture_receipt());
            assert_eq!(held.launch_count, 0);
        }
        abandon_live("reserved");
        assert!(matches!(admit_local(true), LocalAdmission::Ready { .. }));
        assert_eq!(
            gate().lock().unwrap().receipt.as_ref().unwrap(),
            &parakeet_receipt()
        );
    }

    #[test]
    fn verification_and_loading_replace_stale_unavailable_snapshot() {
        let (_state, mut gate, _) = consented_gate();
        gate.publish(LocalReadiness::Unavailable {
            error: "previous failure".into(),
        });
        // Selection verifies before launch; its stage is published through the
        // independent short mutex rather than falling back to a gate-lock error.
        let selected = gate.peek_selected_receipt().unwrap();
        assert_eq!(
            published_snapshot(&gate.published),
            LocalReadiness::Verifying
        );
        assert!(gate.install_selected_worker(&selected));
        gate.receipt = Some(selected);
        assert_eq!(published_snapshot(&gate.published), LocalReadiness::Loading);
        assert!(matches!(gate.refresh(), LocalReadiness::Ready { .. }));
        assert!(matches!(
            published_snapshot(&gate.published),
            LocalReadiness::Ready { .. }
        ));
    }

    #[test]
    fn readiness_refresh_is_single_flight_and_releases_ownership() {
        let gate: &'static Mutex<Gate> = Box::leak(Box::new(Mutex::new(Gate::new())));
        let refreshing = Box::leak(Box::new(AtomicBool::new(false)));
        let held = gate.lock().unwrap();
        let first = schedule_readiness_refresh(gate, refreshing).unwrap();
        assert!(schedule_readiness_refresh(gate, refreshing).is_none());
        drop(held);
        first.join().unwrap();
        assert!(!refreshing.load(Ordering::Acquire));
        schedule_readiness_refresh(gate, refreshing)
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn refresh_panic_publishes_terminal_failure_and_releases_ownership() {
        for stale in [
            LocalReadiness::Ready {
                model_identity: Some("old worker".into()),
            },
            LocalReadiness::Verifying,
            LocalReadiness::Loading,
        ] {
            let mut owned = Gate::new();
            owned.publish(stale);
            owned.test_before_refresh = Some(Box::new(|| panic!("refresh failure")));
            let published = owned.published.clone();
            let gate: &'static Mutex<Gate> = Box::leak(Box::new(Mutex::new(owned)));
            let refreshing = Box::leak(Box::new(AtomicBool::new(false)));
            schedule_readiness_refresh(gate, refreshing)
                .unwrap()
                .join()
                .unwrap();
            assert!(!refreshing.load(Ordering::Acquire));
            assert!(gate.is_poisoned());
            assert_eq!(published_snapshot(&published), refresh_failed_readiness());
            // A later poll takes the poisoned-lock path and must not restart
            // workers or resurrect the previous Ready/progress snapshot.
            schedule_readiness_refresh(gate, refreshing)
                .unwrap()
                .join()
                .unwrap();
            assert!(!refreshing.load(Ordering::Acquire));
            assert_eq!(published_snapshot(&published), refresh_failed_readiness());
        }
    }

    #[test]
    fn audited_extra_permission_change_refuses_status_and_capture() {
        use std::os::unix::fs::PermissionsExt;
        for admission in [false, true] {
            let (_state, mut gate, _) = consented_gate();
            let install = gate.verify_consented().unwrap();
            let cache = install
                .layout()
                .root
                .join("runtime/venv/lib/python3.11/site-packages/fixture/__pycache__");
            drop(install);
            std::fs::create_dir(&cache).unwrap();
            let path = cache.join("fixture.pyc");
            std::fs::write(&path, b"cache").unwrap();
            assert!(matches!(gate.refresh(), LocalReadiness::Ready { .. }));
            let verifications = gate.verification_count.get();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
            if admission {
                assert!(matches!(
                    gate.admit(true),
                    LocalAdmission::Unavailable { .. }
                ));
            } else {
                assert!(matches!(
                    gate.refresh_passive(),
                    LocalReadiness::Unavailable { .. }
                ));
            }
            assert!(gate.verification_count.get() > verifications);
        }
    }

    #[test]
    fn unstarted_admission_releases_the_worker_when_capture_never_begins() {
        let _session = test_session();
        inject_ready(fixture_receipt(), "original");
        assert!(matches!(admit_local(true), LocalAdmission::Ready { .. }));
        let unstarted = UnstartedAdmission::claim(true);
        assert_eq!(readiness_snapshot(), LocalReadiness::Busy);
        assert!(matches!(
            admit_local(true),
            LocalAdmission::Unavailable { .. }
        ));
        // The daemon refuses before capture (quota, shutdown, superseded
        // admission): the claim is dropped without begin_live or abandon_live.
        drop(unstarted);
        assert!(matches!(readiness_snapshot(), LocalReadiness::Ready { .. }));
        assert!(matches!(admit_local(true), LocalAdmission::Ready { .. }));
    }

    #[test]
    fn unstarted_admission_never_releases_a_begun_recording() {
        let _session = test_session();
        inject_ready(fixture_receipt(), "original");
        assert!(matches!(admit_local(true), LocalAdmission::Ready { .. }));
        UnstartedAdmission::claim(true).begin_live_with_terms("running", Vec::new());
        // A stale claim dropped while a Recording runs must not free its worker.
        drop(UnstartedAdmission::claim(true));
        {
            let mut held = gate().lock().unwrap();
            held.test_selected = Some(parakeet_receipt());
            held.test_next_worker = Some(FakeWorker::default());
            assert_eq!(held.refresh_passive(), LocalReadiness::Busy);
            assert_eq!(held.receipt.as_ref().unwrap(), &fixture_receipt());
        }
        abandon_live("running");
        assert!(matches!(admit_local(true), LocalAdmission::Ready { .. }));
    }

    #[test]
    fn reserved_worker_completes_before_receipt_replacement() {
        let _session = test_session();
        inject_ready(fixture_receipt(), "original");
        assert!(matches!(admit_local(true), LocalAdmission::Ready { .. }));
        begin_live("reserved-completion");
        {
            let mut held = gate().lock().unwrap();
            held.test_selected = Some(parakeet_receipt());
            held.test_next_worker = Some(FakeWorker::default());
            assert_eq!(held.refresh_passive(), LocalReadiness::Busy);
        }
        let completion = complete_recording(
            "reserved-completion",
            vec![1, 0],
            &[],
            WritingMode::Literal,
            false,
            false,
        )
        .unwrap();
        assert_eq!(
            completion.decision,
            TerminalDecision::Transcript("original".into())
        );
        let mut held = gate().lock().unwrap();
        assert!(held.admitted_generation.is_none());
        assert_eq!(held.receipt.as_ref().unwrap(), &fixture_receipt());
        assert!(matches!(
            held.refresh_passive(),
            LocalReadiness::Ready { .. }
        ));
        assert_eq!(held.receipt.as_ref().unwrap(), &parakeet_receipt());
    }

    #[test]
    fn status_and_sentinel_do_not_wait_for_gate_verification() {
        let _session = test_session();
        let state = tempfile::tempdir().unwrap();
        let config = state.path().join("config.toml");
        crate::asr_mode::persist_asr_mode_at(&config, state.path(), voisu_core::AsrMode::Local)
            .unwrap();
        // Hold the exact lock owned by model verification/worker preparation.
        let held = gate().lock().unwrap();
        held.publish(LocalReadiness::Verifying);
        assert_eq!(readiness_snapshot(), LocalReadiness::Verifying);
        let (tx, rx) = std::sync::mpsc::channel();
        let state_path = state.path().to_path_buf();
        let caller = std::thread::spawn(move || {
            let status = crate::asr_mode::status_report_at(&config, &Some(state_path), None);
            tx.send(status).unwrap();
        });
        let status = rx.recv_timeout(std::time::Duration::from_secs(2));
        drop(held);
        caller.join().unwrap();
        let status = status.expect("status must reply while verification owns the gate");
        assert_eq!(status.local_readiness, LocalReadiness::Verifying);
        assert_eq!(status.local_path_clean, Some(true));
        note_cloud_capability_used();
        let _held = gate().lock().unwrap();
        assert!(
            !sentinel_is_clean(),
            "contention must also preserve actual taint"
        );
    }

    #[test]
    fn same_size_model_edit_invalidates_verified_observation() {
        let (_state, mut gate, _) = consented_gate();
        assert!(matches!(gate.refresh(), LocalReadiness::Ready { .. }));
        let install = gate.verify_consented().unwrap();
        let path = install.layout().weights.join("model.bin");
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, vec![b'x'; bytes.len()]).unwrap();
        drop(install);
        assert!(matches!(gate.refresh(), LocalReadiness::Unavailable { .. }));
        assert_eq!(gate.lifecycle.state(), WorkerState::Unavailable);
        assert_eq!(gate.launch_count, 1);
    }

    #[test]
    fn withdrawn_receipt_invalidates_verified_observation() {
        let (_state, mut gate, _) = consented_gate();
        assert!(matches!(gate.refresh(), LocalReadiness::Ready { .. }));
        let (store, _) = gate.test_consented.as_ref().unwrap();
        std::fs::remove_file(store.root().join("private/consented.receipt.json")).unwrap();
        assert!(matches!(gate.refresh(), LocalReadiness::Unavailable { .. }));
        assert_eq!(gate.lifecycle.state(), WorkerState::Unavailable);
    }

    #[test]
    fn unchanged_ready_refresh_does_not_rehash_model_files() {
        let (_state, mut gate, _) = consented_gate();
        assert!(matches!(gate.refresh(), LocalReadiness::Ready { .. }));
        let verifications = gate.verification_count.get();
        for _ in 0..3 {
            assert!(matches!(gate.refresh(), LocalReadiness::Ready { .. }));
        }
        assert_eq!(gate.verification_count.get(), verifications);
    }

    #[test]
    fn consented_verified_install_is_excluded_by_dev_pilot_precedence() {
        let (_state, gate, receipt) = consented_gate();
        assert_eq!(
            crate::local_worker::select_consented_receipt(false, || gate.verify_consented()),
            Some(receipt)
        );
        assert!(
            crate::local_worker::select_consented_receipt(true, || panic!(
                "pilot must not verify consented install"
            ))
            .is_none()
        );
    }

    #[test]
    fn consented_verified_receipt_opens_local_and_corruption_closes_it() {
        let (_state, mut gate, receipt) = consented_gate();
        assert_eq!(gate.peek_selected_receipt(), Some(receipt.clone()));
        assert!(matches!(gate.refresh(), LocalReadiness::Ready { .. }));
        assert_eq!(gate.receipt, Some(receipt));
        let install = gate.verify_consented().unwrap();
        std::fs::write(
            install.layout().weights.join("model.bin"),
            b"corrupted weight",
        )
        .unwrap();
        drop(install);
        assert!(
            matches!(gate.refresh(), LocalReadiness::Unavailable { error } if error.starts_with(LOCAL_UNAVAILABLE))
        );
        assert_eq!(gate.lifecycle.state(), WorkerState::Unavailable);
        assert_eq!(gate.launch_count, 1, "corruption cannot launch a fallback");
        assert!(gate.sentinel.local_path_clean());
    }

    #[test]
    fn consented_launch_reverifies_weight_after_selection() {
        let (_state, mut gate, receipt) = consented_gate();
        assert_eq!(gate.peek_selected_receipt(), Some(receipt.clone()));
        let install = gate.verify_consented().unwrap();
        std::fs::write(
            install.layout().weights.join("model.bin"),
            b"corrupted weight",
        )
        .unwrap();
        drop(install);
        assert!(!gate.install_selected_worker(&receipt));
        assert_eq!(gate.worker_failure.as_deref(), Some(LOCAL_UNAVAILABLE));
        assert_eq!(gate.launch_count, 0);
        assert!(gate.sentinel.local_path_clean());
    }

    #[test]
    fn consented_launch_refuses_replaced_receipt_after_selection() {
        let (_state, mut gate, receipt) = consented_gate();
        assert_eq!(gate.peek_selected_receipt(), Some(receipt.clone()));
        let (store, _) = gate.test_consented.as_ref().unwrap();
        let new = crate::local_model::consented::install_tests::replace_fixture_receipt(store);
        assert_ne!(new.receipt_hash, receipt.receipt_hash);
        assert!(!gate.install_selected_worker(&receipt));
        assert_eq!(gate.launch_count, 0);
        let selected = gate.peek_selected_receipt().unwrap();
        assert_eq!(selected.receipt_hash, new.receipt_hash);
        assert!(gate.install_selected_worker(&selected));
        assert_eq!(gate.launch_count, 1);
    }

    fn fixture_receipt() -> ActiveReceipt {
        crate::local_model::from_entry(crate::local_model::ci_fixture_entry(), "l4-artifact")
    }

    #[test]
    fn local_start_requires_receipt_and_ready_worker() {
        let _lock = test_session();
        match admit_local(true) {
            LocalAdmission::Unavailable { error } => {
                assert_eq!(error, LOCAL_UNAVAILABLE);
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
        assert_eq!(completion.local_asr.model_id, fixture_receipt().catalog_id);
        assert_eq!(
            completion.local_asr.outcome,
            voisu_core::LocalAsrOutcome::Transcript
        );
        assert_eq!(completion.local_asr.observed_device.as_deref(), Some("cpu"));
        authorize_and_confirm_delivery("rec-1", "hello", false).unwrap();
        assert!(sentinel_is_clean());
    }

    #[test]
    fn public_readiness_stays_unavailable_with_an_injected_test_worker() {
        let _lock = test_session();
        inject_ready(fixture_receipt(), "hello");
        assert_eq!(
            runtime_readiness(),
            LocalReadiness::Unavailable {
                error: LOCAL_UNAVAILABLE.to_owned()
            }
        );
        assert!(matches!(admit_local(true), LocalAdmission::Ready { .. }));
    }

    #[test]
    fn silence_outcome_keeps_prepared_device_and_rejects_unknown() {
        match silence_outcome("cuda:0").unwrap() {
            WorkerOutcome::NoText {
                reason,
                observed_device,
                ..
            } => {
                assert_eq!(reason, "silence");
                assert_eq!(observed_device, "cuda:0");
            }
            WorkerOutcome::Transcript { .. } => panic!("silence must stay NoText"),
        }
        match silence_outcome("cpu").unwrap() {
            WorkerOutcome::NoText {
                observed_device, ..
            } => assert_eq!(observed_device, "cpu"),
            WorkerOutcome::Transcript { .. } => panic!("silence must stay NoText"),
        }
        assert!(silence_outcome("unknown").is_err());
        assert!(silence_outcome("").is_err());
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
            local_asr: None,
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
    fn idle_switch_is_detected_only_when_a_previous_model_is_loaded() {
        assert!(!development_model_changed(
            None,
            None,
            "parakeet-tdt-0.6b-v3",
            "b".repeat(64).as_str()
        ));
        assert!(!development_model_changed(
            Some("parakeet-tdt-0.6b-v3"),
            Some("aa"),
            "parakeet-tdt-0.6b-v3",
            "aa"
        ));
        assert!(development_model_changed(
            Some("nemotron-3.5-0.6b"),
            Some("aa"),
            "parakeet-tdt-0.6b-v3",
            "aa"
        ));
        assert!(development_model_changed(
            Some("parakeet-tdt-0.6b-v3"),
            Some("aa"),
            "parakeet-tdt-0.6b-v3",
            "bb"
        ));
    }

    fn parakeet_receipt() -> ActiveReceipt {
        let mut receipt = fixture_receipt();
        receipt.catalog_id = "parakeet-tdt-0.6b-v3".into();
        receipt.artifact_id = "parakeet-artifact".into();
        receipt
    }

    #[test]
    fn failed_idle_switch_keeps_the_previous_receipt_and_does_not_spawn() {
        let _lock = test_session();
        inject_ready(fixture_receipt(), "nemotron");
        {
            let mut gate = gate().lock().expect("gate");
            if let Some(child) = gate.lifecycle.child_mut() {
                child.ignore_cancel = true;
                child.reap = crate::local_worker::ReapOutcome::Unreaped;
            }
            let _ = gate.lifecycle.cancel();
            gate.test_selected = Some(parakeet_receipt());
            gate.test_next_worker = Some(FakeWorker {
                scripted_text: Some("parakeet".into()),
                model_receipt_hash: parakeet_receipt().receipt_hash,
                ..FakeWorker::default()
            });
        }
        match admit_local(true) {
            LocalAdmission::Unavailable { .. } => {}
            LocalAdmission::Ready { .. } => panic!("unreaped switch must not admit"),
        }
        {
            let held = gate().lock().expect("gate");
            assert_eq!(held.launch_count, 0);
            assert_eq!(
                held.receipt
                    .as_ref()
                    .map(|receipt| receipt.catalog_id.as_str()),
                Some("l3-health-fixture")
            );
            assert!(held.lifecycle.has_child());
            assert!(!held.lifecycle.can_launch_replacement());
        }
        let _ = admit_local(true);
        let held = gate().lock().expect("gate");
        assert_eq!(held.launch_count, 0, "later polls must not spawn Parakeet");
        assert!(held.lifecycle.has_child());
    }

    #[test]
    fn failed_prepare_does_not_replace_receipt_or_spawn_a_second_model() {
        let _lock = test_session();
        inject_ready(fixture_receipt(), "nemotron");
        let parakeet = parakeet_receipt();
        {
            let mut gate = gate().lock().expect("gate");
            gate.test_selected = Some(parakeet.clone());
            gate.test_next_worker = Some(FakeWorker {
                scripted_error: Some("weird-native-code".into()),
                model_receipt_hash: parakeet.receipt_hash.clone(),
                ..FakeWorker::default()
            });
        }
        match admit_local(true) {
            LocalAdmission::Unavailable { error } => assert_eq!(
                error,
                format!("{LOCAL_NOT_READY}: worker refused: weird-native-code")
            ),
            LocalAdmission::Ready { .. } => panic!("failed prepare must not admit"),
        }
        {
            let mut gate = gate().lock().expect("gate");
            assert_eq!(gate.launch_count, 1);
            assert_eq!(
                gate.receipt
                    .as_ref()
                    .map(|receipt| receipt.catalog_id.as_str()),
                Some("l3-health-fixture")
            );
            assert!(gate.lifecycle.has_child());
            gate.test_next_worker = Some(FakeWorker {
                scripted_text: Some("second-parakeet".into()),
                model_receipt_hash: parakeet.receipt_hash,
                ..FakeWorker::default()
            });
        }
        match admit_local(true) {
            LocalAdmission::Unavailable { error } => assert!(
                error.ends_with("worker refused: weird-native-code"),
                "the held worker's refusal stays visible: {error}"
            ),
            LocalAdmission::Ready { .. } => panic!("held failed worker must not admit"),
        }
        let gate = gate().lock().expect("gate");
        assert_eq!(
            gate.launch_count, 1,
            "held child must block a second launch"
        );
        assert_eq!(
            gate.receipt
                .as_ref()
                .map(|receipt| receipt.catalog_id.as_str()),
            Some("l3-health-fixture")
        );
        assert!(gate.lifecycle.has_child());
    }

    #[test]
    fn successful_idle_switch_replaces_receipt_only_after_ready() {
        let _lock = test_session();
        inject_ready(fixture_receipt(), "nemotron");
        let parakeet = parakeet_receipt();
        {
            let mut gate = gate().lock().expect("gate");
            gate.test_selected = Some(parakeet.clone());
            gate.test_next_worker = Some(FakeWorker {
                scripted_text: Some("parakeet".into()),
                model_receipt_hash: parakeet.receipt_hash.clone(),
                observed_device: "cpu".into(),
                ..FakeWorker::default()
            });
        }
        match admit_local(true) {
            LocalAdmission::Ready {
                model_identity,
                receipt,
            } => {
                assert_eq!(model_identity, "parakeet-tdt-0.6b-v3");
                assert_eq!(receipt.catalog_id, "parakeet-tdt-0.6b-v3");
            }
            other => panic!("expected Ready after switch, got {other:?}"),
        }
        let gate = gate().lock().expect("gate");
        assert_eq!(gate.launch_count, 1);
        assert_eq!(
            gate.receipt
                .as_ref()
                .map(|receipt| receipt.catalog_id.as_str()),
            Some("parakeet-tdt-0.6b-v3")
        );
        assert_eq!(gate.lifecycle.state(), WorkerState::Ready);
        assert!(gate.lifecycle.has_child());
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
